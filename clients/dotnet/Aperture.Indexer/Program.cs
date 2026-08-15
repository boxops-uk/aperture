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
        Console.WriteLine($"  schema fingerprint {CodeIndex.Schema.Fingerprint():x16}");

        var loading = Stopwatch.StartNew();
        IReadOnlyList<LoadedProject> projects;

        try
        {
            projects = Loader.Load(options, Console.Out);
        }
        catch (Exception failure) when (failure is IOException or InvalidOperationException)
        {
            Console.Error.WriteLine($"could not load {options.Source}: {failure.Message}");
            return 1;
        }

        loading.Stop();
        Console.WriteLine($"  {projects.Count} compilation(s) in {loading.Elapsed.TotalSeconds:F1}s");
        Console.WriteLine();

        using var connection = Connect(options);

        if (connection is not null)
        {
            Console.WriteLine($"  connected: protocol {connection.Hello.Version}, "
                + $"{connection.Hello.Predicates} predicates, schema {connection.Hello.SchemaFingerprint:x16}");
            Console.WriteLine();
        }

        var walking = Stopwatch.StartNew();
        int files;
        Indexer indexer;

        using (var sink = new FactSink(options, connection))
        {
            indexer = new Indexer(options, sink, root);
            var reported = TimeSpan.Zero;

            foreach (var project in projects)
            {
                if (indexer.Exhausted)
                {
                    Console.WriteLine($"stopping at {options.MaxFiles} files (--max-files)");
                    break;
                }

                indexer.Index(project, _ =>
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

            // Disposing the sink flushes what is left, so the counts below are final.
            sink.FlushAll();
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

    private static ApertureConnection? Connect(Options options)
    {
        if (options.DryRun)
        {
            Console.WriteLine("  --dry-run: encoding the facts and connecting to nothing");
            Console.WriteLine();
            return null;
        }

        Console.WriteLine($"connecting to {options.Socket} ({options.Database})");

        // A claim, not a question: an indexer that disagrees with the server about the
        // schema is refused at the handshake rather than after an hour of writing facts
        // nobody can read back.
        return ApertureConnection.Connect(
            options.Socket,
            options.Database,
            CodeIndex.Schema,
            SessionMode.ReadWrite,
            assertSchema: true);
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
            Console.WriteLine($"  {"writing",-20}{sink.Writing.TotalSeconds,14:F1}s of {elapsed.TotalSeconds:F1}s");
        }

        var rate = sink.Total / Math.Max(elapsed.TotalSeconds, 0.001);
        Console.WriteLine($"  {"throughput",-20}{Count((long)rate),14} facts/s");

        Console.WriteLine();
        Console.WriteLine($"references: {Count(indexer.References)} resolved, "
            + $"{Count(indexer.External)} to declarations outside the index, "
            + $"{Count(indexer.Unresolved)} unresolved");

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

        if (sample is not null)
        {
            Run($"declarations named `{sample}`, which is a seek",
                $"{{kind = D.value, line = D.line, name = D.name}} "
                + $"where src.SearchByName {{name = \"{sample}\", to = D}}");

            Run($"uses of `{sample}`, which is a join",
                $"{{line = R.at.line, col = R.at.col}} where R = src.Ref {{to = D}}; "
                + $"src.SearchByName {{name = \"{sample}\", to = D}}");
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
                    Console.WriteLine($"    {Render(row)}");
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

    private static string Render(ApertureValue value) => value switch
    {
        ApertureValue.Int number => number.Value.ToString(CultureInfo.InvariantCulture),
        ApertureValue.Str text => $"\"{text.Value}\"",
        ApertureValue.Ref { Value: ApertureRef.Id id } => $"#{id.FactId >> 40}:{id.FactId & 0xFFFFFFFFFF}",
        ApertureValue.Ref { Value: ApertureRef.Nested nested } => $"<{Render(nested.Fact.Key)}>",
        ApertureValue.Record record => "{" + string.Join(", ", record.Fields.Select(Render)) + "}",
        _ => "?",
    };

    private static string Count(long value) => value.ToString("N0", CultureInfo.InvariantCulture);

    private static string Megabytes(long bytes) =>
        (bytes / (1024.0 * 1024.0)).ToString("N1", CultureInfo.InvariantCulture);
}
