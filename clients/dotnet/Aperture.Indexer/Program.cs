using System.Diagnostics;
using System.Globalization;

using Aperture.Client;

namespace Aperture.Indexer;

/// <summary>
/// A real indexer for a real language, pointed at an Aperture database.
/// </summary>
/// <remarks>
/// <para>
/// <c>Aperture.Demo</c> shows that the protocol works by writing six declarations
/// somebody typed out. This writes however many a checkout of .NET source contains,
/// which is the other thing a database needs to be shown: that it holds up when the
/// facts are not chosen to be convenient.
/// </para>
/// <para>
/// The shape of the run is deliberately the same as the demo's, because the demo's
/// shape is the point — <b>a producer that holds no fact ids</b>. Roslyn hands this
/// program a symbol; it turns the symbol into the declaration fact that names it and
/// nests that whole fact wherever a reference to it goes. It keeps no map from entities
/// to identities and it emits in whatever order the walk reaches things. At a million
/// facts that stops being an elegance argument and starts being the only tractable
/// option: the alternative is a second pass over an index that no longer fits in
/// memory.
/// </para>
/// </remarks>
internal static class Program
{
    public static int Main(string[] argv)
    {
        if (!Options.TryParse(argv, out var options, out var error))
        {
            Console.Error.WriteLine(error);
            return ReferenceEquals(error, Options.Usage) ? 0 : 2;
        }

        var root = options.Root
            ?? (Directory.Exists(options.Source) ? options.Source : Path.GetDirectoryName(options.Source)!);

        Console.WriteLine($"indexing {options.Source}");
        Console.WriteLine($"  paths relative to {root}");
        Console.WriteLine($"  schema fingerprint {CodeIndex.Schema.Fingerprint:x16}");

        var loading = Stopwatch.StartNew();
        LoadedSolution solution;

        try
        {
            solution = Loader.Load(options, root, Console.Out);
        }
        catch (Exception failure) when (failure is IOException or InvalidOperationException or ArgumentException)
        {
            Console.Error.WriteLine($"could not load {options.Source}: {failure.Message}");
            return 1;
        }

        loading.Stop();
        Console.WriteLine($"  {solution.Projects.Count} project(s) to walk, "
            + $"loaded in {loading.Elapsed.TotalSeconds:F1}s");
        Console.WriteLine();

        var connections = Connect(options);
        using var closing = new Closing(connections);
        var connection = connections.Count > 0 ? connections[0] : null;

        if (connection is not null)
        {
            Console.WriteLine($"  connected: protocol {connection.Hello.Version}, "
                + $"{connection.Hello.Predicates} predicates, schema {connection.Hello.SchemaFingerprint:x16}");
            Console.WriteLine();
        }

        var walking = Stopwatch.StartNew();
        int files;
        Indexer indexer;

        using (var sink = new FactSink(options, connections))
        {
            indexer = new Indexer(options, sink, root, solution.Build);
            var reported = TimeSpan.Zero;

            // The build layer first, and whole: this is what the repository *is*, not
            // what the walk reached, so a run stopped early by `--max-files` still says
            // which projects exist and what they depend on.
            solution.Build.Emit(sink);

            foreach (var project in solution.Projects)
            {
                if (indexer.Exhausted)
                {
                    Console.WriteLine($"stopping at {options.MaxFiles} files (--max-files)");
                    break;
                }

                // Compiled here rather than up front, so one project's symbols are
                // reachable only while that project is being walked.
                if (project.Compile() is not { } compilation)
                {
                    Console.WriteLine($"  ! {project.Name}: no compilation, skipping it");
                    continue;
                }

                indexer.Index(compilation, _ =>
                {
                    // Every couple of seconds, not every file: a hundred thousand
                    // progress lines is not progress.
                    if (walking.Elapsed - reported < TimeSpan.FromSeconds(2))
                    {
                        return;
                    }

                    reported = walking.Elapsed;
                    var rate = sink.Total / Math.Max(walking.Elapsed.TotalSeconds, 0.001);

                    Console.WriteLine(
                        $"  {indexer.Files,7} files  {Count(sink.Total),12} facts  "
                        + $"{Count((long)rate),9} facts/s  {project.Name}");
                });
            }

            // Drain rather than flush: the writer thread is still draining what
            // FlushAll queues, and every count below — and the elapsed time they are
            // divided by — is only final once it has stopped.
            sink.Drain();
            walking.Stop();
            files = indexer.Files;

            Report(options, sink, indexer, walking.Elapsed);
        }

        if (connection is not null && options.Smoke && files > 0)
        {
            Smoke(connection, indexer);
        }

        return 0;
    }

    /// <summary>Closes every connection when the run ends, however it ends.</summary>
    /// <remarks>
    /// A list is not <see cref="IDisposable"/>, and a run that threw halfway would
    /// otherwise leave sockets open until the process exited — which is tidy enough for
    /// a tool and untidy for a server counting connections.
    /// </remarks>
    private sealed class Closing(IReadOnlyList<ApertureConnection> connections) : IDisposable
    {
        public void Dispose()
        {
            foreach (var connection in connections)
            {
                connection.Dispose();
            }
        }
    }

    /// <summary>One connection per writer thread.</summary>
    /// <remarks>
    /// <para>
    /// <b>Connections rather than streams, because the client cannot multiplex.</b>
    /// <see cref="ApertureConnection"/> issues streams sequentially over one socket, so
    /// two concurrent write streams need two sockets. The server does not mind: it
    /// excludes writers per key rather than per database.
    /// </para>
    /// <para>
    /// <b>One writer when emitting.</b> <c>--emit</c> writes every block to a file, and
    /// that file is a checked-in golden — several writers would interleave into it and
    /// make its contents depend on scheduling. Anything that has to be reproducible byte
    /// for byte gets one writer, whatever <c>--writers</c> says.
    /// </para>
    /// </remarks>
    private static List<ApertureConnection> Connect(Options options)
    {
        if (options.DryRun)
        {
            Console.WriteLine("  --dry-run: encoding the facts and connecting to nothing");
            Console.WriteLine();
            return [];
        }

        var writers = options.Emit is null ? options.Writers : 1;
        if (options.Emit is not null && options.Writers > 1)
        {
            Console.WriteLine("  --emit: one writer, so the file is a deterministic run of blocks");
        }

        Console.WriteLine($"connecting to {options.Socket} ({options.Database}), {writers} writer(s)");

        // A claim, not a question: an indexer that disagrees with the server about the
        // schema is refused at the handshake rather than after an hour of writing facts
        // nobody can read back.
        var connections = new List<ApertureConnection>(writers);
        for (var n = 0; n < writers; n++)
        {
            connections.Add(ApertureConnection.Connect(
                options.Socket,
                options.Database,
                CodeIndex.Schema,
                SessionMode.ReadWrite,
                assertSchema: true));
        }

        return connections;
    }

    private static void Report(Options options, FactSink sink, Indexer indexer, TimeSpan elapsed)
    {
        Console.WriteLine();
        Console.WriteLine($"indexed {Count(indexer.Files)} file(s) in {elapsed.TotalSeconds:F1}s");

        foreach (var predicate in CodeIndex.Predicates)
        {
            Console.WriteLine($"  {CodeIndex.NameOf(predicate),-20}{Count(sink.Facts[predicate]),14}");
        }

        Console.WriteLine($"  {"total",-20}{Count(sink.Total),14} facts in {Count(sink.Blocks)} blocks");

        if (sink.Bytes > 0)
        {
            Console.WriteLine($"  {"encoded",-20}{Megabytes(sink.Bytes),14} MB"
                + $"  ({(double)sink.Bytes / Math.Max(sink.Total, 1):F0} bytes/fact)");
        }

        if (!options.DryRun)
        {
            // Created counts every fact written, nested targets included; deduped those
            // already there. A million references naming ten thousand declarations is
            // supposed to show up here as a large dedup count — that is interning
            // working, and it is the number this whole exercise is a measurement of.
            Console.WriteLine($"  {"server",-20}{Count((long)sink.Created),14} created, "
                + $"{Count((long)sink.Deduped)} deduped");
            Console.WriteLine($"  {"writing",-20}{sink.Writing.TotalSeconds,14:F1}s"
                + $"  (summed over {sink.Writers} writer(s), overlapped — not wall clock)");
            Console.WriteLine($"  {"queueing",-20}{sink.Queueing.TotalSeconds,14:F1}s"
                + $"  (walk blocked on a full queue)");
        }

        {
            Console.WriteLine($"  {"gate wait",-20}{indexer.GateWait.TotalSeconds,14:F1}s"
                + $"  (walkers blocked on the gate)");
            Console.WriteLine($"  {"gate held",-20}{indexer.GateHeld.TotalSeconds,14:F1}s");
        }

        var rate = sink.Total / Math.Max(elapsed.TotalSeconds, 0.001);
        Console.WriteLine($"  {"throughput",-20}{Count((long)rate),14} facts/s");

        Console.WriteLine();
        Console.WriteLine($"references: {Count(indexer.References)} resolved, "
            + $"{Count(indexer.External)} to declarations outside the index, "
            + $"{Count(indexer.Unresolved)} unresolved");

        if (indexer.Unattributed > 0)
        {
            // Shared source, or a checkout with no project files under `--source`. Said
            // out loud because a silent zero for `src.ProjectSource` looks like a bug in
            // the schema rather than a fact about the repository.
            Console.WriteLine($"  {Count(indexer.Unattributed)} file(s) no project compiles "
                + "(shared source, or outside every project directory)");
        }

        if (indexer.Conflicts > 0)
        {
            Console.WriteLine($"  {Count(indexer.Conflicts)} declaration key(s) reached with two kinds; "
                + "the first won (see Indexer._kinds)");
        }
    }

    /// <summary>
    /// Ask the database what it just took, on the same connection.
    /// </summary>
    /// <remarks>
    /// Three questions, chosen for what they cost rather than for what they mean: a scan
    /// of a small predicate, a seek into the search index, and the join that reaches
    /// through a reference. The last one is the interesting number — <c>src.Ref</c>'s
    /// key begins with a position, so finding every use of a declaration reads the
    /// predicate rather than narrowing into it, which is exactly the shape of argument
    /// <c>src.SearchByName</c> exists to answer at the declaration level.
    /// </remarks>
    private static void Smoke(ApertureConnection connection, Indexer indexer)
    {
        var sample = indexer.SampleName;

        Console.WriteLine();
        Console.WriteLine("querying it back");

        Run("every namespace, which is a scan", "N where src.Module {name = N}");

        Run("every assembly the repository builds, which is the build layer",
            "A where src.Assembly A");

        if (sample is not null)
        {
            Run($"declarations named `{sample}`, which is a seek",
                $"{{kind = D.value, line = D.line, name = D.name}} "
                + $"where src.SearchByName {{name = \"{sample}\", to = D}}");

            Run($"uses of `{sample}`, which is a join",
                $"{{line = R.at.line, col = R.at.col}} where R = src.Ref {{to = D}}; "
                + $"src.SearchByName {{name = \"{sample}\", to = D}}");

            // Into the declaration graph and out the other side: a name, the
            // declaration it reaches, and a seek into a predicate keyed by that
            // declaration. `src.Param`'s key is (decl, index, name), so this is one seek
            // and the parameters come back in order.
            if (indexer.SampleMethod is { } method)
            {
                Run($"the parameters of `{method}`, which is a seek keyed by a declaration",
                    $"{{at = P.index, name = P.name, type = P.value}} "
                    + $"where src.SearchByName {{name = \"{method}\", to = D}}; "
                    + $"P = src.Param {{decl = D}}");
            }

            // Two references followed — declaration to module to file — and the result
            // used as the key of a third predicate. No string is compared: the file is
            // an id by the time `src.Line` is seeked.
            //
            // **The conjuncts are in this order because the field access needs it.**
            // `reorder` is free to run them either way round, but `D.module.file` is
            // typechecked where it is written, and a variable no earlier conjunct has
            // bound has no type there to take a field of.
            Run($"the source of the file declaring `{sample}`, which is a fetch then a seek",
                $"{{line = L.line, text = L.value}} "
                + $"where src.SearchByName {{name = \"{sample}\", to = D}}; "
                + $"L = src.Line {{file = D.module.file}}");
        }

        void Run(string what, string focus)
        {
            Console.WriteLine();
            Console.WriteLine($"  {what}");
            Console.WriteLine($"  focus> {focus}");

            var started = Stopwatch.StartNew();

            try
            {
                var result = connection.Query(focus);
                started.Stop();

                foreach (var row in result.Rows.Take(5))
                {
                    Console.WriteLine($"    {Render(row, result.Shape)}");
                }

                if (result.Rows.Count > 5)
                {
                    Console.WriteLine($"    ... and {Count(result.Rows.Count - 5)} more");
                }

                Console.WriteLine($"    {Count(result.Rows.Count)} row(s) in {started.Elapsed.TotalSeconds:F2}s");
            }
            catch (ApertureServerException failure)
            {
                Console.WriteLine($"    refused ({failure.Code}): {failure.ServerMessage.Split('\n')[0]}");
            }
        }
    }

    /// <summary>
    /// A row, named by the descriptor it came with.
    /// </summary>
    /// <remarks>
    /// A record is positional on the wire — the names are in the row descriptor the
    /// server sent once, at the head of the stream. Printing a row without them says
    /// <c>{5, 283}</c> for a line and a column and leaves a reader to guess which is
    /// which, and the two orders are both plausible.
    /// </remarks>
    private static string Render(ApertureValue value, ApertureType type) => (value, type) switch
    {
        (ApertureValue.Int number, _) => number.Value.ToString(CultureInfo.InvariantCulture),
        (ApertureValue.Str text, _) => $"\"{text.Value}\"",
        (ApertureValue.Ref { Value: ApertureRef.Id id }, _) => $"#{id.FactId >> 40}:{id.FactId & 0xFFFFFFFFFF}",

        (ApertureValue.Record record, ApertureType.Record shape)
            when record.Fields.Count == shape.Fields.Count =>
            "{" + string.Join(", ", record.Fields.Select((field, index) =>
                $"{shape.Fields[index].Name} = {Render(field, shape.Fields[index].Type)}")) + "}",

        _ => "?",
    };

    private static string Count(long value) => value.ToString("N0", CultureInfo.InvariantCulture);

    private static string Megabytes(long bytes) =>
        (bytes / (1024.0 * 1024.0)).ToString("N1", CultureInfo.InvariantCulture);
}
