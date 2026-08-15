namespace Aperture.Client;

/// <summary>
/// LEB128 varints and zigzag, matching <c>aperture-wire</c>'s <c>varint</c> module
/// byte for byte.
/// </summary>
/// <remarks>
/// <para>
/// The transport codec is not order-preserving and carries no type markers, so an
/// integer is a plain LEB128 varint over a zigzag mapping — the same encoding
/// Protocol Buffers' <c>sint64</c> and Avro's <c>long</c> use, and for the same
/// reason both are transmission formats.
/// </para>
/// <para>
/// <b>Minimality is enforced on decode</b>, and that is not defensive tidiness:
/// LEB128 admits padding, so <c>80 00</c> and <c>00</c> both mean zero. A block
/// carries a CRC32 and the same encoding is used on the wire and in a fact file, so
/// "the same facts" has to mean "the same bytes" for a checksum to be worth
/// computing. A client that emitted a non-minimal varint would produce blocks the
/// server rejects; one that accepted them would disagree with the server about what
/// a given fact's bytes are.
/// </para>
/// </remarks>
public static class Varint
{
    /// <summary>The most bytes a 64-bit varint can occupy.</summary>
    public const int MaxLength = 10;

    /// <summary>Map a signed value onto an unsigned one that is small near zero in either direction.</summary>
    public static ulong ZigZag(long value) => (ulong)((value << 1) ^ (value >> 63));

    public static long UnZigZag(ulong bits) => (long)(bits >> 1) ^ -(long)(bits & 1);

    public static void Write(IBufferSink sink, ulong value)
    {
        while (value >= 0x80)
        {
            sink.WriteByte((byte)(value | 0x80));
            value >>= 7;
        }
        sink.WriteByte((byte)value);
    }

    public static void WriteSigned(IBufferSink sink, long value) => Write(sink, ZigZag(value));

    /// <summary>How many bytes <paramref name="value"/> will occupy, without encoding it.</summary>
    public static int Length(ulong value)
    {
        var bytes = 1;
        while (value >= 0x80)
        {
            value >>= 7;
            bytes++;
        }
        return bytes;
    }

    /// <summary>
    /// Read a varint, advancing <paramref name="at"/>.
    /// </summary>
    /// <exception cref="ApertureProtocolException">
    /// If the varint is truncated, overflows 64 bits, or has a shorter equivalent.
    /// </exception>
    public static ulong Read(ReadOnlySpan<byte> bytes, ref int at)
    {
        ulong value = 0;
        var shift = 0;

        for (var index = 0; index < MaxLength; index++)
        {
            if (at + index >= bytes.Length)
            {
                throw new ApertureProtocolException("varint is truncated");
            }

            var b = bytes[at + index];
            var payload = (ulong)(b & 0x7F);

            // The tenth byte carries one usable bit; anything above it would be
            // shifted off the top rather than stored.
            if (shift == 63 && payload > 1)
            {
                throw new ApertureProtocolException("varint overflows 64 bits");
            }

            value |= payload << shift;

            if ((b & 0x80) == 0)
            {
                // A continuation byte whose payload is zero contributes nothing, so
                // the encoding had a shorter equivalent. Index 0 is exempt: a single
                // 0x00 *is* the minimal encoding of zero.
                if (index > 0 && payload == 0)
                {
                    throw new ApertureProtocolException("varint is not minimally encoded");
                }

                at += index + 1;
                return value;
            }

            shift += 7;
        }

        throw new ApertureProtocolException("varint overflows 64 bits");
    }

    public static long ReadSigned(ReadOnlySpan<byte> bytes, ref int at) => UnZigZag(Read(bytes, ref at));
}
