using Fjord.Client;

namespace Fjord.Indexer;

/// <summary>
/// The built-in code-index schema, written down a third time — and on purpose.
/// </summary>
/// <remarks>
/// <para>
/// The server has this schema hardcoded (<c>fjord::code_index</c>) until schemas are
/// parsed, and a client must have it too: the transport codec sends no field names, no
/// type markers and no arities. The handshake compares fingerprints, so a disagreement
/// is refused before a byte of data flows.
/// </para>
/// <para>
/// It is stated here rather than shared with <c>Fjord.Demo</c> for the same reason
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

    // What a code-search viewer needs and a syntax walk cannot key for. Three of these
    // are a *second key order* over data already above — the shape a derived predicate
    // would take if one could be declared (Phase 8b).
    public const uint DeclSpan = 22;
    public const uint SearchByLowerName = 23;
    public const uint FileXRef = 24;
    public const uint DerivesFrom = 25;
    public const uint AttributeOf = 26;

/// <summary>
    /// The schema fingerprint, as <c>fjord schema fingerprint</c> prints it.
    /// </summary>
    /// <remarks>
    /// Carried rather than computed — see <see cref="FjordSchema"/>. A schema edit
    /// moves it, and a stale one is refused at the handshake by name.
    /// </remarks>
    public const ulong SchemaFingerprint = 0xb08eea634e866a75;

    /// <summary>Every predicate id, in schema order — what a report iterates.</summary>
    public static readonly uint[] Predicates =
    [
        File, Module, Decl, SearchByName, Ref, Import,
        Project, Assembly, Compilation, ProjectSource, ProjectRef, Package, PackageRef,
        Member, Extends, Implements, Override, Param, TypeOf, Doc, Attribute, Line,
        DeclSpan, SearchByLowerName, FileXRef, DerivesFrom, AttributeOf,
    ];

    public static readonly FjordSchema Schema = new([
        new FjordPredicate("src.File", FjordType.String, null),

        new FjordPredicate("src.Module", FjordType.Rec(
            ("file", FjordType.Reference(File)),
            ("name", FjordType.String)), null),

        // The value side is the declaration's kind. A value cannot be matched on (I6),
        // which is what makes it the right home for something a query reads but never
        // filters by.
        // Declared {module, name, line}: the join is "the declarations in this module",
        // and the line is what tells two of them apart rather than what finds them.
        new FjordPredicate("src.Decl", FjordType.Rec(
            ("module", FjordType.Reference(Module)),
            ("name", FjordType.String),
            ("line", FjordType.Integer)), FjordType.String),

        new FjordPredicate("src.SearchByName", FjordType.Rec(
            ("name", FjordType.String),
            ("to", FjordType.Reference(Decl))), null),

        // Declared {to, file, at}: find-references is the question, and it seeks only
        // if the target leads. `at.length` is what a viewer draws the link over, and it
        // trails the key so every seek prefix above is unchanged.
        new FjordPredicate("src.Ref", FjordType.Rec(
            ("to", FjordType.Reference(Decl)),
            ("file", FjordType.Reference(File)),
            ("at", FjordType.Rec(
                ("line", FjordType.Integer),
                ("col", FjordType.Integer),
                ("length", FjordType.Integer)))), null),

        new FjordPredicate("src.Import", FjordType.Rec(
            ("from", FjordType.Reference(Module)),
            ("to", FjordType.Reference(Module))), null),

        // ---- the build layer -------------------------------------------------------
        //
        // A project is a path, as a file is. What it is not is the module: a module is
        // a namespace, and a namespace spans projects as freely as a project spans
        // namespaces.
        new FjordPredicate("src.Project", FjordType.String, null),

        new FjordPredicate("src.Assembly", FjordType.String, null),

        // The crossing, and where the multiplicity is: one project builds for several
        // frameworks, and one assembly name is produced by several projects.
        new FjordPredicate("src.Compilation", FjordType.Rec(
            ("assembly", FjordType.Reference(Assembly)),
            ("framework", FjordType.String),
            ("project", FjordType.Reference(Project))), null),

        new FjordPredicate("src.ProjectSource", FjordType.Rec(
            ("file", FjordType.Reference(File)),
            ("project", FjordType.Reference(Project))), null),

        new FjordPredicate("src.ProjectRef", FjordType.Rec(
            ("from", FjordType.Reference(Project)),
            ("to", FjordType.Reference(Project))), null),

        // A package is its name *and* its version: two versions of one package is a
        // thing a repository has and a thing a query should be able to ask about.
        new FjordPredicate("src.Package", FjordType.Rec(
            ("name", FjordType.String),
            ("version", FjordType.String)), null),

        new FjordPredicate("src.PackageRef", FjordType.Rec(
            ("package", FjordType.Reference(Package)),
            ("project", FjordType.Reference(Project))), null),

        // ---- the declaration graph -------------------------------------------------
        //
        // All four are keyed container-first — which is what naming the fields `base`,
        // `iface` and `container` buys, since the field order is sorted and the field
        // order is the key order.
        new FjordPredicate("src.Member", FjordType.Rec(
            ("container", FjordType.Reference(Decl)),
            ("member", FjordType.Reference(Decl))), null),

        new FjordPredicate("src.Extends", FjordType.Rec(
            ("base", FjordType.Reference(Decl)),
            ("type", FjordType.Reference(Decl))), null),

        new FjordPredicate("src.Implements", FjordType.Rec(
            ("iface", FjordType.Reference(Decl)),
            ("type", FjordType.Reference(Decl))), null),

        new FjordPredicate("src.Override", FjordType.Rec(
            ("base", FjordType.Reference(Decl)),
            ("member", FjordType.Reference(Decl))), null),

        // The integer in the middle of the key is what makes one seek walk a method's
        // parameters in declaration order.
        new FjordPredicate("src.Param", FjordType.Rec(
            ("decl", FjordType.Reference(Decl)),
            ("index", FjordType.Integer),
            ("name", FjordType.String)), FjordType.String),

        // A key of one field: a declaration has at most one type and at most one doc
        // comment, so the declaration alone is the identity and the answer is a value.
        new FjordPredicate("src.TypeOf", FjordType.Rec(
            ("decl", FjordType.Reference(Decl))), FjordType.String),

        new FjordPredicate("src.Doc", FjordType.Rec(
            ("decl", FjordType.Reference(Decl))), FjordType.String),

        // The attribute is a name rather than a reference: the framework's own
        // attributes are declared outside any index of this repository, and a join
        // through a declaration that is not here answers nothing.
        new FjordPredicate("src.Attribute", FjordType.Rec(
            ("attribute", FjordType.String),
            ("target", FjordType.Reference(Decl))), null),

        // A file's line table, one fact per line, because there are no arrays — and the
        // widest row in the schema, since the value is a line of source.
        new FjordPredicate("src.Line", FjordType.Rec(
            ("file", FjordType.Reference(File)),
            ("line", FjordType.Integer)), FjordType.String),

        // ---- the viewer's key orders -----------------------------------------------

        new FjordPredicate("src.DeclSpan", FjordType.Rec(
            ("decl", FjordType.Reference(Decl)),
            ("col", FjordType.Integer),
            ("endLine", FjordType.Integer),
            ("endCol", FjordType.Integer)), null),

        new FjordPredicate("src.SearchByLowerName", FjordType.Rec(
            ("name", FjordType.String),
            ("to", FjordType.Reference(Decl))), null),

        // The same references as `src.Ref`, keyed by file and then by position — the
        // question a file view asks, and the one a target-leading key cannot answer.
        new FjordPredicate("src.FileXRef", FjordType.Rec(
            ("file", FjordType.Reference(File)),
            ("at", FjordType.Rec(
                ("line", FjordType.Integer),
                ("col", FjordType.Integer),
                ("length", FjordType.Integer))),
            ("to", FjordType.Reference(Decl))), null),

        new FjordPredicate("src.DerivesFrom", FjordType.Rec(
            ("type", FjordType.Reference(Decl)),
            ("base", FjordType.Reference(Decl))), null),

        new FjordPredicate("src.AttributeOf", FjordType.Rec(
            ("target", FjordType.Reference(Decl)),
            ("attribute", FjordType.String)), null),
    ], SchemaFingerprint);

    public static string NameOf(uint predicate) => Schema[predicate].Name;

    public static FjordFact FileFact(string path) =>
        new(File, FjordValue.Of(path));

    public static FjordFact ModuleFact(FjordFact file, string name) =>
        new(Module, FjordValue.Rec(
            FjordValue.Of(FjordRef.To(file)),
            FjordValue.Of(name)));

    public static FjordFact DeclFact(long line, FjordFact module, string name, string kind) =>
        new(Decl,
            FjordValue.Rec(
                FjordValue.Of(FjordRef.To(module)),
                FjordValue.Of(name),
                FjordValue.Of(line)),
            FjordValue.Of(kind));

    public static FjordFact SearchFact(string name, FjordFact decl) =>
        new(SearchByName, FjordValue.Rec(
            FjordValue.Of(name),
            FjordValue.Of(FjordRef.To(decl))));

    public static FjordFact RefFact(long line, long col, long length, FjordFact file, FjordFact decl) =>
        new(Ref, FjordValue.Rec(
            FjordValue.Of(FjordRef.To(decl)),
            FjordValue.Of(FjordRef.To(file)),
            FjordValue.Rec(
                FjordValue.Of(line),
                FjordValue.Of(col),
                FjordValue.Of(length))));

    /// <summary>The same reference, keyed by file and position — see `src.FileXRef`.</summary>
    public static FjordFact FileXRefFact(long line, long col, long length, FjordFact file, FjordFact decl) =>
        new(FileXRef, FjordValue.Rec(
            FjordValue.Of(FjordRef.To(file)),
            FjordValue.Rec(
                FjordValue.Of(line),
                FjordValue.Of(col),
                FjordValue.Of(length)),
            FjordValue.Of(FjordRef.To(decl))));

    public static FjordFact DeclSpanFact(FjordFact decl, long col, long endLine, long endCol) =>
        new(DeclSpan, FjordValue.Rec(
            FjordValue.Of(FjordRef.To(decl)),
            FjordValue.Of(col),
            FjordValue.Of(endLine),
            FjordValue.Of(endCol)));

    public static FjordFact SearchLowerFact(string name, FjordFact decl) =>
        new(SearchByLowerName, FjordValue.Rec(
            FjordValue.Of(name),
            FjordValue.Of(FjordRef.To(decl))));

    public static FjordFact DerivesFromFact(FjordFact type, FjordFact @base) =>
        new(DerivesFrom, FjordValue.Rec(
            FjordValue.Of(FjordRef.To(type)),
            FjordValue.Of(FjordRef.To(@base))));

    public static FjordFact AttributeOfFact(FjordFact target, string attribute) =>
        new(AttributeOf, FjordValue.Rec(
            FjordValue.Of(FjordRef.To(target)),
            FjordValue.Of(attribute)));

    public static FjordFact ImportFact(FjordFact from, FjordFact to) =>
        new(Import, FjordValue.Rec(
            FjordValue.Of(FjordRef.To(from)),
            FjordValue.Of(FjordRef.To(to))));

    public static FjordFact ProjectFact(string path) =>
        new(Project, FjordValue.Of(path));

    public static FjordFact AssemblyFact(string name) =>
        new(Assembly, FjordValue.Of(name));

    public static FjordFact CompilationFact(FjordFact assembly, string framework, FjordFact project) =>
        new(Compilation, FjordValue.Rec(
            FjordValue.Of(FjordRef.To(assembly)),
            FjordValue.Of(framework),
            FjordValue.Of(FjordRef.To(project))));

    public static FjordFact ProjectSourceFact(FjordFact file, FjordFact project) =>
        new(ProjectSource, FjordValue.Rec(
            FjordValue.Of(FjordRef.To(file)),
            FjordValue.Of(FjordRef.To(project))));

    public static FjordFact ProjectRefFact(FjordFact from, FjordFact to) =>
        new(ProjectRef, FjordValue.Rec(
            FjordValue.Of(FjordRef.To(from)),
            FjordValue.Of(FjordRef.To(to))));

    public static FjordFact PackageFact(string name, string version) =>
        new(Package, FjordValue.Rec(
            FjordValue.Of(name),
            FjordValue.Of(version)));

    public static FjordFact PackageRefFact(FjordFact package, FjordFact project) =>
        new(PackageRef, FjordValue.Rec(
            FjordValue.Of(FjordRef.To(package)),
            FjordValue.Of(FjordRef.To(project))));

    public static FjordFact MemberFact(FjordFact container, FjordFact member) =>
        new(Member, FjordValue.Rec(
            FjordValue.Of(FjordRef.To(container)),
            FjordValue.Of(FjordRef.To(member))));

    public static FjordFact ExtendsFact(FjordFact @base, FjordFact type) =>
        new(Extends, FjordValue.Rec(
            FjordValue.Of(FjordRef.To(@base)),
            FjordValue.Of(FjordRef.To(type))));

    public static FjordFact ImplementsFact(FjordFact iface, FjordFact type) =>
        new(Implements, FjordValue.Rec(
            FjordValue.Of(FjordRef.To(iface)),
            FjordValue.Of(FjordRef.To(type))));

    public static FjordFact OverrideFact(FjordFact @base, FjordFact member) =>
        new(Override, FjordValue.Rec(
            FjordValue.Of(FjordRef.To(@base)),
            FjordValue.Of(FjordRef.To(member))));

    public static FjordFact ParamFact(FjordFact decl, long index, string name, string type) =>
        new(Param,
            FjordValue.Rec(
                FjordValue.Of(FjordRef.To(decl)),
                FjordValue.Of(index),
                FjordValue.Of(name)),
            FjordValue.Of(type));

    public static FjordFact TypeOfFact(FjordFact decl, string type) =>
        new(TypeOf,
            FjordValue.Rec(FjordValue.Of(FjordRef.To(decl))),
            FjordValue.Of(type));

    public static FjordFact DocFact(FjordFact decl, string text) =>
        new(Doc,
            FjordValue.Rec(FjordValue.Of(FjordRef.To(decl))),
            FjordValue.Of(text));

    public static FjordFact AttributeFact(string attribute, FjordFact target) =>
        new(Attribute, FjordValue.Rec(
            FjordValue.Of(attribute),
            FjordValue.Of(FjordRef.To(target))));

    public static FjordFact LineFact(FjordFact file, long line, string text) =>
        new(Line,
            FjordValue.Rec(
                FjordValue.Of(FjordRef.To(file)),
                FjordValue.Of(line)),
            FjordValue.Of(text));
}
