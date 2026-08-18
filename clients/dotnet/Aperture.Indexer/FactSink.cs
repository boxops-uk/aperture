using System.Collections.Concurrent;
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
/// <b>A full block is handed to a writer thread, not written where it filled.</b> The
/// walk holds <c>Indexer._gate</c> across every <see cref="Add"/>, so writing inline
/// meant one walker thread sat in a network round trip and a server intern — measured
/// at 368 ms per block — while the other seven blocked on the lock. On the 25M-fact
/// <c>dotnet/runtime</c> index that was 2255s of 4828s, and it is why <see cref="Add"/>
/// now only detaches the full list and queues it.
/// </para>
/// <para>
/// <b>The queue is bounded, and that bound is the backpressure.</b> The server interns
/// one block at a time per database (its writer mutex), so an unbounded queue would
/// only convert a stall into memory. When the walk outruns the writer, producers block
/// in <see cref="Queueing"/> — which is the honest measure of how much the write path
/// still costs, now that it no longer costs the lock.
/// </para>
/// <para>
/// <b>Nothing here holds an id.</b> Every reference is the target fact nested inline,
/// which is why this sink can flush a partial index at any moment and in any order: no
/// fact it has queued depends on one it has already sent.
/// </para>
/// </remarks>
internal sealed class FactSink : IDisposable
{
    /// <summary>How many full blocks may wait for the writer before producers block.</summary>
    /// <remarks>
    /// Small on purpose. One block is up to <c>--batch</c> facts, and the server drains
    /// them strictly one at a time, so a deep queue buys no throughput — it only hides
    /// the stall from <see cref="Queueing"/>, which is the number we want to see.
    /// </remarks>
    private const int QueueDepth = 8;

    private readonly Options _options;
    private readonly ApertureConnection? _connection;
    private readonly FileStream? _emit;
    private readonly List<ApertureFact>[] _pending;
    private readonly BlockingCollection<(uint Predicate, List<ApertureFact> Facts)> _queue;
    private readonly Thread _writer;

    /// <summary>Set if the writer thread died; rethrown from <see cref="Dispose"/>.</summary>
    /// <remarks>
    /// A writer that fails silently is a partial index that looks complete, so the
    /// failure is latched and the queue is closed — which unblocks any producer parked
    /// on a full queue and makes the next <see cref="Add"/> throw rather than hang.
    /// </remarks>
    private Exception? _failure;

    private long _queueingTicks;

    private bool _drained;

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

        _queue = new BlockingCollection<(uint, List<ApertureFact>)>(QueueDepth);
        _writer = new Thread(WriteLoop) { IsBackground = false, Name = "aperture-writer" };
        _writer.Start();
    }

    /// <summary>Facts queued per predicate, whether or not they turned out to be new.</summary>
    public long[] Facts { get; }

    public long Blocks { get; private set; }

    /// <summary>Encoded block bytes — counted only when this sink encodes, which is when it emits or is dry.</summary>
    public long Bytes { get; private set; }

    public ulong Created { get; private set; }

    public ulong Deduped { get; private set; }

    /// <summary>Time the writer thread spent inside <see cref="ApertureConnection.Write(uint, IReadOnlyList{ApertureFact})"/>.</summary>
    /// <remarks>
    /// No longer time the walk pays: it is the writer thread's, and it overlaps the
    /// walk. Compare it against <see cref="Queueing"/> — writing that exceeds the walk
    /// shows up there and nowhere else.
    /// </remarks>
    public TimeSpan Writing { get; private set; }

    /// <summary>Time producers spent blocked on a full queue, i.e. waiting for the writer.</summary>
    public TimeSpan Queueing => Stopwatch.GetElapsedTime(0, Interlocked.Read(ref _queueingTicks));

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

    /// <summary>Detach the pending block and hand it to the writer. Never writes here.</summary>
    private void Flush(uint predicate)
    {
        var batch = _pending[predicate];

        if (batch.Count == 0)
        {
            return;
        }

        // A fresh list rather than Clear(): the writer thread owns the one we hand over
        // until it has encoded and sent it, and clearing it here would race that.
        _pending[predicate] = new List<ApertureFact>(_options.Batch);

        var started = Stopwatch.GetTimestamp();
        try
        {
            _queue.Add((predicate, batch));
        }
        catch (InvalidOperationException)
        {
            // The writer latched a failure and closed the queue; Dispose rethrows it.
            return;
        }

        Interlocked.Add(ref _queueingTicks, Stopwatch.GetTimestamp() - started);
    }

    /// <summary>The one thread that encodes, emits and writes. Owns every counter it touches.</summary>
    private void WriteLoop()
    {
        try
        {
            foreach (var (predicate, facts) in _queue.GetConsumingEnumerable())
            {
                // Encoded here only when the bytes are wanted for themselves: a connected
                // run hands the facts to the client, which encodes them once on the way out.
                if (_emit is not null || _options.DryRun)
                {
                    var block = Block.Encode(CodeIndex.Schema, predicate, facts);
                    Bytes += block.Length;
                    _emit?.Write(block);
                }

                if (_connection is not null)
                {
                    var started = Stopwatch.GetTimestamp();
                    var summary = _connection.Write(predicate, facts);
                    Writing += Stopwatch.GetElapsedTime(started);

                    Created += summary.Created;
                    Deduped += summary.Deduped;
                }

                Blocks++;
            }
        }
        catch (Exception error)
        {
            _failure = error;
            _queue.CompleteAdding();

            // Drain, so a producer parked on a full queue is released rather than left
            // waiting on a writer that has stopped consuming.
            foreach (var _ in _queue.GetConsumingEnumerable())
            {
            }
        }
    }

    /// <summary>Queue everything left, then wait for the writer to finish.</summary>
    /// <remarks>
    /// <b>Call this before reading any count.</b> <see cref="FlushAll"/> only hands the
    /// remaining blocks to the writer; until it has drained them, <see cref="Blocks"/>,
    /// <see cref="Created"/>, <see cref="Deduped"/> and <see cref="Writing"/> are still
    /// moving. A report taken between the two would be short, and the elapsed time it
    /// divided by would stop before the last block was written.
    /// </remarks>
    public void Drain()
    {
        if (_drained)
        {
            return;
        }

        _drained = true;
        FlushAll();
        _queue.CompleteAdding();
        _writer.Join();

        if (_failure is not null)
        {
            throw new InvalidOperationException("the fact writer failed", _failure);
        }
    }

    public void Dispose()
    {
        Drain();
        _emit?.Dispose();
        _queue.Dispose();
    }
}
