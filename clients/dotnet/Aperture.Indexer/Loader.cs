using Buildalyzer;
using Buildalyzer.Environment;
using Buildalyzer.IO;
using Buildalyzer.Workspaces;

using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.CSharp;

namespace Aperture.Indexer;

/// <summary>
/// A project, and the compilation of it — on demand.
/// </summary>
/// <remarks>
/// A function rather than a compilation, because asking for one materialises every
/// symbol table in it. Over four hundred projects that is the difference between a
/// machine indexing and a machine swapping; asked for one at a time, each is reachable
/// only while it is being walked.
/// </remarks>
internal sealed record LoadedProject(string Name, Func<Compilation?> Compile);

/// <summary>What there is to walk, and what compiled it.</summary>
/// <remarks>
/// The two travel together because they are answered by the same pass: the design-time
/// build that produces a compilation is also the only thing that knows the project's
/// resolved framework, its assembly name and its real source list.
/// </remarks>
internal sealed record LoadedSolution(IReadOnlyList<LoadedProject> Projects, ProjectIndex Build);

/// <summary>
/// Turning a checkout into compilations, which is the half of an indexer that is not
/// about facts at all.
/// </summary>
/// <remarks>
/// <para>
/// <b>MSBuild is asked, not guessed at.</b> Buildalyzer runs a design-time build of
/// each project in a separate <c>dotnet msbuild</c> process — the same evaluation an
/// IDE does, with compilation skipped — and hands back what the compiler would have
/// been given: the source list after globs and conditions, the reference assemblies
/// NuGet resolved, the preprocessor symbols, the language version. Reconstructing any
/// of that from the project XML is how an indexer ends up quietly indexing a different
/// program from the one that builds.
/// </para>
/// <para>
/// Those results become a Roslyn <see cref="AdhocWorkspace"/>, and project references
/// inside it are <i>project</i> references rather than paths to built assemblies — so a
/// symbol used in one project and declared in another still has a source location, and
/// the reference resolves to a declaration this index holds.
/// </para>
/// <para>
/// <b>The fallback is deliberate.</b> A real repository has projects that will not
/// restore on this machine — a Windows-only target, a pinned SDK, a missing feed. One
/// project failing must not cost the other four hundred, so a failure is reported and
/// skipped; if <i>every</i> project fails, the loader falls back to parsing the
/// <c>.cs</c> files it can find. That mode resolves less (see the README) and says so.
/// </para>
/// </remarks>
internal static class Loader
{
    public static LoadedSolution Load(Options options, string root, TextWriter log)
    {
        if (options.SyntaxOnly)
        {
            return Syntax(options, root, log);
        }

        var entry = ResolveEntryPoint(options.Source);
        log.WriteLine($"  entry point {entry}");

        var workspace = new AdhocWorkspace();
        var analyzers = Analyzers(entry, options, log);

        if (options.MaxProjects > 0 && analyzers.Count > options.MaxProjects)
        {
            log.WriteLine($"  stopping at {options.MaxProjects} projects (--max-projects)");
            analyzers = analyzers.Take(options.MaxProjects).ToList();
        }

        // Each design-time build is its own `dotnet msbuild` process, so several at once
        // is several processes and not several threads in this one. That is what makes
        // it worth doing: a few hundred projects at three seconds each is the difference
        // between a coffee and a lunch, and the results are independent.
        var results = new IAnalyzerResult?[analyzers.Count];

        Parallel.For(0, analyzers.Count, new ParallelOptions { MaxDegreeOfParallelism = options.Jobs }, index =>
        {
            results[index] = BuildOne(analyzers[index], options, log);
        });

        var built = 0;
        var failed = 0;

        // Added in the order the solution lists them rather than the order they
        // finished, so two runs over one checkout produce the same index.
        foreach (var result in results)
        {
            if (result is null)
            {
                failed++;
                continue;
            }

            // **It may already be here, and by its own doing.** The previous project's
            // add pulled its project references in with it, and one of those may be this
            // one — a test project sorts before the library it tests as often as not.
            // Adding it twice throws and takes the whole run with it.
            if (Holds(workspace, result.ProjectFilePath))
            {
                built++;
                continue;
            }

            try
            {
                // `addProjectReferences: true` pulls in whatever this project references
                // and is not already here, which is what makes a cross-project reference
                // resolve to source rather than to a metadata symbol with no location to
                // point at.
                result.AddToWorkspace(workspace, addProjectReferences: true);
                built++;
            }
            catch (ArgumentException)
            {
                // **The references are what failed, not this project.** `addProjectReferences`
                // walks to every project this one names, and one whose own design-time
                // build failed has no result to add. Dropping this project too would
                // spend a successful build on nothing; added alone, its own file's
                // declarations are still exact and only the symbols it reached *through*
                // that reference degrade to metadata.
                //
                // The add is not atomic, so ask before retrying: it walks references
                // depth-first and may well have added *this* project before reaching the
                // one it could not resolve, and adding it a second time throws again —
                // this time saying the solution already contains it, which would report
                // a project that is in the workspace as one that failed.
                if (Holds(workspace, result.ProjectFilePath))
                {
                    built++;
                    continue;
                }

                try
                {
                    result.AddToWorkspace(workspace, addProjectReferences: false);
                    built++;
                }
                catch (Exception alone) when (alone is InvalidOperationException or ArgumentException)
                {
                    log.WriteLine($"  ! {Path.GetFileName(result.ProjectFilePath)}: "
                        + $"the workspace refused it — {alone.Message}");
                    failed++;
                }
            }
            catch (InvalidOperationException refused)
            {
                // One project the workspace will not take is not worth the other four
                // hundred.
                log.WriteLine($"  ! {Path.GetFileName(result.ProjectFilePath)}: "
                    + $"the workspace refused it — {refused.Message}");
                failed++;
            }
        }

        if (built == 0)
        {
            log.WriteLine(failed == 0
                ? "  no projects found; falling back to parsing the .cs files under --source"
                : $"  every project failed ({failed}); falling back to parsing the .cs files under --source");

            return Syntax(options, root, log);
        }

        if (failed > 0)
        {
            log.WriteLine($"  {failed} project(s) skipped, {built} built");
        }

        // Ordered by path, not by whatever order the workspace hands them back: with
        // `--max-files` the order decides *which* files get indexed, and a run that
        // indexes a different two thousand each time is not a measurement.
        var walking = workspace.CurrentSolution.Projects
            .Where(project => project.Language == LanguageNames.CSharp)
            .OrderBy(project => project.FilePath ?? project.Name, StringComparer.Ordinal)
            .Select(project => new LoadedProject(
                project.Name,
                () => project.GetCompilationAsync().GetAwaiter().GetResult()))
            .ToList();

        // The build layer is built from *every* project file under the source, not only
        // the ones that built: a project MSBuild refused is still a project, its
        // references are still in its XML, and the files under it still have somewhere
        // to belong. The results that did succeed then overwrite what they know better.
        var build = ProjectIndex.Build(
            root,
            options.Source,
            results.Where(result => result is not null).Select(result => result!).ToList(),
            log);

        return new LoadedSolution(walking, build);
    }

    /// <summary>Whether the workspace already has the project at <paramref name="path"/>.</summary>
    private static bool Holds(Workspace workspace, string? path) =>
        path is not null
        && workspace.CurrentSolution.Projects.Any(project =>
            project.FilePath is { } held
            && string.Equals(Path.GetFullPath(held), Path.GetFullPath(path), StringComparison.Ordinal));

    /// <summary>One project's design-time build, or nothing and a reason.</summary>
    private static IAnalyzerResult? BuildOne(IProjectAnalyzer analyzer, Options options, TextWriter log)
    {
        var name = Path.GetFileName(analyzer.ProjectFile.Path);
        var started = DateTime.UtcNow;

        var plain = Attempt(innerBuilds: false);
        var results = plain;

        // **A multi-targeting project has no `Compile` target to run.** `TargetFrameworks`
        // plural makes the project an *outer* build whose whole job is to dispatch to one
        // inner build per framework, and `Compile` lives only on the inner ones — so the
        // first attempt comes back `MSB4057: the target does not exist`. Asking the outer
        // build to dispatch `Compile` rather than its default `Build` reaches the same
        // `CoreCompile`, once per framework, and `Preferred` still picks one to walk.
        //
        // Tried second rather than first because which of the two is right is a property
        // of the project, not of the repository: a single-targeted project has no
        // `DispatchToInnerBuilds` either, and would fail the mirror-image way.
        if (Preferred(results) is null)
        {
            results = Attempt(innerBuilds: true);
        }

        if (results is null)
        {
            return null;
        }

        if (Preferred(results) is not { } result)
        {
            // The first error is nearly always the real one, and a repository that will
            // not restore says so in the same three words four hundred times.
            //
            // **Both attempts are asked, and "no such target" is discounted.** One of
            // the two is always wrong about this project by construction — a
            // single-targeted project has no `DispatchToInnerBuilds` and a
            // multi-targeted one has no `Compile` — so reporting the last attempt's
            // error tells every reader the wrong thing about why their project was
            // skipped. What is wanted is whichever attempt failed for a reason of its
            // own.
            var reason = Reasons(plain).Concat(Reasons(results))
                .FirstOrDefault(error => !error.Contains("does not exist in the project", StringComparison.Ordinal))
                ?? Reasons(plain).Concat(Reasons(results)).FirstOrDefault();

            Say($"  ! {name}: the design-time build failed, skipping it"
                + (reason is null ? string.Empty : $" — {reason}"));

            return null;
        }

        var elapsed = (DateTime.UtcNow - started).TotalSeconds;
        Say($"  built {name} ({result.TargetFramework}, {result.SourceFiles.Length} files, {elapsed:F1}s)");

        return result;

        // One design-time build, or nothing and a reason. A throw is this project's
        // failure and not the run's, exactly as a build error is.
        IAnalyzerResults? Attempt(bool innerBuilds)
        {
            try
            {
                return analyzer.Build(BuildOptions(options, innerBuilds));
            }
            catch (Exception failure)
            {
                Say($"  ! {name}: the design-time build threw — {failure.Message}");
                return null;
            }
        }

        // Several builds run at once, and a half-interleaved progress line is worse
        // than a slightly delayed one.
        void Say(string line)
        {
            lock (log)
            {
                log.WriteLine(line);
            }
        }
    }

    /// <summary>Every project the entry point names.</summary>
    private static IReadOnlyList<IProjectAnalyzer> Analyzers(string entry, Options options, TextWriter log)
    {
        var managerOptions = new AnalyzerManagerOptions
        {
            LogWriter = options.Verbose ? log : null,
        };

        if (entry.EndsWith(".csproj", StringComparison.OrdinalIgnoreCase))
        {
            var single = new AnalyzerManager(managerOptions);
            var project = single.GetProject(IOPath.Parse(entry));

            return project is null ? [] : [project];
        }

        var manager = new AnalyzerManager(IOPath.Parse(entry), managerOptions);

        var projects = manager.Projects.Values
            .Where(analyzer => analyzer.ProjectFile.Path.EndsWith(".csproj", StringComparison.OrdinalIgnoreCase))
            .OrderBy(analyzer => analyzer.ProjectFile.Path, StringComparer.Ordinal)
            .ToList();

        log.WriteLine($"  {projects.Count} C# project(s) in the solution");
        return projects;
    }

    private static EnvironmentOptions BuildOptions(Options options, bool innerBuilds)
    {
        var environment = new EnvironmentOptions
        {
            // The design-time build is the point: evaluate and resolve, do not compile.
            DesignTime = true,
            Restore = options.Restore,
            Preference = EnvironmentPreference.Core,
        };

        // **Which `dotnet` runs MSBuild is the checkout's business, not this process's.**
        // A repository pinning an SDK in `global.json` — dotnet/runtime pins a preview
        // one and bootstraps it into `.dotnet` — needs *that* host: MSBuild ships as a
        // managed dll beside the SDK, and the framework it asks for is the SDK's own.
        // Left to the default, Buildalyzer spawns whichever `dotnet` this indexer was
        // launched by, which resolves the pinned SDK and then cannot run it.
        if (options.Dotnet is { } host)
        {
            environment.DotnetExePath = host;
        }

        // **`Compile`, not `Build`, and certainly not Buildalyzer's default `Clean;Build`.**
        // Both of those delete things. `Clean` is obvious; `Build` is not — it depends
        // on `IncrementalClean`, which removes whatever the *last* build wrote and this
        // one did not, and a design-time build writes nothing. So a run over a checkout
        // someone had already built would empty every `bin` in it. It did exactly that
        // here, to the indexer's own output, while the indexer was running out of it.
        //
        // `Compile` reaches `CoreCompile` through `ResolveReferences`, which is where
        // the compiler command line — the source list, the references, the defines — is
        // logged, and that is the whole of what Buildalyzer reads.
        environment.TargetsToBuild.Clear();

        if (innerBuilds)
        {
            // The outer build of a multi-targeting project, asked to dispatch `Compile`
            // to each inner build rather than its default `Build` — which would drag
            // `IncrementalClean` back in, one framework at a time.
            environment.TargetsToBuild.Add("DispatchToInnerBuilds");
            environment.GlobalProperties["InnerTargets"] = "Compile";
        }
        else
        {
            environment.TargetsToBuild.Add("Compile");
        }

        // Node reuse leaves MSBuild processes alive between builds, which over a few
        // hundred projects is a few hundred idle processes holding a machine's memory.
        environment.EnvironmentVariables["MSBUILDDISABLENODEREUSE"] = "1";
        environment.EnvironmentVariables["DOTNET_CLI_TELEMETRY_OPTOUT"] = "1";

        return environment;
    }

    /// <summary>
    /// One target framework's result, preferring the newest .NET a multi-targeted
    /// project builds for.
    /// </summary>
    /// <remarks>
    /// Indexing every target framework of a multi-targeted project would index the same
    /// files two or three times over. They dedup on the way in — the facts are
    /// identical — but the work is not, so one is picked here.
    /// </remarks>
    /// <summary>What MSBuild said went wrong, in the order it said it.</summary>
    private static IEnumerable<string> Reasons(IAnalyzerResults? results) =>
        results is null
            ? []
            : results.BuildEventArguments
                .OfType<Microsoft.Build.Framework.BuildErrorEventArgs>()
                .Select(error => error.Message)
                .OfType<string>();

    private static IAnalyzerResult? Preferred(IAnalyzerResults? results) =>
        results?.Results
            .Where(result => result.Succeeded && result.SourceFiles is { Length: > 0 })
            .OrderByDescending(result => Rank(result.TargetFramework))
            .FirstOrDefault();

    private static (int Family, int Version) Rank(string? framework)
    {
        if (string.IsNullOrEmpty(framework))
        {
            return (0, 0);
        }

        // `net10.0` beats `net8.0` beats `netstandard2.0` beats `net472`.
        if (framework.StartsWith("netstandard", StringComparison.OrdinalIgnoreCase))
        {
            return (1, Digits(framework));
        }

        if (framework.StartsWith("net", StringComparison.OrdinalIgnoreCase) && framework.Contains('.'))
        {
            return (2, Digits(framework));
        }

        return (0, Digits(framework));

        static int Digits(string text)
        {
            var value = 0;
            foreach (var character in text)
            {
                if (char.IsAsciiDigit(character))
                {
                    value = (value * 10) + (character - '0');
                }
            }

            return value;
        }
    }

    /// <summary>
    /// Every <c>.cs</c> file under the source, parsed against the running framework's
    /// reference set — no MSBuild, no NuGet, no project graph.
    /// </summary>
    /// <remarks>
    /// This is the honest degraded mode. Declarations are all still found: they are in
    /// the syntax. References to anything whose type comes from a NuGet package are not,
    /// because the type is an error type and the member on it binds to nothing. It is
    /// here so that a repository which will not restore still produces an index, and so
    /// that a run measuring the <i>database</i> need not wait for MSBuild first.
    /// </remarks>
    /// <summary>
    /// The syntax-only walk, and the build layer read straight off the disk beside it.
    /// </summary>
    /// <remarks>
    /// No MSBuild means no resolved framework and no exact source list, but the project
    /// files are still there and still say what they reference — so the layer is thinner
    /// rather than absent, and <see cref="ProjectIndex"/> is explicit about which of the
    /// two a fact came from.
    /// </remarks>
    private static LoadedSolution Syntax(Options options, string root, TextWriter log) =>
        new([SyntaxOnly(options, log)], ProjectIndex.Build(root, options.Source, [], log));

    private static LoadedProject SyntaxOnly(Options options, TextWriter log)
    {
        var root = Directory.Exists(options.Source)
            ? options.Source
            : Path.GetDirectoryName(options.Source)!;

        var found = Directory
            .EnumerateFiles(root, "*.cs", SearchOption.AllDirectories)
            .Where(Indexable)
            .OrderBy(path => path, StringComparer.Ordinal)
            .ToList();

        // `--max-files` bounds the *parse* here, not just the walk. Parsing seventeen
        // thousand files to index two thousand of them is the wrong shape for the flag
        // people reach for when they want a quick answer.
        //
        // With `--skip-files` it is also the slice: this compilation holds files
        // [skip, skip + max) of the source root in path order, and the next run holds
        // the ones after them. Path order is what makes the slices a partition rather
        // than a lottery — the same run twice is the same files.
        IEnumerable<string> slice = found;

        if (options.SkipFiles > 0)
        {
            slice = slice.Skip(options.SkipFiles);
        }

        if (options.MaxFiles > 0)
        {
            slice = slice.Take(options.MaxFiles);
        }

        var files = ReferenceEquals(slice, found) ? found : slice.ToList();

        log.WriteLine($"  syntax-only: {files.Count} of {found.Count} file(s) under {root}"
            + (options.SkipFiles > 0 ? $", skipping the first {options.SkipFiles}" : string.Empty));

        var parse = new CSharpParseOptions(LanguageVersion.Preview);
        var trees = new List<SyntaxTree>(files.Count);

        foreach (var file in files)
        {
            try
            {
                trees.Add(CSharpSyntaxTree.ParseText(File.ReadAllText(file), parse, path: file));
            }
            catch (IOException failure)
            {
                log.WriteLine($"  ! {file}: {failure.Message}");
            }
        }

        // The framework the indexer itself is running on. Not the framework the corpus
        // targets — which is exactly the imprecision this mode is admitting to.
        var references = ((string?)AppContext.GetData("TRUSTED_PLATFORM_ASSEMBLIES") ?? string.Empty)
            .Split(Path.PathSeparator, StringSplitOptions.RemoveEmptyEntries)
            .Where(path => path.EndsWith(".dll", StringComparison.OrdinalIgnoreCase))
            .Select(path => (MetadataReference)MetadataReference.CreateFromFile(path))
            .ToList();

        var compilation = CSharpCompilation.Create(
            "syntax-only",
            trees,
            references,
            new CSharpCompilationOptions(OutputKind.DynamicallyLinkedLibrary));

        return new LoadedProject("syntax-only", () => compilation);

        static bool Indexable(string path) =>
            !path.Contains($"{Path.DirectorySeparatorChar}obj{Path.DirectorySeparatorChar}", StringComparison.Ordinal)
            && !path.Contains($"{Path.DirectorySeparatorChar}bin{Path.DirectorySeparatorChar}", StringComparison.Ordinal);
    }

    /// <summary>A solution, a project, or the directory one lives in.</summary>
    private static string ResolveEntryPoint(string source)
    {
        if (File.Exists(source))
        {
            return source;
        }

        if (!Directory.Exists(source))
        {
            throw new FileNotFoundException($"nothing to index at {source}");
        }

        // `.slnx` first: a repository carrying both is mid-migration, and the XML one is
        // the one being kept.
        foreach (var pattern in (string[])["*.slnx", "*.sln", "*.csproj"])
        {
            var found = Directory
                .EnumerateFiles(source, pattern, SearchOption.TopDirectoryOnly)
                .OrderBy(path => path, StringComparer.Ordinal)
                .FirstOrDefault();

            if (found is not null)
            {
                return found;
            }
        }

        throw new FileNotFoundException(
            $"no .slnx, .sln or .csproj directly under {source} — name one with --source, or use --syntax-only");
    }
}
