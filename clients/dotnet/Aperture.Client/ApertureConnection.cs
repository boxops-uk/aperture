using System.Net.Sockets;
using System.Text;

namespace Aperture.Client;

/// <summary>Which way a session may go, declared once at startup.</summary>
public enum SessionMode : byte
{
    ReadOnly = 0,
    ReadWrite = 1,
}

/// <summary>What the server said when the session opened.</summary>
public sealed record ServerHello(uint Version, ulong SchemaFingerprint, ulong Predicates);

/// <summary>What a write stream did.</summary>
/// <remarks>
/// <paramref name="Created"/> counts <b>every</b> fact written, nested targets
/// included, and <paramref name="Deduped"/> those already there. A producer sending a
/// thousand declarations that all name one file sees a thousand and one created and
/// nine hundred and ninety-nine deduped — which is how it can tell interning is
/// working without querying anything.
/// </remarks>
public sealed record WriteSummary(ulong Created, ulong Deduped)
{
    public ulong Seen => Created + Deduped;
}

/// <summary>A query's rows, and the shape they came in.</summary>
public sealed record QueryResult(ApertureType Shape, IReadOnlyList<ApertureValue> Rows);

/// <summary>
/// A connection to an Aperture server.
/// </summary>
/// <remarks>
/// <para>
/// One connection carries several streams: a write is a stream and a query is a
/// stream, each identified by a number the caller chooses. This client issues them
/// sequentially — it sends a stream's frames and reads its replies before starting the
/// next — which is all the current server does anyway. The stream ids are real
/// nonetheless, and the server tags every reply with the stream it belongs to.
/// </para>
/// <para>
/// <b>The schema is the client's.</b> Nothing in the protocol describes it: the value
/// codec sends no names and no types because both ends already have them. The
/// handshake asserts they agree, by fingerprint, before a byte of data flows.
/// </para>
/// </remarks>
public sealed class ApertureConnection : IDisposable
{
    /// <summary>The protocol version this client speaks.</summary>
    public const uint ProtocolVersion = 1;

    private readonly Socket _socket;
    private readonly NetworkStream _stream;
    private readonly ApertureSchema _schema;
    private uint _nextStream = 1;

    private ApertureConnection(Socket socket, ApertureSchema schema, ServerHello hello)
    {
        _socket = socket;
        _stream = new NetworkStream(socket, ownsSocket: false);
        _schema = schema;
        Hello = hello;
    }

    public ServerHello Hello { get; }

    /// <summary>
    /// Connect over a Unix socket and complete the handshake.
    /// </summary>
    /// <param name="socketPath">Where the server is listening.</param>
    /// <param name="database">The database to open.</param>
    /// <param name="schema">The schema this client writes against.</param>
    /// <param name="mode">Read-only or read-write, resolved once here.</param>
    /// <param name="assertSchema">
    /// Whether to send the schema fingerprint as a claim. <c>true</c> is the right
    /// default for a producer: a disagreement is then refused at the handshake instead
    /// of by writing facts nobody can read back. <c>false</c> sends <c>0</c>, which
    /// means "do not check" and is what a reader wants.
    /// </param>
    public static ApertureConnection Connect(
        string socketPath,
        string database,
        ApertureSchema schema,
        SessionMode mode = SessionMode.ReadWrite,
        bool assertSchema = true)
    {
        var socket = new Socket(AddressFamily.Unix, SocketType.Stream, ProtocolType.Unspecified);
        socket.Connect(new UnixDomainSocketEndPoint(socketPath));

        var stream = new NetworkStream(socket, ownsSocket: false);

        var startup = new ByteBuffer();
        Varint.Write(startup, ProtocolVersion);
        WriteString(startup, database);
        startup.WriteByte((byte)mode);
        Varint.Write(startup, assertSchema ? schema.Fingerprint() : 0);

        FrameIo.Write(stream, FrameKind.Startup, 0, startup.Span);

        var reply = FrameIo.Read(stream);
        ThrowIfError(reply);

        if (reply.Kind != FrameKind.Ready)
        {
            throw new ApertureProtocolException(
                $"expected a ready frame, got `{(char)reply.Kind}`");
        }

        var at = 0;
        var hello = new ServerHello(
            (uint)Varint.Read(reply.Payload, ref at),
            Varint.Read(reply.Payload, ref at),
            Varint.Read(reply.Payload, ref at));

        if (hello.Version != ProtocolVersion)
        {
            throw new ApertureProtocolException(
                $"this client speaks protocol {ProtocolVersion}, the server speaks {hello.Version}");
        }

        return new ApertureConnection(socket, schema, hello);
    }

    /// <summary>
    /// Write facts, all of one predicate, as one block on one write stream.
    /// </summary>
    /// <remarks>
    /// References inside the facts may be nested — the whole target fact rather than an
    /// id — and the server interns them. That is what lets a producer keep no book of
    /// what it has already sent.
    /// </remarks>
    public WriteSummary Write(uint predicate, IReadOnlyList<ApertureFact> facts) =>
        Write([(predicate, facts)]);

    /// <summary>Write several blocks on one write stream.</summary>
    public WriteSummary Write(IReadOnlyList<(uint Predicate, IReadOnlyList<ApertureFact> Facts)> blocks)
    {
        var stream = _nextStream++;

        FrameIo.Write(_stream, FrameKind.OpenWrite, stream, []);
        var opened = FrameIo.Read(_stream);
        ThrowIfError(opened);

        if (opened.Kind != FrameKind.CopyInResponse)
        {
            throw new ApertureProtocolException(
                $"expected a copy-in response, got `{(char)opened.Kind}`");
        }

        foreach (var (predicate, facts) in blocks)
        {
            var block = Block.Encode(_schema, predicate, facts);
            FrameIo.Write(_stream, FrameKind.CopyData, stream, block);
        }

        FrameIo.Write(_stream, FrameKind.CopyDone, stream, []);

        var complete = FrameIo.Read(_stream);
        ThrowIfError(complete);

        if (complete.Kind != FrameKind.Complete)
        {
            throw new ApertureProtocolException(
                $"expected a complete frame, got `{(char)complete.Kind}`");
        }

        var at = 0;
        return new WriteSummary(
            Varint.Read(complete.Payload, ref at),
            Varint.Read(complete.Payload, ref at));
    }

    /// <summary>Run a focus query and collect its rows.</summary>
    /// <remarks>
    /// The server sends a <b>row descriptor</b> first, because a query's shape comes
    /// from its head rather than from any predicate — <c>{a = X, b = Y}</c> is a record
    /// no predicate declares. Rows then follow positionally against it, decoded by the
    /// same codec that encodes facts.
    /// </remarks>
    public QueryResult Query(string focus)
    {
        var stream = _nextStream++;
        FrameIo.Write(_stream, FrameKind.Query, stream, Encoding.UTF8.GetBytes(focus));

        var described = FrameIo.Read(_stream);
        ThrowIfError(described);

        if (described.Kind != FrameKind.RowDescription)
        {
            throw new ApertureProtocolException(
                $"expected a row description, got `{(char)described.Kind}`");
        }

        var at = 0;
        var shape = RowDescriptor.Read(described.Payload, ref at);
        var rows = new List<ApertureValue>();

        while (true)
        {
            var frame = FrameIo.Read(_stream);
            ThrowIfError(frame);

            if (frame.Kind == FrameKind.Complete)
            {
                return new QueryResult(shape, rows);
            }

            if (frame.Kind != FrameKind.DataRow)
            {
                throw new ApertureProtocolException(
                    $"expected a data row, got `{(char)frame.Kind}`");
            }

            var rowAt = 0;
            rows.Add(ValueCodec.ReadValue(frame.Payload, _schema, shape, ref rowAt));
        }
    }

    private static void ThrowIfError(Frame frame)
    {
        if (frame.Kind != FrameKind.Error)
        {
            return;
        }

        if (frame.Payload.Length < 1)
        {
            throw new ApertureProtocolException("an error frame with no code");
        }

        var code = (ApertureErrorCode)frame.Payload[0];
        var at = 1;
        var length = Varint.Read(frame.Payload, ref at);
        var message = Encoding.UTF8.GetString(frame.Payload, at, (int)length);

        throw new ApertureServerException(code, message);
    }

    private static void WriteString(IBufferSink sink, string text)
    {
        var utf8 = Encoding.UTF8.GetBytes(text);
        Varint.Write(sink, (ulong)utf8.Length);
        sink.Write(utf8);
    }

    public void Dispose()
    {
        _stream.Dispose();
        _socket.Dispose();
    }
}

/// <summary>
/// The row descriptor: the outbound direction's type source.
/// </summary>
/// <remarks>
/// This is the <b>one</b> place the format carries type tags, and it carries them once
/// per stream rather than once per field per row — which is exactly the trade that
/// makes tagging affordable here and not in a fact.
/// </remarks>
public static class RowDescriptor
{
    public static ApertureType Read(ReadOnlySpan<byte> bytes, ref int at)
    {
        var tag = Varint.Read(bytes, ref at);

        switch (tag)
        {
            case 0:
                return ApertureType.Integer;

            case 1:
                return ApertureType.String;

            case 2:
                return ApertureType.Reference((uint)Varint.Read(bytes, ref at));

            case 3:
            {
                var count = Varint.Read(bytes, ref at);
                if (count > (ulong)bytes.Length)
                {
                    throw new ApertureProtocolException("a descriptor declares more fields than could fit");
                }

                var fields = new List<(string, ApertureType)>((int)count);

                for (ulong index = 0; index < count; index++)
                {
                    var length = Varint.Read(bytes, ref at);
                    if (length > (ulong)(bytes.Length - at))
                    {
                        throw new ApertureProtocolException("a field name runs past the descriptor");
                    }

                    var name = Encoding.UTF8.GetString(bytes.Slice(at, (int)length));
                    at += (int)length;

                    fields.Add((name, Read(bytes, ref at)));
                }

                return new ApertureType.Record(fields);
            }

            default:
                throw new ApertureProtocolException($"unknown descriptor tag {tag}");
        }
    }
}
