namespace Boxops.Fjord.Client;

/// <summary>
/// Where a server is, and which database on it: <c>[where//]name[@instance]</c>.
/// </summary>
/// <remarks>
/// <para>
/// The same grammar the Rust client implements, stated independently — which is the
/// point of this client existing at all. If the two ever disagree about what
/// <c>box:7280//code@01M0B3D</c> means, that is a finding rather than a nuisance.
/// </para>
/// <list type="table">
///   <item><term><c>code</c></term><description>the caller's default target</description></item>
///   <item><term><c>//code</c></term><description>the same, said explicitly</description></item>
///   <item><term><c>box:7280//code</c></term><description>TCP</description></item>
///   <item><term><c>box//code</c></term><description>TCP, <see cref="DefaultPort"/></description></item>
///   <item><term><c>/run/fjord.sock//code</c></term><description>a Unix socket</description></item>
///   <item><term><c>./dev.sock//code</c></term><description>a relative socket path</description></item>
/// </list>
/// <para>
/// Two rules carry it. Split at the <b>last</b> <c>//</c>, because a database name cannot
/// contain <c>/</c> and a socket path can. And a relative socket path needs <c>./</c>,
/// because <c>dev.sock//code</c> is otherwise indistinguishable from a host called
/// <c>dev.sock</c>.
/// </para>
/// <para>
/// The selector is carried as a string and never parsed here: which instance of a name is
/// meant is the server's question.
/// </para>
/// </remarks>
public sealed record FjordAddress
{
    /// <summary>The TCP port a <c>host//db</c> address means.</summary>
    public const int DefaultPort = 7280;

    /// <summary>What separates where a server is from which database on it.</summary>
    public const string Separator = "//";

    private FjordAddress(string? socketPath, string? host, int port, string database)
    {
        SocketPath = socketPath;
        Host = host;
        Port = port;
        Database = database;
    }

    /// <summary>The Unix socket to connect to, or <c>null</c> if this is TCP or default.</summary>
    public string? SocketPath { get; }

    /// <summary>The TCP host, or <c>null</c> if this is a socket or default.</summary>
    public string? Host { get; }

    /// <summary>The TCP port, meaningful only when <see cref="Host"/> is set.</summary>
    public int Port { get; }

    /// <summary><c>name</c>, or <c>name@instance</c>. Empty means a control session.</summary>
    public string Database { get; }

    /// <summary>Whether the address named where to go, rather than leaving it to the caller.</summary>
    public bool HasTarget => SocketPath is not null || Host is not null;

    /// <summary>
    /// Parse <c>[where//]database</c>.
    /// </summary>
    /// <exception cref="FormatException">If it is not an address.</exception>
    public static FjordAddress Parse(string text)
    {
        ArgumentNullException.ThrowIfNull(text);

        // The *last* separator: a database name cannot contain `/`, so the last one is
        // always the right one — which is also what makes a socket path holding a
        // doubled slash parse instead of misread.
        var at = text.LastIndexOf(Separator, StringComparison.Ordinal);

        var where = at < 0 ? "" : text[..at];
        var database = at < 0 ? text : text[(at + Separator.Length)..];

        if (database.Contains('/'))
        {
            throw new FormatException(
                $"`{text}`: a database name cannot contain `/`");
        }

        if (where.Length == 0)
        {
            return new FjordAddress(null, null, 0, database);
        }

        if (IsPath(where))
        {
            return new FjordAddress(ExpandHome(where), null, 0, database);
        }

        if (where.Contains('/'))
        {
            throw new FormatException(
                $"`{text}`: `{where}` is neither a host nor a path — " +
                "a relative socket path needs `./`");
        }

        var (host, port) = SplitAuthority(where);
        return new FjordAddress(null, host, port, database);
    }

    /// <summary>An address at <paramref name="socketPath"/>, for a caller that holds one.</summary>
    public static FjordAddress ForSocket(string socketPath, string database) =>
        new(socketPath, null, 0, database);

    /// <summary>This address with a socket filled in if it named no target.</summary>
    public FjordAddress OrSocket(string socketPath) =>
        HasTarget ? this : new FjordAddress(socketPath, null, 0, Database);

    public override string ToString()
    {
        if (SocketPath is not null)
        {
            return $"{SocketPath}{Separator}{Database}";
        }

        if (Host is not null)
        {
            return $"{Authority()}{Separator}{Database}";
        }

        return Database.Length == 0 ? Separator : Database;
    }

    /// <summary>The <c>host:port</c> form, for a message somebody reads.</summary>
    public string Authority() => Host is null ? "" : $"{Host}:{Port}";

    /// <summary>
    /// Absolute, explicitly relative, or home-relative. A bare <c>dev.sock</c> is a host,
    /// which is the ambiguity <c>./</c> exists to settle.
    /// </summary>
    private static bool IsPath(string text) =>
        text.StartsWith('/')
        || text.StartsWith("./", StringComparison.Ordinal)
        || text.StartsWith("../", StringComparison.Ordinal)
        || text == "~"
        || text.StartsWith("~/", StringComparison.Ordinal);

    private static string ExpandHome(string text)
    {
        if (!text.StartsWith('~'))
        {
            return text;
        }

        var home = Environment.GetEnvironmentVariable("HOME");
        if (string.IsNullOrEmpty(home))
        {
            return text;
        }

        return Path.Combine(home, text[1..].TrimStart('/'));
    }

    /// <summary>
    /// Split a host from its port, defaulting the port.
    /// </summary>
    /// <remarks>
    /// Bracketed IPv6 is why this is not a search for <c>:</c> — <c>[::1]</c> is all
    /// colons and no port, and <c>[::1]:7280</c> is the same address with one.
    /// </remarks>
    private static (string Host, int Port) SplitAuthority(string text)
    {
        if (text.StartsWith('['))
        {
            var close = text.LastIndexOf("]:", StringComparison.Ordinal);
            return close < 0
                ? (text, DefaultPort)
                : (text[..(close + 1)], ParsePort(text, text[(close + 2)..]));
        }

        var colon = text.IndexOf(':');
        return colon < 0
            ? (text, DefaultPort)
            : (text[..colon], ParsePort(text, text[(colon + 1)..]));
    }

    private static int ParsePort(string address, string text)
    {
        if (!int.TryParse(text, out var port) || port is < 1 or > 65535)
        {
            throw new FormatException($"`{address}`: `{text}` is not a port");
        }

        return port;
    }
}
