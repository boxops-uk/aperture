using Fjord.Client;

namespace Fjord.Indexer;

/// <summary>What to index, where the facts go, and how much of it to do.</summary>
/// <remarks>
/// Hand-rolled rather than a command-line library: this program's dependencies are
/// MSBuild and Roslyn, both of which are large, and adding a third to read six flags
/// would be the wrong trade.
/// </remarks>
internal sealed record Options
{
    /// <summary>A <c>.sln</c>, <c>.slnx</c>, <c>.csproj</c>, or a directory holding one.</summary>
    public required string Source { get; init; }

    /// <summary>
    /// The directory paths are reported relative to. Defaults to the solution's own
    /// directory, so a file fact reads <c>src/Foo/Bar.cs</c> rather than naming whoever
    /// happened to check the repository out.
    /// </summary>
    public string? Root { get; init; }

    /// <summary>
    /// The <c>dotnet</c> host that runs the design-time builds.
    /// </summary>
    /// <remarks>
    /// Defaults to <c>&lt;root&gt;/.dotnet/dotnet</c> when the checkout has one, which
    /// is where a repository pinning an SDK in <c>global.json</c> bootstraps it — the
    /// arcade convention, and dotnet/runtime's. Null means whichever <c>dotnet</c>
    /// Buildalyzer finds, which is right for a repository that pins nothing.
    /// </remarks>
    public string? Dotnet { get; init; }

    /// <summary>
    /// Where to write, and which database: <c>[where//]name[@instance]</c>.
    /// </summary>
    /// <remarks>
    /// The same address grammar the `fjord` CLI takes, so a producer is pointed at a
    /// server the way everything else is. An address naming no target means the socket
    /// in <see cref="DefaultSocket"/>, since this producer has no configuration layer to
    /// consult.
    /// </remarks>
    public FjordAddress Address { get; init; } =
        FjordAddress.ForSocket(DefaultSocket, "code");

    /// <summary>Where a target-less address goes.</summary>
    public const string DefaultSocket = "/tmp/fjord.sock";

    /// <summary>Index and encode, but connect to nothing — what the volume would be.</summary>
    public bool DryRun { get; init; }

    /// <summary>Also write every block to this file, which is the fact-file format.</summary>
    public string? Emit { get; init; }

    /// <summary>
    /// Write the same facts into this directory as Glean JSON batches, instead of into an
    /// Fjord database.
    /// </summary>
    /// <remarks>
    /// <para>
    /// One walk, two sinks — see <see cref="GleanFacts"/> for what the translation is and
    /// why it is JSON. The walk, the batching and the writer threads are unchanged, which
    /// is the point: what differs between the two runs is the database, not the producer.
    /// </para>
    /// <para>
    /// <b>This is half of a measurement.</b> A file of JSON is a fact nobody has interned
    /// yet; the load is a second step with its own clock
    /// (<c>clients/dotnet/index-repo-glean.sh</c>). Nothing here connects to a Fjord
    /// server, so there is nothing to query afterwards and no <c>created</c>/<c>deduped</c>
    /// to report.
    /// </para>
    /// </remarks>
    public string? GleanOut { get; init; }

    /// <summary>Emit <c>src.Ref</c> and <c>src.Import</c>. Off is a decls-only index.</summary>
    public bool References { get; init; } = true;

    /// <summary>
    /// Emit <c>src.Line</c>: the file's line table, one fact per line of source.
    /// </summary>
    /// <remarks>
    /// The largest predicate in the index by bytes and the second largest by count, and
    /// the only one whose facts are not about a symbol — so it is the one worth being
    /// able to leave out when what is being measured is the semantic index.
    /// </remarks>
    public bool Lines { get; init; } = true;

    /// <summary>Emit <c>src.Doc</c>: the doc comment above a declaration.</summary>
    public bool Docs { get; init; } = true;

    /// <summary>Facts per block. A block is one <c>CopyData</c> frame and one interning batch.</summary>
    public int Batch { get; init; } = 4096;

    /// <summary>Stop after this many source files. 0 means all of them.</summary>
    public int MaxFiles { get; init; }

    /// <summary>
    /// Paths, relative to <see cref="Root"/>, whose files are not walked.
    /// </summary>
    /// <remarks>
    /// <para>
    /// <b>A corpus decision, made explicit.</b> `bench/FINDINGS.md` §15b found that 45% of
    /// the wall clock of a whole-`dotnet/runtime` index was one 24 MB generated file in
    /// `src/tests` whose reference pass resolves nothing — so every facts/s figure ever
    /// quoted for that corpus was measuring one pathological file as much as the database.
    /// This is how that file leaves the corpus without leaving a subset of it to a script.
    /// </para>
    /// <para>
    /// <b>A path prefix, matched relative to the root</b> — <c>--exclude src/tests</c> —
    /// rather than a glob, because the thing being excluded is a *tree* and a prefix says
    /// so without a pattern language. Repeat the flag for more than one.
    /// </para>
    /// <para>
    /// <b>The build layer is still whole</b>, which is the same decision <c>--max-files</c>
    /// already made: what projects a repository has and what they reference is a fact about
    /// the repository, not about which files this run reached. So an excluded tree's
    /// projects still appear, and `src.ProjectSource` still names its files — which
    /// interning creates as `src.File` facts with nothing else said about them.
    /// </para>
    /// </remarks>
    public string[] Excludes { get; init; } = [];

    /// <summary>
    /// Skip this many source files before indexing, in path order — <c>--syntax-only</c>
    /// only.
    /// </summary>
    /// <remarks>
    /// <b>What makes a checkout too big for one compilation indexable anyway.</b> A
    /// syntax-only run holds every tree of its <c>--source</c> at once, which for
    /// dotnet/runtime is more memory than most machines have. With this, the same source
    /// root is indexed in slices — <c>--skip-files 0 --max-files 4000</c>, then 4000,
    /// then 8000 — each run costing only what its slice holds, and the facts accumulating
    /// in one database because interning does not care which run wrote a target first.
    /// <para>
    /// The cost is real and worth stating: a reference from a slice to a declaration in
    /// another slice binds against the framework's metadata rather than against source,
    /// so it is dropped as external. Slices bounded at a library keep nearly all of it;
    /// slices that cut one in half do not.
    /// </para>
    /// </remarks>
    public int SkipFiles { get; init; }

    /// <summary>Stop after this many projects. 0 means all of them.</summary>
    public int MaxProjects { get; init; }

    /// <summary>
    /// How much of the machine to use: design-time builds at once — each its own
    /// process — and files walked at once inside this one.
    /// </summary>
    public int Jobs { get; init; } = Math.Min(4, Environment.ProcessorCount);

    /// <summary>
    /// Write streams to the server, each on its own connection.
    /// </summary>
    /// <remarks>
    /// <para>
    /// A database takes as many writers as there are streams — it excludes them per key
    /// rather than per database — so this is how much of that the indexer asks for.
    /// </para>
    /// <para>
    /// <b>One by default, because that is what has been measured.</b> This was set to
    /// follow <see cref="Jobs"/> on the reasoning that the two sides of a run should be
    /// sized alike, and a 16-file corpus said otherwise: four writers cost ~10%
    /// throughput and moved nothing, because <c>queueing</c> was already near zero — the
    /// writer was not the ceiling, so more of them could only add connections and
    /// handshakes. Most of that is fixed cost and would vanish at scale, but "would" is
    /// not a measurement, and a default should not be an argument.
    /// </para>
    /// <para>
    /// <b>The number that says to raise it is <c>queueing</c></b>: time the walk spent
    /// blocked on a full queue. While it is near zero the writers are keeping up and this
    /// should stay at one. When it is a real share of the run, raise it — that is the
    /// case this exists for, and the one nobody has measured yet, because measuring it
    /// costs a re-index of something the size of <c>dotnet/runtime</c>.
    /// </para>
    /// </remarks>
    public int Writers { get; init; } = 1;

    /// <summary>Let the design-time build restore first. Off is much faster when it is already restored.</summary>
    public bool Restore { get; init; } = true;

    /// <summary>
    /// Skip MSBuild entirely: glob <c>*.cs</c> and compile them against the running
    /// framework's reference set. Fast, and resolves less — see the README.
    /// </summary>
    public bool SyntaxOnly { get; init; }

    /// <summary>Run a handful of queries against what was just written.</summary>
    public bool Smoke { get; init; } = true;

    /// <summary>Let MSBuild's own output through.</summary>
    public bool Verbose { get; init; }

    public const string Usage = """
        fjord-indexer — index a .NET solution into a Fjord database

          --source <path>       a .sln, .slnx, .csproj, or a directory holding one (required)
          --root <path>         paths are reported relative to this (default: the solution's directory)
          --dotnet <path>       the dotnet host to build with (default: <root>/.dotnet/dotnet if present)
          --at <address>        where to write: [where//]name[@instance]
                                (default: code, on /tmp/fjord.sock — a bare name means
                                that socket, `box:7280//code` means TCP, and
                                `/run/fjord.sock//code` names a socket)
          --batch <n>           facts per block (default: 4096)
          --max-files <n>       stop after n source files
          --exclude <path>      do not walk this tree, relative to --root (repeatable)
          --skip-files <n>      skip the first n files, in path order (--syntax-only)
          --max-projects <n>    stop after n projects
          --jobs <n>            builds, and files walked, at once (default: 4, or fewer cores)
          --writers <n>         concurrent write streams, one connection each (default: 1;
                                raise it when the report's `queueing` is a real share of the run)
          --no-refs             declarations only: no src.Ref, no src.Import
          --no-lines            do not write the line table (src.Line)
          --no-docs             do not write doc comments (src.Doc)
          --no-restore          do not let the design-time build restore first
          --syntax-only         skip MSBuild; glob *.cs and parse them
          --dry-run             index and encode, but connect to nothing
          --emit <path>         also write every block to a file
          --glean-out <dir>     write the facts as Glean JSON batches here, not to Fjord
                                (one file per block; load them with glean write — see
                                clients/dotnet/index-repo-glean.sh)
          --no-smoke            do not query the index afterwards
          --verbose             let MSBuild's output through
          --help
        """;

    /// <summary>The `where` half of an address, for the flags that set one half.</summary>
    private static string Where(string address)
    {
        var at = address.LastIndexOf(FjordAddress.Separator, StringComparison.Ordinal);
        return at < 0 ? "" : address[..at];
    }

    /// <summary>The database half, likewise.</summary>
    private static string Database(string address)
    {
        var at = address.LastIndexOf(FjordAddress.Separator, StringComparison.Ordinal);
        return at < 0 ? address : address[(at + FjordAddress.Separator.Length)..];
    }

    /// <summary>Parse <paramref name="argv"/>, or say what is wrong with it.</summary>
    public static bool TryParse(string[] argv, out Options options, out string? error)
    {
        options = null!;
        error = null;

        string? source = null, root = null, emit = null, dotnet = null, gleanOut = null;
        var at = $"{DefaultSocket}{FjordAddress.Separator}code";
        int batch = 4096, maxFiles = 0, maxProjects = 0, skipFiles = 0;
        var excludes = new List<string>();
        var jobs = Math.Min(4, Environment.ProcessorCount);
        int? writers = null;
        bool references = true, restore = true, syntaxOnly = false;
        bool lines = true, docs = true;
        bool dryRun = false, smoke = true, verbose = false;

        for (var index = 0; index < argv.Length; index++)
        {
            var flag = argv[index];

            // A flag that takes a value, and the value that is not there.
            string Value()
            {
                if (index + 1 >= argv.Length)
                {
                    throw new FormatException($"`{flag}` wants a value");
                }

                return argv[++index];
            }

            int Number()
            {
                var text = Value();
                return int.TryParse(text, out var number) && number >= 0
                    ? number
                    : throw new FormatException($"`{flag}` wants a number, not `{text}`");
            }

            try
            {
                switch (flag)
                {
                    case "--source": source = Value(); break;
                    case "--root": root = Value(); break;
                    case "--dotnet": dotnet = Value(); break;
                    case "--at": at = Value(); break;

                    // The two flags this replaced, kept working so a script written
                    // against them still runs — they compose into one address.
                    case "--socket": at = $"{Value()}{FjordAddress.Separator}{Database(at)}"; break;
                    case "--database": at = $"{Where(at)}{FjordAddress.Separator}{Value()}"; break;
                    case "--emit": emit = Value(); break;
                    case "--glean-out": gleanOut = Value(); break;
                    case "--batch": batch = Number(); break;
                    case "--max-files": maxFiles = Number(); break;
                    case "--exclude": excludes.Add(Value().Replace('\\', '/').Trim('/')); break;
                    case "--skip-files": skipFiles = Number(); break;
                    case "--max-projects": maxProjects = Number(); break;
                    case "--jobs": jobs = Math.Max(1, Number()); break;
                    case "--writers": writers = Math.Max(1, Number()); break;
                    case "--no-refs": references = false; break;
                    case "--no-lines": lines = false; break;
                    case "--no-docs": docs = false; break;
                    case "--no-restore": restore = false; break;
                    case "--syntax-only": syntaxOnly = true; break;
                    case "--dry-run": dryRun = true; break;
                    case "--no-smoke": smoke = false; break;
                    case "--verbose": verbose = true; break;

                    case "--help" or "-h":
                        error = Usage;
                        return false;

                    default:
                        error = $"unknown flag `{flag}`\n\n{Usage}";
                        return false;
                }
            }
            catch (FormatException failure)
            {
                error = failure.Message;
                return false;
            }
        }

        if (source is null)
        {
            error = $"--source is required\n\n{Usage}";
            return false;
        }

        if (batch < 1)
        {
            error = "--batch must be at least 1";
            return false;
        }

        // Each of these names where the facts go, and they name different places. Taking
        // two would leave the run's timings covering a mixture of the two write paths,
        // which is the one thing a comparison harness must not do quietly.
        if (gleanOut is not null && (emit is not null || dryRun))
        {
            error = "--glean-out writes the facts somewhere else, so it cannot be combined "
                + "with --emit or --dry-run";
            return false;
        }

        FjordAddress address;
        try
        {
            address = FjordAddress.Parse(at).OrSocket(DefaultSocket);
        }
        catch (FormatException failure)
        {
            error = failure.Message;
            return false;
        }

        options = new Options
        {
            Address = address,
            Source = Path.GetFullPath(source),
            Root = root is null ? null : Path.GetFullPath(root),
            Dotnet = dotnet is null ? Bootstrapped(source, root) : Path.GetFullPath(dotnet),
            Emit = emit is null ? null : Path.GetFullPath(emit),
            GleanOut = gleanOut is null ? null : Path.GetFullPath(gleanOut),
            Batch = batch,
            MaxFiles = maxFiles,
            Excludes = [.. excludes],
            SkipFiles = skipFiles,
            MaxProjects = maxProjects,
            Jobs = jobs,
            Writers = writers ?? 1,
            References = references,
            Lines = lines,
            Docs = docs,
            Restore = restore,
            SyntaxOnly = syntaxOnly,
            DryRun = dryRun,
            Smoke = smoke && !dryRun && gleanOut is null,
            Verbose = verbose,
        };

        return true;
    }

    /// <summary>
    /// The <c>dotnet</c> a checkout bootstrapped for itself, if it did.
    /// </summary>
    /// <remarks>
    /// A repository pinning an SDK in <c>global.json</c> installs it into <c>.dotnet</c>
    /// at its root — arcade's convention, and what <c>eng/common/dotnet.sh</c> does.
    /// Finding it is worth more than a flag nobody remembers to pass: the failure it
    /// prevents is every design-time build failing at once, which reads as a repository
    /// that will not build rather than a host that cannot run its MSBuild.
    /// <para>
    /// <b>The <c>global.json</c> beside it is the whole test.</b> A bare <c>.dotnet</c>
    /// directory is not evidence of anything — <c>$HOME/.dotnet</c> is where a per-user
    /// install lands, so walking ancestors for the directory alone finds it for every
    /// checkout under a home directory and quietly builds against the wrong SDK. What
    /// is being looked for is a checkout that pinned one, and the pin is the file.
    /// </para>
    /// </remarks>
    private static string? Bootstrapped(string source, string? root)
    {
        var checkout = root ?? (Directory.Exists(source) ? source : Path.GetDirectoryName(source));

        while (checkout is not null)
        {
            var host = Path.Combine(checkout, ".dotnet", "dotnet");

            if (File.Exists(host) && File.Exists(Path.Combine(checkout, "global.json")))
            {
                return Path.GetFullPath(host);
            }

            checkout = Path.GetDirectoryName(checkout);
        }

        return null;
    }
}
