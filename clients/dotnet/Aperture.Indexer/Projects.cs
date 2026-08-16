using System.Xml.Linq;

using Aperture.Client;

using Buildalyzer;

namespace Aperture.Indexer;

/// <summary>
/// One project: where it is, what it builds, and what it builds against.
/// </summary>
/// <remarks>
/// Held rather than recomputed because the <see cref="Fact"/> is nested into every
/// <c>src.ProjectSource</c> edge, and a repository's larger projects have thousands of
/// files each.
/// </remarks>
internal sealed class ProjectInfo(string path)
{
    /// <summary>The project file, relative to the index root, with forward slashes.</summary>
    public string Path { get; } = path;

    public ApertureFact Fact { get; } = CodeIndex.ProjectFact(path);

    /// <summary>
    /// The assembly this produces — MSBuild's own default until something says
    /// otherwise, which is the project file's base name.
    /// </summary>
    public string Assembly { get; set; } = System.IO.Path.GetFileNameWithoutExtension(path);

    /// <summary>
    /// The frameworks it targets, <b>as the project states them</b>.
    /// </summary>
    /// <remarks>
    /// A design-time build hands back a framework MSBuild has already resolved. Read
    /// from the project file instead, a multi-targeting repository commonly names one
    /// by property — <c>$(NetCoreAppCurrent)</c> — and that is what is recorded, since
    /// the alternative is either inventing a value or dropping the project's only link
    /// to the assembly it produces. Which of the two a run got is what
    /// <c>--syntax-only</c> says about itself.
    /// </remarks>
    public List<string> Frameworks { get; } = [];

    /// <summary>Projects it references, as index-relative paths.</summary>
    public List<string> ProjectRefs { get; } = [];

    public List<(string Name, string Version)> Packages { get; } = [];

    /// <summary>Whether a design-time build answered for this project, rather than its XML.</summary>
    public bool Built { get; set; }
}

/// <summary>
/// The build layer: every project in the checkout, and which of them compiles a file.
/// </summary>
/// <remarks>
/// <para>
/// <b>Two sources of knowledge, and the better one wins where it exists.</b> Every
/// <c>.csproj</c> under the source is read as XML — which always works, needs no SDK and
/// no restore, and is how a repository that will not build still gets a build layer.
/// A design-time build that succeeded then refines its own project: the resolved target
/// framework, the assembly name MSBuild actually computed, package versions after
/// central package management has had its say, and the <i>exact</i> source list rather
/// than a guess.
/// </para>
/// <para>
/// <b>The guess, where there is no build, is containment</b>: a file belongs to the
/// nearest project at or above it. That is right for the ordinary layout and wrong for
/// shared source — <c>src/libraries/Common</c> in dotnet/runtime is compiled into a
/// hundred assemblies by explicit <c>&lt;Compile Include&gt;</c> and lives under no
/// project at all. Such a file gets no <c>src.ProjectSource</c> edge rather than a
/// plausible one: an index that quietly attributes shared code to whichever project
/// happens to sit above it answers "what builds this" wrongly, and nothing downstream
/// can tell.
/// </para>
/// </remarks>
internal sealed class ProjectIndex
{
    private readonly Dictionary<string, ProjectInfo> _byPath = new(StringComparer.Ordinal);

    /// <summary>Exact membership, from the design-time builds that succeeded.</summary>
    private readonly Dictionary<string, List<ProjectInfo>> _byFile = new(StringComparer.Ordinal);

    /// <summary>
    /// The projects living in each directory, for the containment fallback.
    /// </summary>
    /// <remarks>
    /// A list per directory rather than one project, because a directory holding two
    /// project files is ordinary — a reference assembly beside its implementation — and
    /// picking one of them by name order would be a coin toss recorded as a fact.
    /// </remarks>
    private readonly Dictionary<string, List<ProjectInfo>> _byDirectory = new(StringComparer.Ordinal);

    public IReadOnlyCollection<ProjectInfo> Projects => _byPath.Values;

    /// <summary>How many projects a design-time build, rather than XML, answered for.</summary>
    public int Built => _byPath.Values.Count(project => project.Built);

    /// <summary>
    /// Read every project under <paramref name="source"/>, then let the builds that
    /// succeeded overwrite what the XML could only approximate.
    /// </summary>
    public static ProjectIndex Build(
        string root,
        string source,
        IReadOnlyList<IAnalyzerResult> results,
        TextWriter log)
    {
        var index = new ProjectIndex();
        var directory = Directory.Exists(source) ? source : System.IO.Path.GetDirectoryName(source)!;

        foreach (var file in Discover(directory))
        {
            if (Paths.Relative(root, file) is not { } path)
            {
                continue;
            }

            var project = new ProjectInfo(path);
            index.Read(root, file, project);
            index._byPath[path] = project;
        }

        foreach (var result in results)
        {
            index.Refine(root, result);
        }

        foreach (var project in index._byPath.Values)
        {
            var holding = Parent(project.Path);

            if (!index._byDirectory.TryGetValue(holding, out var here))
            {
                here = [];
                index._byDirectory[holding] = here;
            }

            here.Add(project);
        }

        log.WriteLine($"  build layer: {index._byPath.Count} project(s), "
            + $"{index.Built} from a design-time build, {index._byFile.Count} file(s) attributed exactly");

        return index;
    }

    /// <summary>The projects that compile <paramref name="file"/>, which may be none.</summary>
    /// <remarks>
    /// An exact answer is the whole answer: a file MSBuild listed for one project is not
    /// silently also attributed to whatever project sits above it on disk.
    /// </remarks>
    public IReadOnlyList<ProjectInfo> Owners(string file)
    {
        if (_byFile.TryGetValue(file, out var exact))
        {
            return exact;
        }

        // Up the path rather than across the projects: a walk asks this for every file
        // it reaches, and a repository has far more projects than a path has segments.
        for (var directory = Parent(file); ; directory = Parent(directory))
        {
            if (_byDirectory.TryGetValue(directory, out var here))
            {
                // **One project in the directory is an answer; several is not.**
                // dotnet/runtime's `src/tests/JIT/CodeGenBringUpTests` holds 645
                // project files beside its sources, one per test — containment says
                // "one of these 645" and a fact saying all of them is 644 edges that
                // are not true. Same rule as shared source, one directory lower: no
                // edge rather than a plausible one, and the run counts it.
                return here.Count == 1 ? here : [];
            }

            if (directory.Length == 0)
            {
                return [];
            }
        }
    }

    /// <summary>
    /// Write the build layer itself: the projects, the assemblies, the compilations that
    /// pair them, and the two dependency graphs.
    /// </summary>
    /// <remarks>
    /// Emitted once, up front, rather than as files are reached — this is what the
    /// repository is, not what the walk found, and a run stopped early by
    /// <c>--max-files</c> should still say so.
    /// </remarks>
    public void Emit(FactSink sink)
    {
        foreach (var project in _byPath.Values)
        {
            sink.Add(CodeIndex.Project, project.Fact);

            var assembly = CodeIndex.AssemblyFact(project.Assembly);
            sink.Add(CodeIndex.Assembly, assembly);

            // A project that names no framework still compiles into an assembly, and the
            // compilation is the only fact that says which — so it gets one, with the
            // empty string where the framework would be. Nobody can mistake that for a
            // target framework, which is the whole requirement.
            foreach (var framework in project.Frameworks.Count > 0 ? project.Frameworks : [""])
            {
                sink.Add(
                    CodeIndex.Compilation,
                    CodeIndex.CompilationFact(assembly, framework, project.Fact));
            }

            foreach (var path in project.ProjectRefs)
            {
                // A reference out of the indexed tree — a project in a sibling
                // repository, or one `--source` did not reach. The target has no facts
                // here, and an edge to a project nothing else mentions is an edge to
                // nothing.
                if (_byPath.TryGetValue(path, out var target))
                {
                    sink.Add(
                        CodeIndex.ProjectRef,
                        CodeIndex.ProjectRefFact(project.Fact, target.Fact));
                }
            }

            foreach (var (name, version) in project.Packages)
            {
                var package = CodeIndex.PackageFact(name, version);
                sink.Add(CodeIndex.Package, package);
                sink.Add(CodeIndex.PackageRef, CodeIndex.PackageRefFact(package, project.Fact));
            }
        }
    }

    /// <summary>What a design-time build knows and the project file cannot say.</summary>
    private void Refine(string root, IAnalyzerResult result)
    {
        if (Paths.Relative(root, result.ProjectFilePath) is not { } path
            || !_byPath.TryGetValue(path, out var project))
        {
            return;
        }

        project.Built = true;

        if (result.TargetFramework is { Length: > 0 } framework)
        {
            // The resolved framework replaces whatever the XML said, rather than joining
            // it: `net10.0` and `$(NetCoreAppCurrent)` are the same target framework
            // said twice, and two compilation facts would claim otherwise.
            project.Frameworks.Clear();
            project.Frameworks.Add(framework);
        }

        if (Property(result, "AssemblyName") is { Length: > 0 } assembly)
        {
            project.Assembly = assembly;
        }

        if (result.PackageReferences is { Count: > 0 } packages)
        {
            project.Packages.Clear();

            foreach (var (name, metadata) in packages)
            {
                project.Packages.Add((name, Version(metadata)));
            }
        }

        foreach (var reference in result.ProjectReferences)
        {
            if (Paths.Relative(root, reference) is { } target && !project.ProjectRefs.Contains(target))
            {
                project.ProjectRefs.Add(target);
            }
        }

        // **The exact source list**, which is the one thing containment cannot give:
        // shared files, generated files, and files a glob excluded all differ from what
        // sits under the project's directory.
        foreach (var source in result.SourceFiles)
        {
            if (Paths.Relative(root, source) is not { } file || Paths.IsBuildOutput(file))
            {
                continue;
            }

            if (!_byFile.TryGetValue(file, out var owners))
            {
                owners = [];
                _byFile[file] = owners;
            }

            if (!owners.Contains(project))
            {
                owners.Add(project);
            }
        }
    }

    /// <summary>
    /// What the project file itself says: its assembly name, its frameworks, and both
    /// kinds of reference.
    /// </summary>
    /// <remarks>
    /// XML rather than MSBuild evaluation, deliberately. Evaluating means an SDK, a
    /// restore and an out-of-process build per project — which is exactly what the
    /// design-time path already does and exactly what is unavailable when it fails. A
    /// property this cannot expand is recorded unexpanded rather than guessed at.
    /// </remarks>
    private void Read(string root, string file, ProjectInfo project)
    {
        XDocument document;

        try
        {
            document = XDocument.Load(file);
        }
        catch (Exception failure) when (failure is IOException or System.Xml.XmlException)
        {
            // A project this cannot read is still a project, and its path is still true.
            return;
        }

        var directory = System.IO.Path.GetDirectoryName(file)!;

        foreach (var element in document.Descendants())
        {
            switch (element.Name.LocalName)
            {
                case "AssemblyName" when Literal(element.Value):
                    project.Assembly = element.Value.Trim();
                    break;

                case "TargetFramework" or "TargetFrameworks":
                    foreach (var framework in element.Value.Split(';', StringSplitOptions.RemoveEmptyEntries
                        | StringSplitOptions.TrimEntries))
                    {
                        if (!project.Frameworks.Contains(framework))
                        {
                            project.Frameworks.Add(framework);
                        }
                    }
                    break;

                case "ProjectReference" when element.Attribute("Include")?.Value is { Length: > 0 } include:
                {
                    var target = System.IO.Path.GetFullPath(
                        System.IO.Path.Combine(directory, include.Replace('\\', '/')));

                    if (Paths.Relative(root, target) is { } path && !project.ProjectRefs.Contains(path))
                    {
                        project.ProjectRefs.Add(path);
                    }
                    break;
                }

                case "PackageReference" when element.Attribute("Include")?.Value is { Length: > 0 } name:
                {
                    // The version lives in an attribute or a child element, and under
                    // central package management in neither — the empty string is what
                    // "the project file does not say" looks like, and nobody can mistake
                    // it for a version.
                    var version = element.Attribute("Version")?.Value
                        ?? element.Elements().FirstOrDefault(child => child.Name.LocalName == "Version")?.Value
                        ?? string.Empty;

                    project.Packages.Add((name.Trim(), Literal(version) ? version.Trim() : string.Empty));
                    break;
                }
            }
        }
    }

    /// <summary>Every project file under a directory, ignoring build output.</summary>
    private static IEnumerable<string> Discover(string directory)
    {
        IEnumerable<string> found;

        try
        {
            found = Directory.EnumerateFiles(directory, "*.csproj", SearchOption.AllDirectories);
        }
        catch (IOException)
        {
            return [];
        }

        return found
            .Where(path => !path.Contains($"{System.IO.Path.DirectorySeparatorChar}bin{System.IO.Path.DirectorySeparatorChar}", StringComparison.Ordinal)
                && !path.Contains($"{System.IO.Path.DirectorySeparatorChar}obj{System.IO.Path.DirectorySeparatorChar}", StringComparison.Ordinal))
            .OrderBy(path => path, StringComparer.Ordinal);
    }

    /// <summary>An MSBuild property from the build, or nothing if it did not report one.</summary>
    private static string? Property(IAnalyzerResult result, string name) =>
        result.Properties is { } properties && properties.TryGetValue(name, out var value) ? value : null;

    private static string Version(IReadOnlyDictionary<string, string> metadata) =>
        metadata.TryGetValue("Version", out var version) && Literal(version)
            ? version.Trim()
            : string.Empty;

    /// <summary>Whether a project file said a value outright, rather than by property.</summary>
    private static bool Literal(string? text) =>
        text is { Length: > 0 } && !text.Contains("$(", StringComparison.Ordinal);

    /// <summary>The directory part of an index-relative path, without its trailing slash.</summary>
    private static string Parent(string path)
    {
        var slash = path.LastIndexOf('/');
        return slash < 0 ? string.Empty : path[..slash];
    }
}

/// <summary>Index-relative paths, spelled one way in one place.</summary>
internal static class Paths
{
    /// <summary>
    /// <paramref name="absolute"/> as the index names it, or nothing if it is outside
    /// the root.
    /// </summary>
    /// <remarks>
    /// A path outside the root would come back as <c>../../elsewhere</c>, which is not a
    /// name — it depends on where the root happens to be, so two runs of the same
    /// repository would disagree about it.
    /// </remarks>
    public static string? Relative(string root, string? absolute)
    {
        if (string.IsNullOrEmpty(absolute))
        {
            return null;
        }

        var relative = System.IO.Path.GetRelativePath(root, absolute)
            .Replace(System.IO.Path.DirectorySeparatorChar, '/');

        return relative.StartsWith("../", StringComparison.Ordinal) || System.IO.Path.IsPathRooted(relative)
            ? null
            : relative;
    }

    /// <summary>
    /// Build output rather than source.
    /// </summary>
    /// <remarks>
    /// <c>obj/</c> in particular holds the generated assembly attributes every project
    /// has, which would be the same six declarations in every project and none of them
    /// anything anyone wants to find.
    /// </remarks>
    public static bool IsBuildOutput(string relative) =>
        relative.Contains("/obj/", StringComparison.Ordinal)
        || relative.Contains("/bin/", StringComparison.Ordinal)
        || relative.StartsWith("obj/", StringComparison.Ordinal)
        || relative.StartsWith("bin/", StringComparison.Ordinal);
}
