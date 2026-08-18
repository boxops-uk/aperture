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
/// <b>Several writers, each with its own connection.</b> The server excludes writers per
/// <em>key</em> rather than per database, so a database takes as many as there are
/// streams. One connection cannot carry them: <see cref="ApertureConnection"/> issues
/// streams sequentially and shares one socket, so concurrency here means one connection
/// per writer thread and nothing shared between them but the queue.
/// </para>
/// <para>
/// <b>The queue is bounded, and that bound is the backpressure.</b> An unbounded queue
/// would convert a stall into memory rather than into throughput. The bound scales with
/// the writer count, because the thing it has to keep fed is now several drains rather
/// than one. When the walk outruns them, producers block in <see cref="Queueing"/> —
/// which is what says whether the write path is still the ceiling.
/// </para>
/// <para>
/// <b>Nothing here holds an id.</b> Every reference is the target fact nested inline,
/// which is why this sink can flush a partial index at any moment and in any order: no
/// fact it has queued depends on one it has already sent.
/// </para>
/// </remarks>
internal sealed class FactSink : IDisposable
{
    /// <summary>Blocks that may wait <em>per writer</em> before producers block.</summary>
    /// <remarks>
    /// Small on purpose, and multiplied by the writer count rather than fixed. One block
    /// is up to <c>--batch</c> facts, so a deep queue costs real memory and buys nothing
    /// once the writers are keeping up — it only hides the stall from
    /// <see cref="Queueing"/>, which is the number we want to see.
    /// </remarks>
    private const int QueueDepthPerWriter = 4;

    private readonly Options _options;
    private readonly IReadOnlyList<ApertureConnection> _connections;
    private readonly FileStream? _emit;
    private readonly List<ApertureFact>[] _pending;
    private readonly BlockingCollection<(uint Predicate, List<ApertureFact> Facts)> _queue;
    private readonly Thread[] _writers;

    /// <summary>Set if the writer thread died; rethrown from <see cref="Dispose"/>.</summary>
    /// <remarks>
    /// A writer that fails silently is a partial index that looks complete, so the
    /// failure is latched and the queue is closed — which unblocks any producer parked
    /// on a full queue and makes the next <see cref="Add"/> throw rather than hang.
    /// </remarks>
    private Exception? _failure;

    private long _queueingTicks;

    // Written by every writer thread, so every one of them is interlocked and the
    // properties below are projections rather than fields.
    private long _blocks;
    private long _bytes;
    private long _created;
    private long _deduped;
    private long _writingTicks;

    private bool _drained;

    /// <summary>A sink writing through <paramref name="connections"/>, one writer thread each.</summary>
    /// <remarks>
    /// An empty list is a run that connects to nothing (<c>--dry-run</c>), which still
    /// wants one thread so the encoding it measures happens off the walk.
    /// </remarks>
    public FactSink(Options options, IReadOnlyList<ApertureConnection> connections)
    {
        _options = options;
        _connections = connections;
        _emit = options.Emit is null ? null : File.Create(options.Emit);
        _pending = new List<ApertureFact>[CodeIndex.Predicates.Length];

        foreach (var predicate in CodeIndex.Predicates)
        {
            _pending[predicate] = new List<ApertureFact>(options.Batch);
        }

        Facts = new long[CodeIndex.Predicates.Length];

        var writers = Math.Max(1, connections.Count);
        _queue = new BlockingCollection<(uint, List<ApertureFact>)>(QueueDepthPerWriter * writers);

        _writers = new Thread[writers];
        for (var n = 0; n < writers; n++)
        {
            // Each thread owns exactly one connection, so nothing about the socket or
            // the stream numbering is shared and none of it needs a lock.
            var connection = n < connections.Count ? connections[n] : null;
            _writers[n] = new Thread(() => WriteLoop(connection))
            {
                IsBackground = false,
                Name = $"aperture-writer-{n}",
            };
            _writers[n].Start();
        }
    }

    /// <summary>How many writer threads are draining the queue.</summary>
    public int Writers => _writers.Length;

    /// <summary>Facts queued per predicate, whether or not they turned out to be new.</summary>
    public long[] Facts { get; }

    public long Blocks => Interlocked.Read(ref _blocks);

    /// <summary>Encoded block bytes — counted only when this sink encodes, which is when it emits or is dry.</summary>
    public long Bytes => Interlocked.Read(ref _bytes);

    public ulong Created => (ulong)Interlocked.Read(ref _created);

    public ulong Deduped => (ulong)Interlocked.Read(ref _deduped);

    /// <summary>Time <em>summed over the writers</em> inside <see cref="ApertureConnection.Write(uint, IReadOnlyList{ApertureFact})"/>.</summary>
    /// <remarks>
    /// Not time the walk pays, and — with more than one writer — not wall clock either:
    /// it is a sum across threads that overlap each other as well as the walk, so it can
    /// exceed the run's elapsed time and says nothing on its own. Divide by
    /// <see cref="Writers"/> for a rough per-writer figure, and read
    /// <see cref="Queueing"/> for the question that actually matters — whether the walk
    /// ever had to wait for them.
    /// </remarks>
    public TimeSpan Writing => Stopwatch.GetElapsedTime(0, Interlocked.Read(ref _writingTicks));

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

    /// <summary>One writer: encodes, emits and writes down its own connection.</summary>
    /// <remarks>
    /// Every counter it touches is interlocked, because several of these run at once and
    /// the totals are read from the walk's thread at the end.
    /// </remarks>
    private void WriteLoop(ApertureConnection? connection)
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
                    Interlocked.Add(ref _bytes, block.Length);

                    // Only ever one writer when emitting — see `Program.Connect` — so the
                    // file is a deterministic run of blocks rather than an interleaving.
                    _emit?.Write(block);
                }

                if (connection is not null)
                {
                    var started = Stopwatch.GetTimestamp();
                    var summary = connection.Write(predicate, facts);
                    Interlocked.Add(ref _writingTicks, Stopwatch.GetTimestamp() - started);

                    Interlocked.Add(ref _created, (long)summary.Created);
                    Interlocked.Add(ref _deduped, (long)summary.Deduped);
                }

                Interlocked.Increment(ref _blocks);
            }
        }
        catch (Exception error)
        {
            // First failure wins; the rest are consequences of the queue closing.
            Interlocked.CompareExchange(ref _failure, error, null);
            _queue.CompleteAdding();

            // Drain, so a producer parked on a full queue is released rather than left
            // waiting on writers that have stopped consuming.
            foreach (var _ in _queue.GetConsumingEnumerable())
            {
            }
        }
    }

    /// <summary>Queue everything left, then wait for every writer to finish.</summary>
    /// <remarks>
    /// <b>Call this before reading any count.</b> <see cref="FlushAll"/> only hands the
    /// remaining blocks over; until they have all drained, <see cref="Blocks"/>,
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
        foreach (var writer in _writers)
        {
            writer.Join();
        }

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
