using Aperture.Client;

namespace Aperture.Indexer;

/// <summary>
/// The built-in code-index schema, written down a third time — and on purpose.
/// </summary>
/// <remarks>
/// <para>
/// The server has this schema hardcoded (<c>aperture::code_index</c>) until schemas are
/// parsed, and a client must have it too: the transport codec sends no field names, no
/// type markers and no arities. The handshake compares fingerprints, so a disagreement
/// is refused before a byte of data flows.
/// </para>
/// <para>
/// It is stated here rather than shared with <c>Aperture.Demo</c> for the same reason
/// the demo does not share it with the Rust side: an independent statement is what the
/// fingerprint is <i>for</i>. Two projects reading one constant would agree by
/// construction, which is the agreement being tested.
/// </para>
/// <para>
/// <b>Two rules are load-bearing.</b> A predicate's id <i>is</i> its position in the
/// list, and a record's fields are in the schema's declared order — sorted by name —
/// because that order is the encoding. The <c>*Fact</c> helpers below are the only
/// place either rule has to be remembered.
/// </para>
/// </remarks>
internal static class CodeIndex
{
    public const uint File = 0;
    public const uint Module = 1;
    public const uint Decl = 2;
    public const uint SearchByName = 3;
    public const uint Ref = 4;
    public const uint Import = 5;

    // The build layer: what compiled this file, and into what.
    public const uint Project = 6;
    public const uint Assembly = 7;
    public const uint Compilation = 8;
    public const uint ProjectSource = 9;
    public const uint ProjectRef = 10;
    public const uint Package = 11;
    public const uint PackageRef = 12;

    // The declaration graph, and what a declaration says about itself.
    public const uint Member = 13;
    public const uint Extends = 14;
    public const uint Implements = 15;
    public const uint Override = 16;
    public const uint Param = 17;
    public const uint TypeOf = 18;
    public const uint Doc = 19;
    public const uint Attribute = 20;
    public const uint Line = 21;

    /// <summary>Every predicate id, in schema order — what a report iterates.</summary>
    public static readonly uint[] Predicates =
    [
        File, Module, Decl, SearchByName, Ref, Import,
        Project, Assembly, Compilation, ProjectSource, ProjectRef, Package, PackageRef,
        Member, Extends, Implements, Override, Param, TypeOf, Doc, Attribute, Line,
    ];

    public static readonly ApertureSchema Schema = new([
        new AperturePredicate("src.File", ApertureType.String, null),

        new AperturePredicate("src.Module", ApertureType.Rec(
            ("file", ApertureType.Reference(File)),
            ("name", ApertureType.String)), null),

        // The value side is the declaration's kind. A value cannot be matched on (I6),
        // which is what makes it the right home for something a query reads but never
        // filters by.
        // Declared {module, name, line}: the join is "the declarations in this module",
        // and the line is what tells two of them apart rather than what finds them.
        new AperturePredicate("src.Decl", ApertureType.Rec(
            ("module", ApertureType.Reference(Module)),
            ("name", ApertureType.String),
            ("line", ApertureType.Integer)), ApertureType.String),

        new AperturePredicate("src.SearchByName", ApertureType.Rec(
            ("name", ApertureType.String),
            ("to", ApertureType.Reference(Decl))), null),

        // Declared {to, file, at}: find-references is the question, and it seeks only
        // if the target leads.
        new AperturePredicate("src.Ref", ApertureType.Rec(
            ("to", ApertureType.Reference(Decl)),
            ("file", ApertureType.Reference(File)),
            ("at", ApertureType.Rec(
                ("line", ApertureType.Integer),
                ("col", ApertureType.Integer)))), null),

        new AperturePredicate("src.Import", ApertureType.Rec(
            ("from", ApertureType.Reference(Module)),
            ("to", ApertureType.Reference(Module))), null),

        // ---- the build layer -------------------------------------------------------
        //
        // A project is a path, as a file is. What it is not is the module: a module is
        // a namespace, and a namespace spans projects as freely as a project spans
        // namespaces.
        new AperturePredicate("src.Project", ApertureType.String, null),

        new AperturePredicate("src.Assembly", ApertureType.String, null),

        // The crossing, and where the multiplicity is: one project builds for several
        // frameworks, and one assembly name is produced by several projects.
        new AperturePredicate("src.Compilation", ApertureType.Rec(
            ("assembly", ApertureType.Reference(Assembly)),
            ("framework", ApertureType.String),
            ("project", ApertureType.Reference(Project))), null),

        new AperturePredicate("src.ProjectSource", ApertureType.Rec(
            ("file", ApertureType.Reference(File)),
            ("project", ApertureType.Reference(Project))), null),

        new AperturePredicate("src.ProjectRef", ApertureType.Rec(
            ("from", ApertureType.Reference(Project)),
            ("to", ApertureType.Reference(Project))), null),

        // A package is its name *and* its version: two versions of one package is a
        // thing a repository has and a thing a query should be able to ask about.
        new AperturePredicate("src.Package", ApertureType.Rec(
            ("name", ApertureType.String),
            ("version", ApertureType.String)), null),

        new AperturePredicate("src.PackageRef", ApertureType.Rec(
            ("package", ApertureType.Reference(Package)),
            ("project", ApertureType.Reference(Project))), null),

        // ---- the declaration graph -------------------------------------------------
        //
        // All four are keyed container-first — which is what naming the fields `base`,
        // `iface` and `container` buys, since the field order is sorted and the field
        // order is the key order.
        new AperturePredicate("src.Member", ApertureType.Rec(
            ("container", ApertureType.Reference(Decl)),
            ("member", ApertureType.Reference(Decl))), null),

        new AperturePredicate("src.Extends", ApertureType.Rec(
            ("base", ApertureType.Reference(Decl)),
            ("type", ApertureType.Reference(Decl))), null),

        new AperturePredicate("src.Implements", ApertureType.Rec(
            ("iface", ApertureType.Reference(Decl)),
            ("type", ApertureType.Reference(Decl))), null),

        new AperturePredicate("src.Override", ApertureType.Rec(
            ("base", ApertureType.Reference(Decl)),
            ("member", ApertureType.Reference(Decl))), null),

        // The integer in the middle of the key is what makes one seek walk a method's
        // parameters in declaration order.
        new AperturePredicate("src.Param", ApertureType.Rec(
            ("decl", ApertureType.Reference(Decl)),
            ("index", ApertureType.Integer),
            ("name", ApertureType.String)), ApertureType.String),

        // A key of one field: a declaration has at most one type and at most one doc
        // comment, so the declaration alone is the identity and the answer is a value.
        new AperturePredicate("src.TypeOf", ApertureType.Rec(
            ("decl", ApertureType.Reference(Decl))), ApertureType.String),

        new AperturePredicate("src.Doc", ApertureType.Rec(
            ("decl", ApertureType.Reference(Decl))), ApertureType.String),

        // The attribute is a name rather than a reference: the framework's own
        // attributes are declared outside any index of this repository, and a join
        // through a declaration that is not here answers nothing.
        new AperturePredicate("src.Attribute", ApertureType.Rec(
            ("attribute", ApertureType.String),
            ("target", ApertureType.Reference(Decl))), null),

        // A file's line table, one fact per line, because there are no arrays — and the
        // widest row in the schema, since the value is a line of source.
        new AperturePredicate("src.Line", ApertureType.Rec(
            ("file", ApertureType.Reference(File)),
            ("line", ApertureType.Integer)), ApertureType.String),
    ]);

    public static string NameOf(uint predicate) => Schema[predicate].Name;

    public static ApertureFact FileFact(string path) =>
        new(File, ApertureValue.Of(path));

    public static ApertureFact ModuleFact(ApertureFact file, string name) =>
        new(Module, ApertureValue.Rec(
            ApertureValue.Of(ApertureRef.To(file)),
            ApertureValue.Of(name)));

    public static ApertureFact DeclFact(long line, ApertureFact module, string name, string kind) =>
        new(Decl,
            ApertureValue.Rec(
                ApertureValue.Of(ApertureRef.To(module)),
                ApertureValue.Of(name),
                ApertureValue.Of(line)),
            ApertureValue.Of(kind));

    public static ApertureFact SearchFact(string name, ApertureFact decl) =>
        new(SearchByName, ApertureValue.Rec(
            ApertureValue.Of(name),
            ApertureValue.Of(ApertureRef.To(decl))));

    public static ApertureFact RefFact(long line, long col, ApertureFact file, ApertureFact decl) =>
        new(Ref, ApertureValue.Rec(
            ApertureValue.Of(ApertureRef.To(decl)),
            ApertureValue.Of(ApertureRef.To(file)),
            ApertureValue.Rec(ApertureValue.Of(line), ApertureValue.Of(col))));

    public static ApertureFact ImportFact(ApertureFact from, ApertureFact to) =>
        new(Import, ApertureValue.Rec(
            ApertureValue.Of(ApertureRef.To(from)),
            ApertureValue.Of(ApertureRef.To(to))));

    public static ApertureFact ProjectFact(string path) =>
        new(Project, ApertureValue.Of(path));

    public static ApertureFact AssemblyFact(string name) =>
        new(Assembly, ApertureValue.Of(name));

    public static ApertureFact CompilationFact(ApertureFact assembly, string framework, ApertureFact project) =>
        new(Compilation, ApertureValue.Rec(
            ApertureValue.Of(ApertureRef.To(assembly)),
            ApertureValue.Of(framework),
            ApertureValue.Of(ApertureRef.To(project))));

    public static ApertureFact ProjectSourceFact(ApertureFact file, ApertureFact project) =>
        new(ProjectSource, ApertureValue.Rec(
            ApertureValue.Of(ApertureRef.To(file)),
            ApertureValue.Of(ApertureRef.To(project))));

    public static ApertureFact ProjectRefFact(ApertureFact from, ApertureFact to) =>
        new(ProjectRef, ApertureValue.Rec(
            ApertureValue.Of(ApertureRef.To(from)),
            ApertureValue.Of(ApertureRef.To(to))));

    public static ApertureFact PackageFact(string name, string version) =>
        new(Package, ApertureValue.Rec(
            ApertureValue.Of(name),
            ApertureValue.Of(version)));

    public static ApertureFact PackageRefFact(ApertureFact package, ApertureFact project) =>
        new(PackageRef, ApertureValue.Rec(
            ApertureValue.Of(ApertureRef.To(package)),
            ApertureValue.Of(ApertureRef.To(project))));

    public static ApertureFact MemberFact(ApertureFact container, ApertureFact member) =>
        new(Member, ApertureValue.Rec(
            ApertureValue.Of(ApertureRef.To(container)),
            ApertureValue.Of(ApertureRef.To(member))));

    public static ApertureFact ExtendsFact(ApertureFact @base, ApertureFact type) =>
        new(Extends, ApertureValue.Rec(
            ApertureValue.Of(ApertureRef.To(@base)),
            ApertureValue.Of(ApertureRef.To(type))));

    public static ApertureFact ImplementsFact(ApertureFact iface, ApertureFact type) =>
        new(Implements, ApertureValue.Rec(
            ApertureValue.Of(ApertureRef.To(iface)),
            ApertureValue.Of(ApertureRef.To(type))));

    public static ApertureFact OverrideFact(ApertureFact @base, ApertureFact member) =>
        new(Override, ApertureValue.Rec(
            ApertureValue.Of(ApertureRef.To(@base)),
            ApertureValue.Of(ApertureRef.To(member))));

    public static ApertureFact ParamFact(ApertureFact decl, long index, string name, string type) =>
        new(Param,
            ApertureValue.Rec(
                ApertureValue.Of(ApertureRef.To(decl)),
                ApertureValue.Of(index),
                ApertureValue.Of(name)),
            ApertureValue.Of(type));

    public static ApertureFact TypeOfFact(ApertureFact decl, string type) =>
        new(TypeOf,
            ApertureValue.Rec(ApertureValue.Of(ApertureRef.To(decl))),
            ApertureValue.Of(type));

    public static ApertureFact DocFact(ApertureFact decl, string text) =>
        new(Doc,
            ApertureValue.Rec(ApertureValue.Of(ApertureRef.To(decl))),
            ApertureValue.Of(text));

    public static ApertureFact AttributeFact(string attribute, ApertureFact target) =>
        new(Attribute, ApertureValue.Rec(
            ApertureValue.Of(attribute),
            ApertureValue.Of(ApertureRef.To(target))));

    public static ApertureFact LineFact(ApertureFact file, long line, string text) =>
        new(Line,
            ApertureValue.Rec(
                ApertureValue.Of(ApertureRef.To(file)),
                ApertureValue.Of(line)),
            ApertureValue.Of(text));
}
