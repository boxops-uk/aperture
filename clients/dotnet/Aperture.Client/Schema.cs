namespace Aperture.Client;

/// <summary>
/// A type in the database's schema, mirroring <c>PredicateTy</c>.
/// </summary>
/// <remarks>
/// <para>
/// <b>A client must have the schema, and this is why.</b> The transport codec sends
/// no field names, no type markers and no record arities — the server knows them, the
/// client knows them, and sending what the reader already has is what a
/// transmission-shaped format declines to do. That is Avro's model, and Avro is blunt
/// about the consequence: a schema must always be used in order to read the data.
/// </para>
/// <para>
/// Until schemas are parsed (Phase 8), a client writes its schema down as this
/// structure and asserts it at the handshake with a fingerprint. Getting it wrong is
/// caught there rather than by writing facts nobody can read back.
/// </para>
/// </remarks>
public abstract record ApertureType
{
    public sealed record Int : ApertureType;

    public sealed record Str : ApertureType;

    /// <summary>A reference to a fact of <paramref name="Predicate"/>.</summary>
    public sealed record Fact(uint Predicate) : ApertureType;

    /// <summary>
    /// A record. <b>Fields must be in the schema's declared order</b>, which is sorted
    /// by name — a record's field order is part of its encoding, and values are sent
    /// positionally against it.
    /// </summary>
    public sealed record Record(IReadOnlyList<(string Name, ApertureType Type)> Fields) : ApertureType;

    public static readonly ApertureType Integer = new Int();
    public static readonly ApertureType String = new Str();

    public static ApertureType Reference(uint predicate) => new Fact(predicate);

    public static ApertureType Rec(params (string Name, ApertureType Type)[] fields) =>
        new Record(fields);
}

/// <summary>One predicate: its name, its key type, and its value side if it has one.</summary>
public sealed record AperturePredicate(string Name, ApertureType Key, ApertureType? Value);

/// <summary>
/// The schema a client writes against, and the fingerprint it carries.
/// </summary>
/// <remarks>
/// <para>
/// A predicate's <b>id is its own</b> — a position in this list — and the server's may
/// differ, which is nobody's problem because a block header names the predicate.
/// </para>
/// <para>
/// <b>The fingerprint is carried, not computed.</b> It is a hash over the canonical
/// form chapter 6 specifies, and a second implementation of that in every client is a
/// port every future client pays for and a drift every one of them can cause. Glean
/// does not ask it either: its schema compiler emits the constant and its clients hold
/// it. So: run <c>aperture schema fingerprint</c>, paste the number, and a stale one
/// fails the handshake loudly — which is what the assertion is for. What it asserts is
/// <i>provenance</i>: that this client was written against that schema. That the shapes
/// below are right is the byte-identical golden's claim, and it is the stronger one.
/// </para>
/// </remarks>
public sealed class ApertureSchema(IReadOnlyList<AperturePredicate> predicates, ulong fingerprint)
{
    public IReadOnlyList<AperturePredicate> Predicates { get; } = predicates;

    /// <summary>
    /// The schema fingerprint, as <c>aperture schema fingerprint</c> prints it. Zero
    /// means "do not check" — a reader with no opinion.
    /// </summary>
    public ulong Fingerprint { get; } = fingerprint;

    public AperturePredicate this[uint id] =>
        id < Predicates.Count
            ? Predicates[(int)id]
            : throw new ApertureProtocolException($"no predicate {id} in this schema");

    /// <summary>
    /// The fully-qualified name of a predicate, which is what a block header carries.
    /// </summary>
    /// <remarks>
    /// A client's ids are its <i>own</i> — a position in the list it declares — and the
    /// server's may differ. Naming the predicate on the wire is what makes that nobody's
    /// problem.
    /// </remarks>
    public string NameOf(uint id) => this[id].Name;

    public uint IdOf(string name)
    {
        for (var index = 0; index < Predicates.Count; index++)
        {
            if (Predicates[index].Name == name)
            {
                return (uint)index;
            }
        }

        throw new ApertureProtocolException($"no predicate named `{name}` in this schema");
    }

}
