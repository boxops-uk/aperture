namespace Aperture.Client;

/// <summary>
/// CRC-32 (IEEE 802.3) — the same polynomial zlib, PNG, gzip and Avro use, and the
/// one <c>aperture-wire::crc</c> computes, so a block checksummed here verifies there.
/// </summary>
public static class Crc32
{
    private const uint Polynomial = 0xEDB88320;
    private static readonly uint[] Table = BuildTable();

    private static uint[] BuildTable()
    {
        var table = new uint[256];

        for (var index = 0; index < table.Length; index++)
        {
            var value = (uint)index;
            for (var bit = 0; bit < 8; bit++)
            {
                value = (value & 1) == 1 ? (value >> 1) ^ Polynomial : value >> 1;
            }
            table[index] = value;
        }

        return table;
    }

    public static uint Compute(ReadOnlySpan<byte> bytes) => Finish(Update(Start, bytes));

    public const uint Start = 0xFFFFFFFF;

    public static uint Update(uint crc, ReadOnlySpan<byte> bytes)
    {
        foreach (var b in bytes)
        {
            crc = Table[(crc ^ b) & 0xFF] ^ (crc >> 8);
        }
        return crc;
    }

    public static uint Finish(uint crc) => crc ^ 0xFFFFFFFF;
}
