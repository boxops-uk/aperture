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

    /// <summary>Every predicate id, in schema order — what a report iterates.</summary>
    public static readonly uint[] Predicates = [File, Module, Decl, SearchByName, Ref, Import];

    public static readonly ApertureSchema Schema = new([
        new AperturePredicate("src.File", ApertureType.String, null),

        new AperturePredicate("src.Module", ApertureType.Rec(
            ("file", ApertureType.Reference(File)),
            ("name", ApertureType.String)), null),

        // The value side is the declaration's kind. A value cannot be matched on (I6),
        // which is what makes it the right home for something a query reads but never
        // filters by.
        new AperturePredicate("src.Decl", ApertureType.Rec(
            ("line", ApertureType.Integer),
            ("module", ApertureType.Reference(Module)),
            ("name", ApertureType.String)), ApertureType.String),

        new AperturePredicate("src.SearchByName", ApertureType.Rec(
            ("name", ApertureType.String),
            ("to", ApertureType.Reference(Decl))), null),

        new AperturePredicate("src.Ref", ApertureType.Rec(
            ("at", ApertureType.Rec(
                ("col", ApertureType.Integer),
                ("line", ApertureType.Integer))),
            ("file", ApertureType.Reference(File)),
            ("to", ApertureType.Reference(Decl))), null),

        new AperturePredicate("src.Import", ApertureType.Rec(
            ("from", ApertureType.Reference(Module)),
            ("to", ApertureType.Reference(Module))), null),
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
                ApertureValue.Of(line),
                ApertureValue.Of(ApertureRef.To(module)),
                ApertureValue.Of(name)),
            ApertureValue.Of(kind));

    public static ApertureFact SearchFact(string name, ApertureFact decl) =>
        new(SearchByName, ApertureValue.Rec(
            ApertureValue.Of(name),
            ApertureValue.Of(ApertureRef.To(decl))));

    public static ApertureFact RefFact(long line, long col, ApertureFact file, ApertureFact decl) =>
        new(Ref, ApertureValue.Rec(
            ApertureValue.Rec(ApertureValue.Of(col), ApertureValue.Of(line)),
            ApertureValue.Of(ApertureRef.To(file)),
            ApertureValue.Of(ApertureRef.To(decl))));

    public static ApertureFact ImportFact(ApertureFact from, ApertureFact to) =>
        new(Import, ApertureValue.Rec(
            ApertureValue.Of(ApertureRef.To(from)),
            ApertureValue.Of(ApertureRef.To(to))));
}
