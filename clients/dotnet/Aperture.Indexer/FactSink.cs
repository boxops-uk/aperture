using System.Diagnostics;

using Aperture.Client;

namespace Aperture.Indexer;

/// <summary>
/// Where facts go: batched by predicate, encoded as blocks, and written.
/// </summary>
/// <remarks>
/// <para>
/// <b>A block carries one predicate</b>, so the batching is per predicate rather than
/// one queue in emission order — an indexer produces a declaration, its search entry
/// and a dozen references interleaved, and a single queue would mean a block per fact.
/// </para>
/// <para>
/// <b>A flush is a write stream.</b> The client issues streams sequentially and the
/// server interns a block inside its per-database writer lock, so a bigger
/// <c>--batch</c> is fewer round trips and a longer lock hold. Four thousand is a
/// compromise; it is a flag because finding out what it should be is the point of
/// having something to measure with.
/// </para>
/// <para>
/// <b>Nothing here holds an id.</b> Every reference is the target fact nested inline,
/// which is why this sink can flush a partial index at any moment and in any order: no
/// fact it has queued depends on one it has already sent.
/// </para>
/// </remarks>
internal sealed class FactSink : IDisposable
{
    private readonly Options _options;
    private readonly ApertureConnection? _connection;
    private readonly FileStream? _emit;
    private readonly List<ApertureFact>[] _pending;

    public FactSink(Options options, ApertureConnection? connection)
    {
        _options = options;
        _connection = connection;
        _emit = options.Emit is null ? null : File.Create(options.Emit);
        _pending = new List<ApertureFact>[CodeIndex.Predicates.Length];

        foreach (var predicate in CodeIndex.Predicates)
        {
            _pending[predicate] = new List<ApertureFact>(options.Batch);
        }

        Facts = new long[CodeIndex.Predicates.Length];
    }

    /// <summary>Facts queued per predicate, whether or not they turned out to be new.</summary>
    public long[] Facts { get; }

    public long Blocks { get; private set; }

    /// <summary>Encoded block bytes — counted only when this sink encodes, which is when it emits or is dry.</summary>
    public long Bytes { get; private set; }

    public ulong Created { get; private set; }

    public ulong Deduped { get; private set; }

    /// <summary>Time inside <see cref="ApertureConnection.Write(uint, IReadOnlyList{ApertureFact})"/>.</summary>
    public TimeSpan Writing { get; private set; }

    public long Total
    {
        get
        {
            var total = 0L;
            foreach (var count in Facts)
            {
                total += count;
            }

            return total;
        }
    }

    public void Add(uint predicate, ApertureFact fact)
    {
        var batch = _pending[predicate];
        batch.Add(fact);
        Facts[predicate]++;

        if (batch.Count >= _options.Batch)
        {
            Flush(predicate);
        }
    }

    public void FlushAll()
    {
        foreach (var predicate in CodeIndex.Predicates)
        {
            Flush(predicate);
        }
    }

    private void Flush(uint predicate)
    {
        var batch = _pending[predicate];

        if (batch.Count == 0)
        {
            return;
        }

        // Encoded here only when the bytes are wanted for themselves: a connected run
        // hands the facts to the client, which encodes them once on the way out.
        if (_emit is not null || _options.DryRun)
        {
            var block = Block.Encode(CodeIndex.Schema, predicate, batch);
            Bytes += block.Length;
            _emit?.Write(block);
        }

        if (_connection is not null)
        {
            var started = Stopwatch.GetTimestamp();
            var summary = _connection.Write(predicate, batch);
            Writing += Stopwatch.GetElapsedTime(started);

            Created += summary.Created;
            Deduped += summary.Deduped;
        }

        Blocks++;
        batch.Clear();
    }

    public void Dispose()
    {
        FlushAll();
        _emit?.Dispose();
    }
}
