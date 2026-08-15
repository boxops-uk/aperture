using Aperture.Client;

// A non-Rust producer writing facts to Aperture over the wire protocol.
//
// The point of this program is what it *does not* do: it holds no fact ids, keeps no
// map from entities to identities, and emits in whatever order it likes. Every
// reference it writes is the target fact itself, nested inline, and the server interns
// it. That is the whole reason a reference on the way in is a fact rather than an id —
// an indexer walking a syntax tree knows the file when it reaches the declaration, and
// should not have to remember what the server called it.

var socket = Args("--socket") ?? "/tmp/aperture.sock";
var database = Args("--database") ?? "code";

string? Args(string flag)
{
    var argv = Environment.GetCommandLineArgs();
    for (var i = 1; i < argv.Length - 1; i++)
    {
        if (argv[i] == flag)
        {
            return argv[i + 1];
        }
    }
    return null;
}

// ---- the schema, written down because a client must have it -------------------
//
// The transport codec sends no field names, no types and no arities: the server has
// them and so does this. Two rules are load-bearing, and the handshake below is what
// checks them rather than something failing later:
//
//   * a predicate's id IS its position here;
//   * a record's fields are in the schema's declared order, which is sorted by name,
//     because that order is part of the encoding.
//
// This mirrors `aperture::code_index` on the Rust side. It is written twice on
// purpose — that is the whole point of the fingerprint.

const uint File = 0;
const uint Module = 1;
const uint Decl = 2;
const uint Reference = 4;

var schema = new ApertureSchema([
    new AperturePredicate("src.File", ApertureType.String, null),

    new AperturePredicate("src.Module", ApertureType.Rec(
        ("file", ApertureType.Reference(File)),
        ("name", ApertureType.String)), null),

    // A value side: the declaration's kind. A value cannot be matched on (I6), which
    // is what makes it the right home for something a query wants to *read* but never
    // to filter by.
    new AperturePredicate("src.Decl", ApertureType.Rec(
        ("line", ApertureType.Integer),
        ("module", ApertureType.Reference(Module)),
        ("name", ApertureType.String)), ApertureType.String),

    new AperturePredicate("src.SearchByName", ApertureType.Rec(
        ("name", ApertureType.String),
        ("to", ApertureType.Reference(Decl))), null),

    // A nested record inside a key, and two references to two different predicates.
    new AperturePredicate("src.Ref", ApertureType.Rec(
        ("at", ApertureType.Rec(
            ("col", ApertureType.Integer),
            ("line", ApertureType.Integer))),
        ("file", ApertureType.Reference(File)),
        ("to", ApertureType.Reference(Decl))), null),

    new AperturePredicate("src.Import", ApertureType.Rec(
        ("from", ApertureType.Reference(Module)),
        ("to", ApertureType.Reference(Module))), null),
]);

Console.WriteLine($"connecting to {socket} ({database})");
Console.WriteLine($"  our schema fingerprint {schema.Fingerprint():x16}");

using var connection = ApertureConnection.Connect(
    socket,
    database,
    schema,
    SessionMode.ReadWrite,
    // A claim, not a question: if the server's schema differs, the handshake refuses
    // before a byte of data flows.
    assertSchema: true);

Console.WriteLine($"  connected: protocol {connection.Hello.Version}, "
    + $"{connection.Hello.Predicates} predicates, schema {connection.Hello.SchemaFingerprint:x16}");
Console.WriteLine();

// ---- writing facts that hold no ids -------------------------------------------

// A declaration names its module, which names its file. Two levels of nesting, and
// this program never learns what any of them were called.
ApertureFact FileFact(string path) =>
    new(File, ApertureValue.Of(path));

ApertureFact ModuleFact(string path, string name) =>
    new(Module, ApertureValue.Rec(
        ApertureValue.Of(ApertureRef.To(FileFact(path))),
        ApertureValue.Of(name)));

// Fields in the schema's order — line, module, name — and the kind on the value side.
ApertureFact DeclFact(string path, string module, string kind, long line, string name) =>
    new(Decl,
        ApertureValue.Rec(
            ApertureValue.Of(line),
            ApertureValue.Of(ApertureRef.To(ModuleFact(path, module))),
            ApertureValue.Of(name)),
        ApertureValue.Of(kind));

var declarations = new List<ApertureFact>
{
    DeclFact("store/keys.py", "keys", "def", 12, "key_of"),
    DeclFact("store/keys.py", "keys", "def", 48, "key_prefix"),
    DeclFact("store/keys.py", "keys", "def", 77, "key_successor"),
    DeclFact("store/codec.py", "codec", "def", 7, "encode_key"),
    DeclFact("store/codec.py", "codec", "class", 31, "CodecError"),
    DeclFact("query/plan.py", "plan", "class", 5, "Plan"),
};

Console.WriteLine($"writing {declarations.Count} declarations, every reference nested");

var summary = connection.Write(Decl, declarations);

Console.WriteLine($"  created {summary.Created}, deduped {summary.Deduped} "
    + $"(of {summary.Seen} facts touched)");
Console.WriteLine($"  {declarations.Count} declarations + 3 modules + 3 files = 12 distinct facts");
Console.WriteLine();

// A reference: a nested record in the key, plus two references to two predicates —
// and the declaration it names is one already written, so it dedups rather than
// creating a second copy.
var references = new List<ApertureFact>
{
    new(Reference, ApertureValue.Rec(
        ApertureValue.Rec(ApertureValue.Of(4L), ApertureValue.Of(19L)),
        ApertureValue.Of(ApertureRef.To(FileFact("query/plan.py"))),
        ApertureValue.Of(ApertureRef.To(
            DeclFact("store/keys.py", "keys", "def", 12, "key_of"))))),
};

var refs = connection.Write(Reference, references);
Console.WriteLine($"a reference with a nested-record key: created {refs.Created}, "
    + $"deduped {refs.Deduped} (its file and declaration were already there)");
Console.WriteLine();

// Writing the same block again writes nothing: interning is idempotent, which is what
// makes a retry after a dropped connection safe.
var again = connection.Write(Decl, declarations);
Console.WriteLine($"the same declarations again: created {again.Created}, deduped {again.Deduped}");
Console.WriteLine();

// ---- reading them back, on the same connection --------------------------------

void Run(string focus)
{
    Console.WriteLine($"focus> {focus}");

    var result = connection.Query(focus);
    Console.WriteLine($"  : {Describe(result.Shape)}");

    foreach (var row in result.Rows)
    {
        Console.WriteLine($"  {Render(row)}");
    }

    Console.WriteLine($"  {result.Rows.Count} row(s)");
    Console.WriteLine();
}

Run("F where src.File F");
Run("N where src.Module {name = N}");
Run("{at = D.line, what = D.name} where D = src.Decl _");
// The value side, which a query can read but never match on (I6).
Run("D.value where D = src.Decl _");

// The denial: declarations whose name does not start with `key`.
Run("N where src.Decl {name = N}; N != \"key\"..");

// Reaching through a reference — the join that makes a fact database worth having.
Run("{decl = D.name, file = D.module.file} where D = src.Decl _");

// The reference's nested-record key, read back.
Run("{line = R.at.line, col = R.at.col} where R = src.Ref _");

try
{
    connection.Query("this is not focus");
}
catch (ApertureServerException error)
{
    Console.WriteLine($"a bad query fails its stream, by code: {error.Code}");
    Console.WriteLine($"  {error.ServerMessage.Split('\n')[0]}");
    Console.WriteLine();
}

// ...and the connection is still usable afterwards, which is the point of failing a
// stream rather than a connection.
Run("F where src.File F");

Console.WriteLine("done");

static string Describe(ApertureType type) => type switch
{
    ApertureType.Int => "int",
    ApertureType.Str => "string",
    ApertureType.Fact fact => $"fact({fact.Predicate})",
    ApertureType.Record record =>
        "{" + string.Join(", ", record.Fields.Select(f => $"{f.Name} : {Describe(f.Type)}")) + "}",
    _ => "?",
};

static string Render(ApertureValue value) => value switch
{
    ApertureValue.Int n => n.Value.ToString(),
    ApertureValue.Str s => $"\"{s.Value}\"",
    ApertureValue.Ref { Value: ApertureRef.Id id } => $"#{id.FactId >> 40}:{id.FactId & 0xFFFFFFFFFF}",
    ApertureValue.Ref { Value: ApertureRef.Nested nested } => $"<{Render(nested.Fact.Key)}>",
    ApertureValue.Record record => "{" + string.Join(", ", record.Fields.Select(Render)) + "}",
    _ => "?",
};
