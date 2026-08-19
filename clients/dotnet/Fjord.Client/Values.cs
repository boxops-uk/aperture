using System.Text;

namespace Fjord.Client;

/// <summary>A value in flight, typed against an <see cref="FjordType"/>.</summary>
/// <remarks>
/// <b>A record is positional</b> — a list of values, no names. The schema supplies the
/// names and their order, so there is nothing here to put in the wrong order, which is
/// the type system carrying the codec's central rule.
/// </remarks>
public abstract record FjordValue
{
    public sealed record Int(long Value) : FjordValue;

    public sealed record Str(string Value) : FjordValue;

    public sealed record Ref(FjordRef Value) : FjordValue;

    public sealed record Record(IReadOnlyList<FjordValue> Fields) : FjordValue;

    public static FjordValue Of(long value) => new Int(value);

    public static FjordValue Of(string value) => new Str(value);

    public static FjordValue Of(FjordRef value) => new Ref(value);

    public static FjordValue Rec(params FjordValue[] fields) => new Record(fields);
}

/// <summary>
/// How a reference travels: as an id, or as the fact it names.
/// </summary>
/// <remarks>
/// <para>
/// <b>Nesting is the point of this client.</b> An indexer walking a syntax tree knows
/// the file when it reaches the declaration; every id-based alternative would make it
/// keep a map from each entity to an identity the server assigned, plus an emission
/// order that respects one. Sending the target itself means keeping no book at all —
/// the server interns it and substitutes the id.
/// </para>
/// <para>
/// <see cref="Id"/> is there for a producer that <i>does</i> hold ids: a deriver
/// reading them back from an earlier write, or an incremental writer.
/// </para>
/// </remarks>
public abstract record FjordRef
{
    public sealed record Id(ulong FactId) : FjordRef;

    public sealed record Nested(FjordFact Fact) : FjordRef;

    public static FjordRef To(FjordFact fact) => new Nested(fact);

    public static FjordRef ById(ulong id) => new Id(id);
}

/// <summary>A fact: its predicate, its key, and its value side if the predicate has one.</summary>
/// <remarks>
/// The predicate is carried for the caller and is <b>not encoded</b>: a top-level fact
/// takes it from the block header, and a nested one from the field's declared target,
/// so writing it into the fact as well would be a second source of truth a peer could
/// disagree with itself about.
/// </remarks>
public sealed record FjordFact(uint Predicate, FjordValue Key, FjordValue? Value = null);

/// <summary>
/// The value codec: schema-driven, positional, and the only tag it writes is the one
/// choice the schema cannot predict — whether a reference is an id or a nested fact.
/// </summary>
public static class ValueCodec
{
    private const ulong RefId = 0;
    private const ulong RefNested = 1;

    public static void WriteFact(IBufferSink sink, FjordSchema schema, FjordFact fact)
    {
        var declared = schema[fact.Predicate];

        WriteValue(sink, schema, declared.Key, fact.Key);

        switch (declared.Value, fact.Value)
        {
            case (null, null):
                // No presence flag: the schema says whether there is a value side.
                break;

            case ({ } type, { } value):
                WriteValue(sink, schema, type, value);
                break;

            case ({ }, null):
                throw new FjordProtocolException(
                    $"`{declared.Name}` declares a value side and this fact has none");

            case (null, { }):
                throw new FjordProtocolException(
                    $"`{declared.Name}` declares no value side and this fact has one");
        }
    }

    public static void WriteValue(
        IBufferSink sink,
        FjordSchema schema,
        FjordType type,
        FjordValue value)
    {
        switch (type, value)
        {
            case (FjordType.Int, FjordValue.Int n):
                Varint.WriteSigned(sink, n.Value);
                break;

            // Length-prefixed and raw: no escaping and no terminator, so a blob costs
            // its own size whatever bytes are in it.
            case (FjordType.Str, FjordValue.Str s):
            {
                var utf8 = Encoding.UTF8.GetBytes(s.Value);
                Varint.Write(sink, (ulong)utf8.Length);
                sink.Write(utf8);
                break;
            }

            case (FjordType.Fact fact, FjordValue.Ref reference):
                WriteRef(sink, schema, fact.Predicate, reference.Value);
                break;

            case (FjordType.Record declared, FjordValue.Record record):
            {
                if (declared.Fields.Count != record.Fields.Count)
                {
                    throw new FjordProtocolException(
                        $"record has {record.Fields.Count} fields, the schema declares {declared.Fields.Count}");
                }

                // Concatenation, and that is the whole of it.
                for (var index = 0; index < declared.Fields.Count; index++)
                {
                    WriteValue(sink, schema, declared.Fields[index].Type, record.Fields[index]);
                }
                break;
            }

            default:
                throw new FjordProtocolException(
                    $"value {value.GetType().Name} does not fit type {type.GetType().Name}");
        }
    }

    private static void WriteRef(
        IBufferSink sink,
        FjordSchema schema,
        uint target,
        FjordRef reference)
    {
        switch (reference)
        {
            case FjordRef.Id id:
                // The id's own top bits name its predicate, so a reference aimed at
                // the wrong one is catchable here rather than at the far end.
                var tag = (uint)(id.FactId >> 40);
                if (tag != target)
                {
                    throw new FjordProtocolException(
                        $"reference names predicate {tag}, the field declares {target}");
                }

                Varint.Write(sink, RefId);
                Varint.Write(sink, id.FactId);
                break;

            case FjordRef.Nested nested:
                if (nested.Fact.Predicate != target)
                {
                    throw new FjordProtocolException(
                        $"nested fact is of predicate {nested.Fact.Predicate}, the field declares {target}");
                }

                Varint.Write(sink, RefNested);
                WriteFact(sink, schema, nested.Fact);
                break;

            default:
                throw new FjordProtocolException("unknown reference form");
        }
    }

    /// <summary>Read a value of <paramref name="type"/>, advancing <paramref name="at"/>.</summary>
    public static FjordValue ReadValue(
        ReadOnlySpan<byte> bytes,
        FjordSchema schema,
        FjordType type,
        ref int at)
    {
        switch (type)
        {
            case FjordType.Int:
                return new FjordValue.Int(Varint.ReadSigned(bytes, ref at));

            case FjordType.Str:
            {
                var length = Varint.Read(bytes, ref at);
                if (length > (ulong)(bytes.Length - at))
                {
                    throw new FjordProtocolException("string runs past the end of the payload");
                }

                var text = Encoding.UTF8.GetString(bytes.Slice(at, (int)length));
                at += (int)length;
                return new FjordValue.Str(text);
            }

            case FjordType.Fact fact:
            {
                var form = Varint.Read(bytes, ref at);

                return form switch
                {
                    RefId => new FjordValue.Ref(new FjordRef.Id(Varint.Read(bytes, ref at))),
                    RefNested => new FjordValue.Ref(
                        new FjordRef.Nested(ReadFact(bytes, schema, fact.Predicate, ref at))),
                    _ => throw new FjordProtocolException($"unknown reference form {form}"),
                };
            }

            case FjordType.Record record:
            {
                var fields = new List<FjordValue>(record.Fields.Count);
                foreach (var (_, field) in record.Fields)
                {
                    fields.Add(ReadValue(bytes, schema, field, ref at));
                }
                return new FjordValue.Record(fields);
            }

            default:
                throw new FjordProtocolException($"unknown type {type}");
        }
    }

    public static FjordFact ReadFact(
        ReadOnlySpan<byte> bytes,
        FjordSchema schema,
        uint predicate,
        ref int at)
    {
        var declared = schema[predicate];
        var key = ReadValue(bytes, schema, declared.Key, ref at);
        var value = declared.Value is { } type ? ReadValue(bytes, schema, type, ref at) : null;
        return new FjordFact(predicate, key, value);
    }
}
