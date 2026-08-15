namespace Aperture.Client;

/// <summary>Somewhere bytes are appended to. Kept minimal so the codec has one seam.</summary>
public interface IBufferSink
{
    void WriteByte(byte value);
    void Write(ReadOnlySpan<byte> bytes);
}

/// <summary>A growable byte buffer.</summary>
public sealed class ByteBuffer : IBufferSink
{
    private byte[] _bytes = new byte[256];

    public int Length { get; private set; }

    public ReadOnlySpan<byte> Span => _bytes.AsSpan(0, Length);

    public byte[] ToArray() => Span.ToArray();

    public void Clear() => Length = 0;

    public void WriteByte(byte value)
    {
        EnsureRoom(1);
        _bytes[Length++] = value;
    }

    public void Write(ReadOnlySpan<byte> bytes)
    {
        EnsureRoom(bytes.Length);
        bytes.CopyTo(_bytes.AsSpan(Length));
        Length += bytes.Length;
    }

    /// <summary>Overwrite four bytes already written — used to backfill a length or a checksum.</summary>
    public Span<byte> SliceAt(int offset, int length) => _bytes.AsSpan(offset, length);

    private void EnsureRoom(int extra)
    {
        if (Length + extra <= _bytes.Length)
        {
            return;
        }

        var grown = _bytes.Length;
        while (grown < Length + extra)
        {
            grown *= 2;
        }

        Array.Resize(ref _bytes, grown);
    }
}
