using System.Buffers.Binary;

namespace Aperture.Client;

/// <summary>
/// A <b>block</b>: a run of facts of one predicate, behind a sync marker and a
/// checksummed header. The same bytes a fact file holds and a <c>CopyData</c> frame
/// carries.
/// </summary>
/// <remarks>
/// <para>
/// <c>[sync FF x 10][magic "APBK"][nameLen u32][count u32][length u32][crc32 u32][name][payload]</c>
/// </para>
/// <para>
/// <b>The predicate is named, not numbered.</b> The header carried the database's
/// numeric id until Phase 8, which meant a client had to keep a table of ids in step
/// with a server's and a fact file was only meaningful against the database whose
/// numbering wrote it. A name costs about six more bytes <i>once per block</i>, against
/// payloads of hundreds to thousands of facts, and buys both back. The name sits after
/// the fixed-width fields so a splitter still reaches <c>length</c> at a fixed offset,
/// and it cannot contribute to a sync marker for the same reason a string cannot: UTF-8
/// never uses <c>0xF8</c>–<c>0xFF</c>.
/// </para>
/// <para>
/// The ten <c>0xFF</c> bytes are a marker this encoding <i>cannot otherwise
/// produce</i>: UTF-8 never uses <c>0xF8</c>–<c>0xFF</c>, and a varint's last byte is
/// below <c>0x80</c> so a run ends where the varint does — the longest reachable is
/// nine. That is what lets a fact file be split at an arbitrary offset. A client does
/// not have to care, but a client that <i>emitted</i> a marker inside a payload would
/// break it, which is why the encoding here has to match rather than merely decode.
/// </para>
/// <para>
/// Header fields are little-endian; the checksum covers the header's own fields as
/// well as the payload, so a corrupted length is caught rather than trusted.
/// </para>
/// </remarks>
public static class Block
{
    public static ReadOnlySpan<byte> Sync => [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];

    public static ReadOnlySpan<byte> Magic => "APBK"u8;

    /// <summary>Magic, name length, count, length, checksum.</summary>
    public const int HeaderLength = 4 + 4 + 4 + 4 + 4;

    /// <summary>Framing per block: the marker and the header.</summary>
    public const int Overhead = 10 + HeaderLength;

    public const uint MaxFacts = 0x00FFFFFF;
    public const uint MaxPayload = 0x04000000;

    /// <summary>The longest predicate name a block may carry.</summary>
    public const uint MaxName = 0x0000FFFF;

    /// <summary>Encode <paramref name="facts"/>, all of <paramref name="predicate"/>, as one block.</summary>
    public static byte[] Encode(ApertureSchema schema, uint predicate, IReadOnlyList<ApertureFact> facts)
    {
        if (facts.Count > MaxFacts)
        {
            throw new ApertureProtocolException($"{facts.Count} facts exceeds the maximum of {MaxFacts}");
        }

        var payload = new ByteBuffer();
        foreach (var fact in facts)
        {
            if (fact.Predicate != predicate)
            {
                throw new ApertureProtocolException(
                    $"a block carries one predicate; this fact is of {fact.Predicate}, not {predicate}");
            }

            ValueCodec.WriteFact(payload, schema, fact);
        }

        if (payload.Length > MaxPayload)
        {
            throw new ApertureProtocolException(
                $"{payload.Length} payload bytes exceeds the maximum of {MaxPayload}");
        }

        // The name comes from the schema this call already takes, so a caller still
        // speaks its own local id and only the wire carries a name.
        var name = System.Text.Encoding.UTF8.GetBytes(schema.NameOf(predicate));
        if (name.Length > MaxName)
        {
            throw new ApertureProtocolException(
                $"{name.Length} name bytes exceeds the maximum of {MaxName}");
        }

        Span<byte> header = stackalloc byte[HeaderLength - 4];
        Magic.CopyTo(header);
        BinaryPrimitives.WriteUInt32LittleEndian(header[4..], (uint)name.Length);
        BinaryPrimitives.WriteUInt32LittleEndian(header[8..], (uint)facts.Count);
        BinaryPrimitives.WriteUInt32LittleEndian(header[12..], (uint)payload.Length);

        // Name as well as payload: a corrupted one would otherwise resolve to a
        // different predicate, or to none.
        var checksum = Crc32.Finish(
            Crc32.Update(Crc32.Update(Crc32.Update(Crc32.Start, header), name), payload.Span));

        var block = new ByteBuffer();
        block.Write(Sync);
        block.Write(header);

        Span<byte> crc = stackalloc byte[4];
        BinaryPrimitives.WriteUInt32LittleEndian(crc, checksum);
        block.Write(crc);
        block.Write(name);
        block.Write(payload.Span);

        return block.ToArray();
    }
}
