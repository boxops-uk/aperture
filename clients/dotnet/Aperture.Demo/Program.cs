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

// With `--golden <path>` this program connects to nothing: it encodes a fixed corpus
// and writes the bytes out, for the Rust client's test to compare itself against. See
// EmitGolden below for why that file exists.
var goldenPath = Args("--golden");

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

// The build layer and the declaration graph. This program writes none of them — it is
// six declarations somebody typed out, and they are answered by a compiler — but the
// fingerprint is over the *whole* schema, so a client that omits them is a client the
// handshake refuses. `Aperture.Indexer` is what fills them.
const uint Project = 6;
const uint Assembly = 7;
const uint Package = 11;
const uint Param = 17;
const uint Doc = 19;

var schema = new ApertureSchema([
    new AperturePredicate("src.File", ApertureType.String, null),

    new AperturePredicate("src.Module", ApertureType.Rec(
        ("file", ApertureType.Reference(File)),
        ("name", ApertureType.String)), null),

    // A value side: the declaration's kind. A value cannot be matched on (I6), which
    // is what makes it the right home for something a query wants to *read* but never
    // to filter by.
    new AperturePredicate("src.Decl", ApertureType.Rec(
        ("module", ApertureType.Reference(Module)),
        ("name", ApertureType.String),
        ("line", ApertureType.Integer)), ApertureType.String),

    new AperturePredicate("src.SearchByName", ApertureType.Rec(
        ("name", ApertureType.String),
        ("to", ApertureType.Reference(Decl))), null),

    // A nested record inside a key, and two references to two different predicates.
    new AperturePredicate("src.Ref", ApertureType.Rec(
        ("to", ApertureType.Reference(Decl)),
        ("file", ApertureType.Reference(File)),
        ("at", ApertureType.Rec(
            ("line", ApertureType.Integer),
            ("col", ApertureType.Integer)))), null),

    new AperturePredicate("src.Import", ApertureType.Rec(
        ("from", ApertureType.Reference(Module)),
        ("to", ApertureType.Reference(Module))), null),

    // ---- the build layer: what compiled a file, and into what ---------------------

    new AperturePredicate("src.Project", ApertureType.String, null),

    new AperturePredicate("src.Assembly", ApertureType.String, null),

    new AperturePredicate("src.Compilation", ApertureType.Rec(
        ("assembly", ApertureType.Reference(Assembly)),
        ("framework", ApertureType.String),
        ("project", ApertureType.Reference(Project))), null),

    new AperturePredicate("src.ProjectSource", ApertureType.Rec(
        ("file", ApertureType.Reference(File)),
        ("project", ApertureType.Reference(Project))), null),

    new AperturePredicate("src.ProjectRef", ApertureType.Rec(
        ("from", ApertureType.Reference(Project)),
        ("to", ApertureType.Reference(Project))), null),

    new AperturePredicate("src.Package", ApertureType.Rec(
        ("name", ApertureType.String),
        ("version", ApertureType.String)), null),

    new AperturePredicate("src.PackageRef", ApertureType.Rec(
        ("package", ApertureType.Reference(Package)),
        ("project", ApertureType.Reference(Project))), null),

    // ---- the declaration graph ----------------------------------------------------

    new AperturePredicate("src.Member", ApertureType.Rec(
        ("container", ApertureType.Reference(Decl)),
        ("member", ApertureType.Reference(Decl))), null),

    new AperturePredicate("src.Extends", ApertureType.Rec(
        ("base", ApertureType.Reference(Decl)),
        ("type", ApertureType.Reference(Decl))), null),

    new AperturePredicate("src.Implements", ApertureType.Rec(
        ("iface", ApertureType.Reference(Decl)),
        ("type", ApertureType.Reference(Decl))), null),

    new AperturePredicate("src.Override", ApertureType.Rec(
        ("base", ApertureType.Reference(Decl)),
        ("member", ApertureType.Reference(Decl))), null),

    new AperturePredicate("src.Param", ApertureType.Rec(
        ("decl", ApertureType.Reference(Decl)),
        ("index", ApertureType.Integer),
        ("name", ApertureType.String)), ApertureType.String),

    // A key of one field, which encodes as the bare reference does.
    new AperturePredicate("src.TypeOf", ApertureType.Rec(
        ("decl", ApertureType.Reference(Decl))), ApertureType.String),

    new AperturePredicate("src.Doc", ApertureType.Rec(
        ("decl", ApertureType.Reference(Decl))), ApertureType.String),

    new AperturePredicate("src.Attribute", ApertureType.Rec(
        ("attribute", ApertureType.String),
        ("target", ApertureType.Reference(Decl))), null),

    new AperturePredicate("src.Line", ApertureType.Rec(
        ("file", ApertureType.Reference(File)),
        ("line", ApertureType.Integer)), ApertureType.String),
]);

if (goldenPath is not null)
{
    EmitGolden(goldenPath);
    return;
}

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
            ApertureValue.Of(ApertureRef.To(ModuleFact(path, module))),
            ApertureValue.Of(name),
            ApertureValue.Of(line)),
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
        ApertureValue.Of(ApertureRef.To(
            DeclFact("store/keys.py", "keys", "def", 12, "key_of"))),
        ApertureValue.Of(ApertureRef.To(FileFact("query/plan.py"))),
        ApertureValue.Rec(ApertureValue.Of(19L), ApertureValue.Of(4L)))),
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

// ---- the golden corpus ---------------------------------------------------------
//
// Phase 9e's acceptance criterion is that the Rust and C# clients produce **byte
// identical** blocks for the same facts. Interoperating today does not prove that:
// the two could disagree about something the server happens to tolerate, or about a
// case neither demo exercises, and a fact file written by one would then not be the
// file the other writes.
//
// So this mode encodes a fixed corpus and writes the bytes out. `aperture-client`'s
// test reads the file, encodes the same facts from its own independent statement of
// the same schema, and compares. Neither side can be changed alone without the other
// noticing — which is the whole reason there is a second implementation at all.
//
// The corpus is chosen for what it *reaches* rather than for what it means: scalars,
// a value side, two levels of nesting, a record inside a key, two references to two
// different predicates, and integers on both sides of the varint's one-byte boundary.
void EmitGolden(string path)
{
    (string Name, uint Predicate, IReadOnlyList<ApertureFact> Facts)[] blocks =
    [
        ("src.File", File, [FileFact("store/keys.py"), FileFact("query/plan.py")]),

        ("src.Decl", Decl,
        [
            DeclFact("store/keys.py", "keys", "def", 12, "key_of"),
            // Zero, and a value past a single varint byte: zigzag is where a codec
            // that agrees on small numbers can still disagree.
            DeclFact("store/keys.py", "keys", "def", 0, "zero"),
            DeclFact("query/plan.py", "plan", "class", 2147483648, "Plan"),
        ]),

        ("src.Ref", Reference,
        [
            new(Reference, ApertureValue.Rec(
                ApertureValue.Of(ApertureRef.To(
                    DeclFact("store/keys.py", "keys", "def", 12, "key_of"))),
                ApertureValue.Of(ApertureRef.To(FileFact("query/plan.py"))),
                ApertureValue.Rec(ApertureValue.Of(19L), ApertureValue.Of(4L)))),
        ]),

        // A reference in the *middle* of a key, an integer after it, and a value side
        // behind all three — none of which the three blocks above put together.
        ("src.Param", Param,
        [
            new(Param,
                ApertureValue.Rec(
                    ApertureValue.Of(ApertureRef.To(
                        DeclFact("store/keys.py", "keys", "def", 12, "key_of"))),
                    ApertureValue.Of(0L),
                    ApertureValue.Of("key")),
                ApertureValue.Of("bytes")),

            // A negative, because zigzag is where two codecs that agree about every
            // positive integer can still disagree. A parameter is never at index -1;
            // this corpus is chosen for what it reaches, not for what it means.
            new(Param,
                ApertureValue.Rec(
                    ApertureValue.Of(ApertureRef.To(
                        DeclFact("store/keys.py", "keys", "def", 12, "key_of"))),
                    ApertureValue.Of(-1L),
                    ApertureValue.Of("rest")),
                ApertureValue.Of("int")),
        ]),

        // A key of one field, which encodes as the bare reference does — and would go
        // on doing so if either client quietly started framing records.
        ("src.Doc", Doc,
        [
            new(Doc,
                ApertureValue.Rec(ApertureValue.Of(ApertureRef.To(
                    DeclFact("query/plan.py", "plan", "class", 5, "Plan")))),
                ApertureValue.Of("A plan is an ordered list of steps.")),
        ]),
    ];

    List<string> lines =
    [
        "# Blocks produced by the .NET client, as hex — Phase 9e's acceptance criterion.",
        "# `aperture-client` encodes the same facts and must produce the same bytes.",
        "#",
        "# Regenerate with ./clients/dotnet/emit-golden.sh. A diff here is either a",
        "# deliberate format change — in which case both clients move together — or a",
        "# divergence, which is the thing this file exists to catch.",
        $"schema-fingerprint {schema.Fingerprint():x16}",
    ];

    foreach (var (name, predicate, facts) in blocks)
    {
        var bytes = Block.Encode(schema, predicate, facts);
        lines.Add($"block {name} {predicate} {Convert.ToHexString(bytes).ToLowerInvariant()}");
    }

    // `System.IO.File`, spelled out: `File` is the predicate id above.
    System.IO.File.WriteAllLines(path, lines);
    Console.WriteLine($"wrote {blocks.Length} golden blocks to {path}");
}

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
