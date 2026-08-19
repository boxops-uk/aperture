namespace Boxops.Fjord.Client;

/// <summary>The peer sent something this client cannot make sense of.</summary>
public sealed class FjordProtocolException(string message) : Exception(message);

/// <summary>The server refused, and said why.</summary>
public sealed class FjordServerException(FjordErrorCode code, string message)
    : Exception($"{code}: {message}")
{
    /// <summary>What went wrong, as a code a caller can branch on without reading English.</summary>
    public FjordErrorCode Code { get; } = code;

    /// <summary>The server's own wording.</summary>
    public string ServerMessage { get; } = message;
}

/// <summary>
/// Mirrors <c>fjord_server::protocol::ErrorCode</c>. The numbers are the wire
/// contract, so they are written out rather than left to declaration order.
/// </summary>
public enum FjordErrorCode : byte
{
    Protocol = 1,
    UnknownDatabase = 2,
    SchemaMismatch = 3,
    ModeRefused = 4,
    BadFacts = 5,
    Conflict = 6,
    BadQuery = 7,
    Internal = 8,
}
