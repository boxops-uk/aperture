using Boxops.Fjord.Client;

namespace Boxops.Fjord.Indexer;

/// <summary>
/// Where a full block goes, once the walk has let go of it.
/// </summary>
/// <remarks>
/// <para>
/// <b>Why there is a seam here at all.</b> Everything above this interface — the Roslyn
/// walk, the per-predicate batching, the bounded queue, the writer threads — is a
/// statement about how an <i>indexer</i> produces facts, and none of it is a statement
/// about Fjord. Pointing the same walk at a different fact database is then one
/// method, which is what makes a measurement of the two a measurement of the databases
/// rather than of two indexers that happen to read the same source.
/// </para>
/// <para>
/// <b>The unit is a block of one predicate</b>, because that is what both ends want: an
/// Fjord write stream carries one predicate per <c>CopyData</c> frame, and a Glean
/// JSON batch is a list of <c>{predicate, facts}</c> items. Nothing here holds a fact
/// id — every reference is the target fact nested inline — so a target may write its
/// blocks in any order, and several targets may write at once.
/// </para>
/// </remarks>
internal interface IBlockTarget : IDisposable
{
    /// <summary>Write one block, and say what that did.</summary>
    BlockWritten Write(uint predicate, IReadOnlyList<FjordFact> facts);
}

/// <summary>What one block's write did.</summary>
/// <remarks>
/// <see cref="Created"/> and <see cref="Deduped"/> are a <i>database's</i> answer, so
/// only a target talking to one can fill them in; a target that writes files leaves them
/// zero, because nothing has been interned yet and saying so is better than implying a
/// number. <see cref="Bytes"/> is what this target actually wrote, and is zero for a
/// target that does not know — the Fjord client encodes inside itself and does not
/// report a size.
/// </remarks>
internal readonly record struct BlockWritten(ulong Created, ulong Deduped, long Bytes);

/// <summary>A block written down one Fjord connection.</summary>
/// <remarks>
/// The connection outlives this: <c>Program</c> closes every one of them when the run
/// ends, however it ends, so this is a way of pointing the sink at a connection and not
/// a claim of ownership over it.
/// </remarks>
internal sealed class FjordTarget(FjordConnection connection) : IBlockTarget
{
    public BlockWritten Write(uint predicate, IReadOnlyList<FjordFact> facts)
    {
        var summary = connection.Write(predicate, facts);
        return new BlockWritten(summary.Created, summary.Deduped, 0);
    }

    public void Dispose()
    {
    }
}
