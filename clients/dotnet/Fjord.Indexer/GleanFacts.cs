using System.Text.Encodings.Web;
using System.Text.Json;

using Fjord.Client;

namespace Fjord.Indexer;

/// <summary>
/// The same facts, in Glean's JSON batch format.
/// </summary>
/// <remarks>
/// <para>
/// <b>Why this exists.</b> The comparison this repository keeps making against Glean is
/// made out of documents — what each system can be asked, what it spends, what it
/// charges (<c>docs/glean-capabilities.md</c>). A number needs the same corpus in both
/// systems, and the same corpus needs the same producer: one Roslyn walk, one set of
/// facts, two sinks. This is the second sink.
/// </para>
/// <para>
/// <b>JSON, because JSON is Glean's own indexer protocol.</b> Glean's external-indexer
/// driver runs a program, hands it a directory in <c>$JSON_BATCH_DIR</c>, and then loads
/// every file it finds there with a concurrency of twenty
/// (<c>glean/index/Glean/Indexer/External.hs</c>). Writing that directory is therefore
/// the sanctioned way in, not a shortcut around one: the binary alternative is a Thrift
/// <c>Batch</c> of locally-numbered facts, which would mean re-implementing Glean's fact
/// numbering in C# and would measure our copy of it rather than theirs.
/// </para>
/// <para>
/// <b>References stay nested here too.</b> Glean's JSON reader accepts a fact reference
/// as an id, as <c>{"id": N}</c>, or as the whole target fact — and an anonymous nested
/// fact is deduplicated against the batch and against the database exactly as interning
/// does on our side (<c>glean/db/Glean/Write/JSON.hs</c>). So the producer holds no ids
/// on this path either, and the two write paths are being asked the same question:
/// resolve-or-create, bottom-up, for every reference in the corpus.
/// </para>
/// <para>
/// <b>One substitution, and it is checked.</b> Angle has no signed integer, so every
/// <c>int</c> in <c>schemas/code.sigla</c> is a <c>nat</c> in <c>fjbench.angle</c>. Every
/// one of them is a line, a column, an end position, a length or a parameter index —
/// non-negative by construction — and <see cref="WriteValue"/> throws rather than
/// assuming it, because the one corpus that violates this (the demo's parameter at
/// index -1, which exists to exercise zigzag) must not silently become something else.
/// </para>
/// <para>
/// <b>What this does not do.</b> Emitting is not writing: a file of JSON is a fact
/// nobody has interned. The load is a second step and a second number
/// (<c>clients/dotnet/index-repo-glean.sh</c>), and the report at the end of a run says
/// so, because the honest total for this side of the comparison is emit plus load.
/// </para>
/// </remarks>
internal static class GleanFacts
{
    /// <summary>
    /// The Angle schema these facts are written against.
    /// </summary>
    /// <remarks>
    /// Not <c>src</c>: Glean ships its own <c>src.1</c> which also declares
    /// <c>predicate File : string</c>, so a corpus in that namespace would collide with
    /// Glean's the moment the two were loaded together. The predicate names are otherwise
    /// identical — <c>fjbench.Decl</c> is <c>src.Decl</c> — and so are the field orders,
    /// which on both systems are the key's byte order and therefore the index design.
    /// </remarks>
    public const string Namespace = "fjbench";

    /// <summary>The version every predicate in that schema carries.</summary>
    public const int Version = 1;

    /// <summary>The namespace the Fjord side states its predicates in.</summary>
    private const string FjordNamespace = "src.";

    /// <summary>How the batch files are written.</summary>
    /// <remarks>
    /// <b>The relaxed encoder, deliberately.</b> The default escapes every non-ASCII
    /// character as <c>\uXXXX</c>, which for a line table of real source is a large
    /// multiple of the bytes and buys nothing: the reader at the far end is folly's JSON
    /// parser, not a browser or an HTML attribute.
    /// </remarks>
    public static readonly JsonWriterOptions WriterOptions = new()
    {
        Encoder = JavaScriptEncoder.UnsafeRelaxedJsonEscaping,
        Indented = false,
    };

    /// <summary>
    /// <c>src.Decl</c> becomes <c>fjbench.Decl.1</c>.
    /// </summary>
    /// <remarks>
    /// Derived rather than tabulated. A second hand-written name table would be a second
    /// thing to keep in step with <c>fjbench.angle</c>, and the translation it would
    /// state is mechanical — the schema was written to preserve every name.
    /// </remarks>
    public static string PredicateName(string fjordName) =>
        fjordName.StartsWith(FjordNamespace, StringComparison.Ordinal)
            ? $"{Namespace}.{fjordName[FjordNamespace.Length..]}.{Version}"
            : throw new InvalidOperationException(
                $"`{fjordName}` is not in the `{FjordNamespace}` schema, so there is "
                + $"no `{Namespace}` predicate to write it as");

    /// <summary>Write one block as a whole batch file: a JSON array of one item.</summary>
    /// <remarks>
    /// A file is a complete document because that is the unit Glean's loader takes — it
    /// reads a file, parses it whole, and sends it as one batch. One predicate per file
    /// keeps the file the same thing the block is, so a batch that fails to load names
    /// the predicate that broke it.
    /// </remarks>
    public static void WriteBatch(
        Utf8JsonWriter json,
        FjordSchema schema,
        uint predicate,
        IReadOnlyList<FjordFact> facts)
    {
        json.WriteStartArray();
        json.WriteStartObject();
        json.WriteString("predicate", PredicateName(schema[predicate].Name));
        json.WritePropertyName("facts");
        json.WriteStartArray();

        foreach (var fact in facts)
        {
            WriteFact(json, schema, fact);
        }

        json.WriteEndArray();
        json.WriteEndObject();
        json.WriteEndArray();
    }

    /// <summary>
    /// A fact: <c>{"key": …}</c>, and <c>"value"</c> when the predicate has a value side.
    /// </summary>
    /// <remarks>
    /// The same function serves a top-level fact and a nested one, which is what keeps
    /// the value side right: a nested <c>src.Decl</c> carries its kind, because the
    /// nested target <i>is</i> the fact the producer built and Glean would otherwise
    /// intern a key with an empty value and then refuse the real one as a redefinition.
    /// </remarks>
    private static void WriteFact(Utf8JsonWriter json, FjordSchema schema, FjordFact fact)
    {
        var declared = schema[fact.Predicate];

        json.WriteStartObject();
        json.WritePropertyName("key");
        WriteValue(json, schema, declared.Key, fact.Key);

        switch (declared.Value, fact.Value)
        {
            case (null, null):
                break;

            case ({ } type, { } value):
                json.WritePropertyName("value");
                WriteValue(json, schema, type, value);
                break;

            case ({ }, null):
                throw new InvalidOperationException(
                    $"`{declared.Name}` declares a value side and this fact has none");

            case (null, { }):
                throw new InvalidOperationException(
                    $"`{declared.Name}` declares no value side and this fact has one");
        }

        json.WriteEndObject();
    }

    private static void WriteValue(
        Utf8JsonWriter json,
        FjordSchema schema,
        FjordType type,
        FjordValue value)
    {
        switch (type, value)
        {
            case (FjordType.Int, FjordValue.Int number):
                if (number.Value < 0)
                {
                    throw new InvalidOperationException(
                        $"{number.Value} cannot be written as a Glean `nat`; this schema's "
                        + "`int` fields are positions and lengths, and a negative one means "
                        + "the corpus is not the one fjbench.angle was translated for");
                }

                json.WriteNumberValue(number.Value);
                break;

            case (FjordType.Str, FjordValue.Str text):
                json.WriteStringValue(Utf8Safe(text.Value));
                break;

            case (FjordType.Fact declared, FjordValue.Ref reference):
                WriteRef(json, schema, declared.Predicate, reference.Value);
                break;

            case (FjordType.Record declared, FjordValue.Record record):
            {
                if (declared.Fields.Count != record.Fields.Count)
                {
                    throw new InvalidOperationException(
                        $"record has {record.Fields.Count} fields, the schema declares "
                        + $"{declared.Fields.Count}");
                }

                // Named here, positional on the Fjord wire — the same field order
                // either way, since the schema is what supplies it.
                json.WriteStartObject();
                for (var index = 0; index < declared.Fields.Count; index++)
                {
                    json.WritePropertyName(declared.Fields[index].Name);
                    WriteValue(json, schema, declared.Fields[index].Type, record.Fields[index]);
                }

                json.WriteEndObject();
                break;
            }

            default:
                throw new InvalidOperationException(
                    $"value {value.GetType().Name} does not fit type {type.GetType().Name}");
        }
    }

    private static void WriteRef(
        Utf8JsonWriter json,
        FjordSchema schema,
        uint target,
        FjordRef reference)
    {
        switch (reference)
        {
            case FjordRef.Nested nested:
                if (nested.Fact.Predicate != target)
                {
                    throw new InvalidOperationException(
                        $"nested fact is of predicate {nested.Fact.Predicate}, the field "
                        + $"declares {target}");
                }

                WriteFact(json, schema, nested.Fact);
                break;

            case FjordRef.Id id:
                // Glean's JSON reader does take {"id": N}, but the N would have to be a
                // Glean fact id: dense, database-wide, and assigned by the write this
                // file is an input to. A Fjord id is none of those — its top bits
                // name its predicate — so writing one would produce a file that loads
                // and points somewhere arbitrary. This producer holds no ids, so the
                // case is unreachable and says so rather than guessing.
                throw new InvalidOperationException(
                    $"reference to Fjord fact id {id.FactId} cannot be written as a Glean "
                    + "reference: Glean assigns its own ids at write time, and this producer "
                    + "is supposed to nest its targets");

            default:
                throw new InvalidOperationException("unknown reference form");
        }
    }

    /// <summary>The same string, with any unpaired surrogate replaced by U+FFFD.</summary>
    /// <remarks>
    /// A .NET string is UTF-16 and Roslyn hands over whatever the file contained, so a
    /// lone surrogate is reachable from real source — a broken byte sequence in a comment
    /// is enough. <c>Encoding.UTF8.GetBytes</c>, which the Fjord codec uses,
    /// substitutes U+FFFD for one silently; <see cref="Utf8JsonWriter"/> refuses to
    /// transcode it. Substituting the same character keeps the two corpora the same
    /// corpus, rather than having one of them die on a file the other took.
    /// <para>
    /// The scan allocates nothing in the common case, which matters: on a full index this
    /// runs once per string in eight and a half million line facts.
    /// </para>
    /// </remarks>
    private static string Utf8Safe(string text)
    {
        for (var index = 0; index < text.Length; index++)
        {
            if (!char.IsSurrogate(text[index]))
            {
                continue;
            }

            if (char.IsHighSurrogate(text[index])
                && index + 1 < text.Length
                && char.IsLowSurrogate(text[index + 1]))
            {
                index++;
                continue;
            }

            return Repaired(text, index);
        }

        return text;
    }

    private static string Repaired(string text, int from)
    {
        var chars = text.ToCharArray();

        for (var index = from; index < chars.Length; index++)
        {
            if (!char.IsSurrogate(chars[index]))
            {
                continue;
            }

            if (char.IsHighSurrogate(chars[index])
                && index + 1 < chars.Length
                && char.IsLowSurrogate(chars[index + 1]))
            {
                index++;
                continue;
            }

            chars[index] = '\uFFFD';
        }

        return new string(chars);
    }
}

/// <summary>One block, written as one Glean JSON batch file.</summary>
/// <remarks>
/// <para>
/// <b>A file per block, not a file per run.</b> Glean's loader parses a whole file into
/// one batch before sending it, so a multi-gigabyte file would be a multi-gigabyte parse
/// and one enormous batch; a file per block keeps each batch the size the sink already
/// chose with <c>--batch</c>, and gives the loader independent units it can write
/// concurrently.
/// </para>
/// <para>
/// <b>One of these per writer thread, and nothing shared.</b> The sequence number in the
/// file name is this target's own, and the writer number keeps two targets from picking
/// the same path — so no lock, and no ordering claim either: the names are for a human
/// reading a failure, and the loader does not care which order it takes them in.
/// </para>
/// </remarks>
internal sealed class GleanTarget(FjordSchema schema, string directory, int writer) : IBlockTarget
{
    private int _sequence;

    /// <summary>Files this target has written.</summary>
    public int Files => _sequence;

    public BlockWritten Write(uint predicate, IReadOnlyList<FjordFact> facts)
    {
        var path = Path.Combine(
            directory,
            $"w{writer}-{_sequence++:D6}-{GleanFacts.PredicateName(schema[predicate].Name)}.json");

        using var file = new FileStream(
            path, FileMode.CreateNew, FileAccess.Write, FileShare.None, 1 << 16);
        using var json = new Utf8JsonWriter(file, GleanFacts.WriterOptions);

        GleanFacts.WriteBatch(json, schema, predicate, facts);
        json.Flush();

        return new BlockWritten(0, 0, json.BytesCommitted);
    }

    public void Dispose()
    {
    }
}
