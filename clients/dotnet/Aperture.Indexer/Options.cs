namespace Aperture.Indexer;

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

    public string Socket { get; init; } = "/tmp/aperture.sock";

    public string Database { get; init; } = "code";

    /// <summary>Index and encode, but connect to nothing — what the volume would be.</summary>
    public bool DryRun { get; init; }

    /// <summary>Also write every block to this file, which is the fact-file format.</summary>
    public string? Emit { get; init; }

    /// <summary>Emit <c>src.Ref</c> and <c>src.Import</c>. Off is a decls-only index.</summary>
    public bool References { get; init; } = true;

    /// <summary>Facts per block. A block is one <c>CopyData</c> frame and one interning batch.</summary>
    public int Batch { get; init; } = 4096;

    /// <summary>Stop after this many source files. 0 means all of them.</summary>
    public int MaxFiles { get; init; }

    /// <summary>Stop after this many projects. 0 means all of them.</summary>
    public int MaxProjects { get; init; }

    /// <summary>
    /// How much of the machine to use: design-time builds at once — each its own
    /// process — and files walked at once inside this one.
    /// </summary>
    public int Jobs { get; init; } = Math.Min(4, Environment.ProcessorCount);

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
        aperture-indexer — index a .NET solution into an Aperture database

          --source <path>       a .sln, .slnx, .csproj, or a directory holding one (required)
          --root <path>         paths are reported relative to this (default: the solution's directory)
          --socket <path>       the server's socket (default: /tmp/aperture.sock)
          --database <name>     the database to write to (default: code)
          --batch <n>           facts per block (default: 4096)
          --max-files <n>       stop after n source files
          --max-projects <n>    stop after n projects
          --jobs <n>            builds, and files walked, at once (default: 4, or fewer cores)
          --no-refs             declarations only: no src.Ref, no src.Import
          --no-restore          do not let the design-time build restore first
          --syntax-only         skip MSBuild; glob *.cs and parse them
          --dry-run             index and encode, but connect to nothing
          --emit <path>         also write every block to a file
          --no-smoke            do not query the index afterwards
          --verbose             let MSBuild's output through
          --help
        """;

    /// <summary>Parse <paramref name="argv"/>, or say what is wrong with it.</summary>
    public static bool TryParse(string[] argv, out Options options, out string? error)
    {
        options = null!;
        error = null;

        string? source = null, root = null, emit = null;
        var socket = "/tmp/aperture.sock";
        var database = "code";
        int batch = 4096, maxFiles = 0, maxProjects = 0;
        var jobs = Math.Min(4, Environment.ProcessorCount);
        bool references = true, restore = true, syntaxOnly = false;
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
                    case "--socket": socket = Value(); break;
                    case "--database": database = Value(); break;
                    case "--emit": emit = Value(); break;
                    case "--batch": batch = Number(); break;
                    case "--max-files": maxFiles = Number(); break;
                    case "--max-projects": maxProjects = Number(); break;
                    case "--jobs": jobs = Math.Max(1, Number()); break;
                    case "--no-refs": references = false; break;
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

        options = new Options
        {
            Source = Path.GetFullPath(source),
            Root = root is null ? null : Path.GetFullPath(root),
            Socket = socket,
            Database = database,
            Emit = emit is null ? null : Path.GetFullPath(emit),
            Batch = batch,
            MaxFiles = maxFiles,
            MaxProjects = maxProjects,
            Jobs = jobs,
            References = references,
            Restore = restore,
            SyntaxOnly = syntaxOnly,
            DryRun = dryRun,
            Smoke = smoke && !dryRun,
            Verbose = verbose,
        };

        return true;
    }
}
