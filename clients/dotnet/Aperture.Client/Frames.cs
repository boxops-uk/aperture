using System.Buffers.Binary;
using System.Net.Sockets;

namespace Aperture.Client;

/// <summary>What a frame carries. A byte, not a closed enum — see <see cref="FrameIo"/>.</summary>
public static class FrameKind
{
    public const byte Startup = (byte)'S';
    public const byte Ready = (byte)'R';
    public const byte OpenWrite = (byte)'W';
    public const byte CopyInResponse = (byte)'G';
    public const byte CopyData = (byte)'d';
    public const byte CopyDone = (byte)'c';
    public const byte Query = (byte)'Q';
    public const byte RowDescription = (byte)'T';
    public const byte DataRow = (byte)'D';
    public const byte Complete = (byte)'C';
    public const byte Error = (byte)'E';
}

/// <summary>A frame as it arrived.</summary>
public sealed record Frame(byte Kind, uint Stream, byte[] Payload);

/// <summary>
/// The frame layer: <c>[kind u8][stream u32][length u32][payload]</c>, little-endian.
/// </summary>
/// <remarks>
/// The <c>stream</c> field is what departs from PostgreSQL, and it is the reason for
/// departing: PG's model is strictly serial, so a long query blocks a short one behind
/// it. Here a query is a stream and a write is a stream, and a frame says which.
/// <para>
/// An unrecognised kind is <b>returned, not rejected</b>. A framing layer delimits; it
/// does not interpret. Refusing here would leave a client unable to skip a frame it did
/// not understand, which is the one thing the length is for.
/// </para>
/// </remarks>
public static class FrameIo
{
    public const int HeaderLength = 1 + 4 + 4;
    public const uint MaxPayload = Block.MaxPayload;

    public static void Write(Stream stream, byte kind, uint streamId, ReadOnlySpan<byte> payload)
    {
        if (payload.Length > MaxPayload)
        {
            throw new ApertureProtocolException(
                $"{payload.Length} payload bytes exceeds the maximum of {MaxPayload}");
        }

        Span<byte> header = stackalloc byte[HeaderLength];
        header[0] = kind;
        BinaryPrimitives.WriteUInt32LittleEndian(header[1..], streamId);
        BinaryPrimitives.WriteUInt32LittleEndian(header[5..], (uint)payload.Length);

        stream.Write(header);
        stream.Write(payload);
        stream.Flush();
    }

    public static Frame Read(Stream stream)
    {
        var header = ReadExactly(stream, HeaderLength);

        var kind = header[0];
        var streamId = BinaryPrimitives.ReadUInt32LittleEndian(header.AsSpan(1));
        var length = BinaryPrimitives.ReadUInt32LittleEndian(header.AsSpan(5));

        // A length sizes an allocation and came from the peer.
        if (length > MaxPayload)
        {
            throw new ApertureProtocolException(
                $"frame declares {length} payload bytes, past the maximum of {MaxPayload}");
        }

        return new Frame(kind, streamId, ReadExactly(stream, (int)length));
    }

    private static byte[] ReadExactly(Stream stream, int count)
    {
        var buffer = new byte[count];
        var filled = 0;

        while (filled < count)
        {
            var read = stream.Read(buffer, filled, count - filled);
            if (read == 0)
            {
                throw new ApertureProtocolException(
                    $"the connection closed {filled} bytes into a {count}-byte read");
            }
            filled += read;
        }

        return buffer;
    }
}
