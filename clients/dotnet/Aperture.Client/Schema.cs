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
/// The schema a client writes against.
/// </summary>
/// <remarks>
/// A predicate's <b>id is its position</b>, so the order here is the wire contract and
/// not a presentation choice.
/// </remarks>
public sealed class ApertureSchema(IReadOnlyList<AperturePredicate> predicates)
{
    public IReadOnlyList<AperturePredicate> Predicates { get; } = predicates;

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

    /// <summary>
    /// The provisional schema fingerprint, matching
    /// <c>aperture_server::protocol::provisional_fingerprint</c>.
    /// </summary>
    /// <remarks>
    /// <para>
    /// FNV-1a over predicate names and types. This is <b>not</b> the schema identity
    /// chapter 6 specifies — that is canonical form plus per-predicate fingerprints,
    /// and it arrives with Phase 8's schema parsing. It exists so that a client and a
    /// server disagreeing about the schema find out at the handshake.
    /// </para>
    /// <para>
    /// When Phase 8 lands, the server's protocol version bumps and this goes with it.
    /// A client that wants no opinion sends <c>0</c>, which means "do not check".
    /// </para>
    /// </remarks>
    public ulong Fingerprint()
    {
        const ulong offset = 0xcbf29ce484222325;
        var hash = offset;

        // **By name, in name order — not in the order this schema was written.** The
        // server's schema comes from parsing a `.aps` file, whose predicates are sorted
        // by name; the list above is in whatever order reads well. Hashing the
        // declaration order made the two disagree about a schema they share, which is
        // what this sort exists to prevent. `Ordinal` because the server compares UTF-8
        // bytes and has no notion of a culture.
        foreach (var predicate in Predicates.OrderBy(p => p.Name, StringComparer.Ordinal))
        {
            Feed(ref hash, predicate.Name);
            FeedType(ref hash, predicate.Key);

            if (predicate.Value is { } value)
            {
                Feed(ref hash, "+value");
                FeedType(ref hash, value);
            }
            else
            {
                Feed(ref hash, "-value");
            }
        }

        // Never zero: zero is the client's "do not check", so a schema that happened
        // to hash to it would silently disable the check for everyone.
        return hash == 0 ? 1 : hash;
    }

    private static void Feed(ref ulong hash, string text) =>
        Feed(ref hash, System.Text.Encoding.UTF8.GetBytes(text));

    private static void Feed(ref ulong hash, ReadOnlySpan<byte> bytes)
    {
        const ulong prime = 0x00000100000001B3;

        foreach (var b in bytes)
        {
            hash ^= b;
            hash = unchecked(hash * prime);
        }
    }

    private void FeedType(ref ulong hash, ApertureType type)
    {
        switch (type)
        {
            case ApertureType.Int:
                Feed(ref hash, "int");
                break;

            case ApertureType.Str:
                Feed(ref hash, "str");
                break;

            case ApertureType.Fact fact:
                Feed(ref hash, "fact");
                // **By name, not by id.** An id is a position, and since Phase 8 the
                // server sorts a schema's names to get one — so two ends that agree
                // about every predicate can still number them differently, and hashing
                // the number would make them disagree about a schema they share.
                Feed(ref hash, NameOf(fact.Predicate));
                break;

            case ApertureType.Record record:
                Feed(ref hash, "record");
                Span<byte> count = stackalloc byte[8];
                System.Buffers.Binary.BinaryPrimitives.WriteUInt64LittleEndian(count, (ulong)record.Fields.Count);
                Feed(ref hash, count);
                foreach (var (name, field) in record.Fields)
                {
                    Feed(ref hash, name);
                    FeedType(ref hash, field);
                }
                break;

            default:
                throw new ApertureProtocolException($"unknown type {type}");
        }
    }
}
