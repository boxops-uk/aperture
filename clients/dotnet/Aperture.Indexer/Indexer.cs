using System.Diagnostics;
using System.Text;

using Aperture.Client;

using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.CSharp;
using Microsoft.CodeAnalysis.CSharp.Syntax;

namespace Aperture.Indexer;

/// <summary>
/// The walk: a compilation in, facts out.
/// </summary>
/// <remarks>
/// <para>
/// <b>Everything here is a symbol question, not a syntax question.</b> A name in C#
/// means whatever the compiler says it means — an extension method invoked as an
/// instance method, a partial class continued in another file, a member reached through
/// a type inferred from a lambda's parameter. Roslyn has already answered all of it, so
/// this walk asks rather than guesses; that is the difference between this indexer and
/// <c>example/index.py</c>, which is honest about stopping at the line where types
/// would be needed.
/// </para>
/// <para>
/// <b>One function decides what a declaration is.</b> <see cref="DeclFor"/> maps a
/// symbol to the <c>src.Decl</c> fact that names it, and both paths go through it: the
/// walk that emits declarations, and the reference that points at one. They cannot
/// disagree, because there is nothing for them to disagree with — the reference nests
/// the very fact the declaration emitted.
/// </para>
/// <para>
/// <b>No fact ids anywhere.</b> A reference carries its target inline and the server
/// interns it. So this class keeps no book of what the server has called things, and
/// the memoisation below is an optimisation rather than a correctness requirement:
/// forget all of it and the same index comes out, more slowly.
/// </para>
/// </remarks>
internal sealed class Indexer(Options options, FactSink sink, string root, ProjectIndex projects)
{
    /// <summary>A module fact, and a small integer to key sets and maps by.</summary>
    private sealed record Module(ApertureFact Fact, int Id);

    /// <summary>A declaration fact, the module it was declared in, and how often it was named.</summary>
    /// <remarks>
    /// <para>
    /// <see cref="Uses"/> is counted for one reason only: to have a name worth
    /// querying at the end of a run. Which declaration a repository leans on hardest is
    /// not knowable in advance, and a smoke query against an arbitrary name can quietly
    /// return nothing and look like it worked.
    /// </para>
    /// <para>
    /// <see cref="First"/> says this run is the one that settled the declaration's key,
    /// and it is what gates every fact carrying a <i>value</i> — the kind, the type, the
    /// doc comment. See <see cref="_kinds"/>: two declarations agreeing on a key and
    /// disagreeing on a value are a conflict the server is right to reject, and the
    /// cheapest way not to send one is to describe a key once.
    /// </para>
    /// </remarks>
    private sealed record Declared(ApertureFact Fact, Module Module, string Name, string Kind, bool First)
    {
        public int Uses { get; set; }
    }

    private readonly Dictionary<string, ApertureFact> _files = new(StringComparer.Ordinal);
    private readonly Dictionary<(string Path, string Namespace), Module> _modules = [];

    /// <summary>
    /// The kind already settled for a declaration key — <c>ops-I5</c>'s conflict rule,
    /// enforced on this side of the wire.
    /// </summary>
    /// <remarks>
    /// A <c>src.Decl</c> key is (module, line, name) and its value is the kind, so two
    /// declarations that agree on the key and differ on the kind are a same-key
    /// different-value conflict — which the server rejects, deterministically and by
    /// name, failing the stream carrying it. That is the right behaviour for a database
    /// and the wrong way to lose an hour of indexing, so the first kind seen wins here
    /// and the disagreement is counted and reported.
    /// </remarks>
    private readonly Dictionary<string, string> _kinds = new(StringComparer.Ordinal);

    /// <summary>Module-to-module edges already emitted, keyed by the two ids packed together.</summary>
    private readonly HashSet<long> _imports = [];

    /// <summary>Files already walked — the same file is often in two projects.</summary>
    private readonly HashSet<string> _walked = new(StringComparer.Ordinal);

    /// <summary>
    /// The one lock: everything above, the counters, and the sink.
    /// </summary>
    /// <remarks>
    /// One rather than several because the things it guards are reached through each
    /// other — a reference wants a declaration, which wants a module, which wants a
    /// file, and each may emit a fact. Several locks in that shape is an ordering
    /// problem nobody needs for critical sections this short.
    /// </remarks>
    private readonly Lock _gate = new();

    /// <summary>Ticks spent waiting to enter <see cref="_gate"/>, summed over all walkers.</summary>
    /// <remarks>
    /// The point of measuring it: the gate is the only thing eight walker threads share,
    /// so it is the ceiling on how much of the walk is actually parallel. Both counters
    /// are accumulated while the gate is *held*, so neither needs an interlocked add.
    /// </remarks>
    private long _gateWaitTicks;

    /// <summary>Ticks the gate was held, summed over all walkers.</summary>
    private long _gateHeldTicks;

    /// <summary>Total time walkers spent blocked on the gate.</summary>
    public TimeSpan GateWait => Stopwatch.GetElapsedTime(0, Interlocked.Read(ref _gateWaitTicks));

    /// <summary>Total time the gate was held.</summary>
    public TimeSpan GateHeld => Stopwatch.GetElapsedTime(0, Interlocked.Read(ref _gateHeldTicks));

    /// <summary>Enter the gate, timing the wait and the hold.</summary>
    private Guard Enter() => new(this);

    /// <summary>A timed <see cref="_gate"/> acquisition; dispose to release.</summary>
    private readonly struct Guard : IDisposable
    {
        private readonly Indexer _owner;
        private readonly long _entered;

        public Guard(Indexer owner)
        {
            _owner = owner;
            var before = Stopwatch.GetTimestamp();
            owner._gate.Enter();
            _entered = Stopwatch.GetTimestamp();
            // Safe unsynchronised: we hold the gate.
            owner._gateWaitTicks += _entered - before;
        }

        public void Dispose()
        {
            _owner._gateHeldTicks += Stopwatch.GetTimestamp() - _entered;
            _owner._gate.Exit();
        }
    }

    /// <summary>
    /// Symbol to declaration, for one compilation.
    /// </summary>
    /// <remarks>
    /// Per compilation because symbols are: the same type in two projects is two
    /// symbols, and <see cref="SymbolEqualityComparer"/> will say so. The facts they
    /// produce are identical, which is what makes it safe to throw this away between
    /// projects and what makes the duplicates dedup on the way in.
    /// </remarks>
    private Dictionary<ISymbol, Declared?> _declarations = new(SymbolEqualityComparer.Default);

    public int Files { get; private set; }

    public int Declarations { get; private set; }

    public int References { get; private set; }

    /// <summary>References to something declared outside the index — the BCL, a package.</summary>
    public int External { get; private set; }

    /// <summary>Names the compiler could not bind at all: missing references, broken code.</summary>
    public int Unresolved { get; private set; }

    /// <summary>Declaration keys reached with two different kinds. See <see cref="_kinds"/>.</summary>
    public int Conflicts { get; private set; }

    /// <summary>Lines of source written as <c>src.Line</c> facts.</summary>
    public long Lines { get; private set; }

    /// <summary>Files no project compiles — shared source, or a checkout with no project files.</summary>
    public int Unattributed { get; private set; }

    /// <summary>The most-referenced declaration's short name — something to query for.</summary>
    public string? SampleName { get; private set; }

    /// <summary>The most-referenced *method*, which is what a query about parameters needs.</summary>
    public string? SampleMethod { get; private set; }

    private int _sampleUses;

    private int _sampleMethodUses;

    public bool Exhausted => options.MaxFiles > 0 && _claimed >= options.MaxFiles;

    /// <summary>Files handed to the walk, which is what `--max-files` counts.</summary>
    private int _claimed;

    /// <summary>
    /// Index one project, reporting each file as it is finished.
    /// </summary>
    /// <remarks>
    /// <para>
    /// <b>Which files, decided in order; then walked in parallel.</b> Asking a symbol
    /// what it means is most of the cost of indexing, a <see cref="Compilation"/> is
    /// thread-safe, and separate semantic models over one compilation are exactly what
    /// an IDE runs concurrently all day. What must not become concurrent is <i>which</i>
    /// files: choosing them sequentially is what keeps <c>--max-files 2000</c> the same
    /// two thousand files on every run.
    /// </para>
    /// <para>
    /// Everything downstream of the symbol — the memos, the counters, the sink — is
    /// under one lock. The critical sections are dictionary work between binding calls
    /// that are far longer, and holding the sink's flush inside the lock is
    /// backpressure rather than a cost: a write stream is one at a time anyway.
    /// </para>
    /// </remarks>
    public void Index(Compilation compilation, Action<string>? onFile = null)
    {
        _declarations = new Dictionary<ISymbol, Declared?>(SymbolEqualityComparer.Default);

        var walking = new List<(SyntaxTree Tree, string Path)>();

        foreach (var tree in compilation.SyntaxTrees)
        {
            if (Exhausted)
            {
                break;
            }

            var path = Relative(tree.FilePath);

            if (path is null || !_walked.Add(path))
            {
                continue;
            }

            walking.Add((tree, path));
            _claimed++;
        }

        Parallel.ForEach(
            walking,
            new ParallelOptions { MaxDegreeOfParallelism = options.Jobs },
            item =>
            {
                IndexTree(compilation.GetSemanticModel(item.Tree), item.Tree, item.Path);

                using (Enter())
                {
                    Files++;
                    onFile?.Invoke(item.Path);
                }
            });
    }

    private void IndexTree(SemanticModel model, SyntaxTree tree, string path)
    {
        var syntax = tree.GetRoot();
        ApertureFact file;
        Module here;

        using (Enter())
        {
            file = FileOf(path);

            // The file's own module, for the dependency edges below. A file may declare
            // more than one namespace — each declaration is filed under its own — but
            // the edge "this file depends on that one" needs a single end, and the first
            // namespace in the file is the one that names it.
            here = ModuleFor(path, PrimaryNamespace(syntax));
        }

        IndexLines(tree, file);

        foreach (var node in syntax.DescendantNodes())
        {
            switch (node)
            {
                // Every form of declaration that has a symbol of its own. `GetDeclaredSymbol`
                // is what turns each into one, so the list is about *reaching* them.
                case BaseTypeDeclarationSyntax:
                case DelegateDeclarationSyntax:
                case BaseMethodDeclarationSyntax:
                case BasePropertyDeclarationSyntax:
                case EnumMemberDeclarationSyntax:
                case LocalFunctionStatementSyntax:
                    Declare(model, node);
                    break;

                // `int a, b;` is one field declaration and two fields, and the symbol
                // hangs off the declarator rather than the statement.
                case VariableDeclaratorSyntax declarator
                    when declarator.Parent?.Parent is BaseFieldDeclarationSyntax:
                    Declare(model, declarator);
                    break;

                case SimpleNameSyntax name when options.References:
                    Reference(model, name, file, here);
                    break;
            }
        }
    }

    /// <summary>
    /// The file's line table: one <c>src.Line</c> fact per line, the text on the value
    /// side.
    /// </summary>
    /// <remarks>
    /// <para>
    /// <b>Every line, including the blank ones.</b> A line table whose gaps mean
    /// "empty" is a table a consumer has to know a rule about, and the rule is
    /// indistinguishable from "that line was never indexed". Completeness is the
    /// property that makes it a table.
    /// </para>
    /// <para>
    /// Built outside the lock and added inside it. A large file is a few thousand facts,
    /// and holding the sink — whose flush is a write stream — across the construction of
    /// all of them would serialise the walk behind the network.
    /// </para>
    /// </remarks>
    private void IndexLines(SyntaxTree tree, ApertureFact file)
    {
        if (!options.Lines)
        {
            return;
        }

        var text = tree.GetText();
        var facts = new List<ApertureFact>(text.Lines.Count);

        foreach (var line in text.Lines)
        {
            facts.Add(CodeIndex.LineFact(file, line.LineNumber + 1, Clip(line.ToString())));
        }

        using (Enter())
        {
            foreach (var fact in facts)
            {
                sink.Add(CodeIndex.Line, fact);
            }

            Lines += facts.Count;
        }
    }

    private void Declare(SemanticModel model, SyntaxNode node)
    {
        if (model.GetDeclaredSymbol(node) is { } symbol)
        {
            using (Enter())
            {
                // The fact is emitted by `DeclFor` the first time the symbol is reached,
                // by whichever path reaches it first. Here that is its own declaration.
                DeclFor(symbol);
            }
        }
    }

    private void Reference(SemanticModel model, SimpleNameSyntax name, ApertureFact file, Module here)
    {
        var info = model.GetSymbolInfo(name);

        // A single candidate is an ambiguity the compiler declined to resolve but a
        // reader would read straight through — an inaccessible member, a failed
        // overload. Several candidates is a genuine ambiguity, and guessing would put a
        // wrong edge in the graph.
        var symbol = info.Symbol
            ?? (info.CandidateSymbols.Length == 1 ? info.CandidateSymbols[0] : null);

        if (symbol is null)
        {
            using (Enter())
            {
                Unresolved++;
            }

            return;
        }

        // Namespaces, locals, parameters, type parameters, labels, ranges: real symbols
        // that are not declarations this index holds.
        if (symbol.Kind is not (SymbolKind.NamedType or SymbolKind.Method
            or SymbolKind.Property or SymbolKind.Field or SymbolKind.Event))
        {
            return;
        }

        // The symbol is resolved; from here on it is bookkeeping, and bookkeeping is
        // shared.
        using (Enter())
        {
            if (DeclFor(symbol) is not { } target)
            {
                External++;
                return;
            }

            // **The identifier's extent, not the whole expression's.** A viewer draws
            // the link over the name someone can click, so `Foo.Bar` is two references
            // rather than one span covering both.
            var span = name.Identifier.GetLocation().GetLineSpan();
            var at = span.StartLinePosition;
            var length = name.Identifier.Span.Length;

            sink.Add(CodeIndex.Ref, CodeIndex.RefFact(
                at.Line + 1, at.Character + 1, length, file, target.Fact));

            // The same reference, keyed by file and position. Written twice because a
            // predicate leads with one field: find-references needs the target to lead
            // and a file view needs the file to, and until a derived predicate can be
            // declared the producer is what states the second order.
            sink.Add(CodeIndex.FileXRef, CodeIndex.FileXRefFact(
                at.Line + 1, at.Character + 1, length, file, target.Fact));

            References++;

            if (++target.Uses > _sampleUses)
            {
                _sampleUses = target.Uses;
                SampleName = target.Name;
            }

            // A second sample, kept separately because the most-used declaration in a
            // repository is nearly always a type — and a type has no parameters, so the
            // query that demonstrates `src.Param` would return nothing and look broken.
            if (target.Kind == "method" && target.Uses > _sampleMethodUses)
            {
                _sampleMethodUses = target.Uses;
                SampleMethod = target.Name;
            }

            // The dependency edge the reference implies. In C# a `using` names a
            // namespace, which is declared across many files and says nothing about
            // which of them this one needs; what carries that is where the names
            // actually resolved to.
            if (target.Module.Id != here.Id)
            {
                var edge = ((long)here.Id << 32) | (uint)target.Module.Id;

                if (_imports.Add(edge))
                {
                    sink.Add(CodeIndex.Import, CodeIndex.ImportFact(here.Fact, target.Module.Fact));
                }
            }
        }
    }

    /// <summary>The declaration a symbol names, or nothing if it is not in this index.</summary>
    /// <remarks>Called with <see cref="_gate"/> held: it memoises, counts and emits.</remarks>
    private Declared? DeclFor(ISymbol symbol)
    {
        Debug.Assert(_gate.IsHeldByCurrentThread, "the declaration memo is shared");

        symbol = Canonical(symbol);

        if (_declarations.TryGetValue(symbol, out var known))
        {
            return known;
        }

        var built = Build(symbol);

        // **The memo is written before the graph is walked, and that is load-bearing.**
        // Describing a declaration reaches its container, its base type and the
        // interface members it implements — every one of which is a declaration this
        // function is then asked for. Containment is a tree and inheritance is a DAG in
        // code that compiles; this indexer is pointed at code that sometimes does not,
        // and a cycle would otherwise recurse until the stack ran out.
        _declarations[symbol] = built;

        if (built is { First: true })
        {
            Describe(symbol, built);
        }

        return built;
    }

    /// <summary>
    /// Everything about a declaration that is not the declaration: what contains it,
    /// what it extends and implements, what it overrides, its parameters, its type, its
    /// doc comment, its attributes.
    /// </summary>
    /// <remarks>
    /// <para>
    /// <b>This is the half a syntax walk cannot do.</b> Every question here is asked of
    /// a symbol — what a base type resolves to across projects, which interface member
    /// a method implicitly implements, what a parameter's type is after inference — and
    /// the answers are what make the index a graph rather than a list of names.
    /// </para>
    /// <para>
    /// Called with <see cref="_gate"/> held, once per declaration key, from
    /// <see cref="DeclFor"/> — see there for why it is called after the memo is written.
    /// </para>
    /// </remarks>
    private void Describe(ISymbol symbol, Declared declared)
    {
        Debug.Assert(_gate.IsHeldByCurrentThread, "the declaration memo is shared");

        // Containment. `src.Decl`'s name is already qualified by its containing types,
        // which is how a person reads the nesting; this is how a query joins on it.
        if (symbol.ContainingType is { } containing && DeclFor(containing) is { } parent)
        {
            sink.Add(CodeIndex.Member, CodeIndex.MemberFact(parent.Fact, declared.Fact));
        }

        foreach (var attribute in symbol.GetAttributes())
        {
            // Resolved only, and here the reason is about *keys* rather than values: an
            // attribute the compiler could not bind displays as whatever the source
            // wrote, so `[Obsolete]` and `System.ObsoleteAttribute` would be two keys
            // for one attribute and a search for either would miss the other.
            if (attribute.AttributeClass is { } applied
                && Known(applied)
                && applied.OriginalDefinition.ToDisplayString() is { Length: > 0 } name)
            {
                sink.Add(CodeIndex.Attribute, CodeIndex.AttributeFact(name, declared.Fact));
                sink.Add(CodeIndex.AttributeOf, CodeIndex.AttributeOfFact(declared.Fact, name));
            }
        }

        if (options.Docs && DocComment(symbol) is { } doc)
        {
            sink.Add(CodeIndex.Doc, CodeIndex.DocFact(declared.Fact, doc));
        }

        switch (symbol)
        {
            case INamedTypeSymbol type:
                DescribeType(type, declared);
                break;

            case IMethodSymbol method:
                Parameters(declared, method.Parameters);

                // A constructor's return type is `void` because Roslyn has to say
                // something, not because the constructor returns anything.
                if (method.MethodKind is not (MethodKind.Constructor or MethodKind.StaticConstructor
                    or MethodKind.Destructor))
                {
                    TypeOf(declared, method.ReturnType);
                }

                Overrides(declared, method.OverriddenMethod);
                break;

            case IPropertySymbol property:
                // An indexer's parameters are its subscript — `this[int index]`.
                Parameters(declared, property.Parameters);
                TypeOf(declared, property.Type);
                Overrides(declared, property.OverriddenProperty);
                break;

            case IFieldSymbol field:
                TypeOf(declared, field.Type);
                break;

            case IEventSymbol @event:
                TypeOf(declared, @event.Type);
                Overrides(declared, @event.OverriddenEvent);
                break;
        }
    }

    /// <summary>What a type extends, what it implements, and who implements it.</summary>
    /// <remarks>
    /// <para>
    /// <b><c>AllInterfaces</c>, not the interfaces the declaration lists.</b> A type
    /// that says <c>: List&lt;T&gt;</c> is an <c>IEnumerable</c>, and a query asking for
    /// every enumerable in a repository is asking the semantic question. There is no
    /// recursion in focus to close a transitive relation with afterwards, so the
    /// closure is written down — which is a decision this schema makes twice, the other
    /// being <c>src.SearchByName</c>.
    /// </para>
    /// <para>
    /// <c>System.Object</c> is skipped: every class extends it, the edge distinguishes
    /// nothing, and in an index <i>of</i> the framework it would be one key with a
    /// hundred thousand rows under it.
    /// </para>
    /// </remarks>
    private void DescribeType(INamedTypeSymbol type, Declared declared)
    {
        if (type.BaseType is { SpecialType: not SpecialType.System_Object } @base
            && DeclFor(@base) is { } extended)
        {
            sink.Add(CodeIndex.Extends, CodeIndex.ExtendsFact(extended.Fact, declared.Fact));
            sink.Add(CodeIndex.DerivesFrom, CodeIndex.DerivesFromFact(declared.Fact, extended.Fact));
        }

        foreach (var iface in type.AllInterfaces)
        {
            if (DeclFor(iface) is { } implemented)
            {
                sink.Add(CodeIndex.Implements, CodeIndex.ImplementsFact(implemented.Fact, declared.Fact));
            }

            // **Implicit implementation is the common case in C#**, and there is nothing
            // in the syntax that says a method implements an interface member — only the
            // compiler knows. Asked here, per type, rather than per member: the answer
            // is a lookup on the type either way, and a member-by-member sweep would ask
            // the same question once for every interface member the type has.
            foreach (var member in iface.GetMembers())
            {
                if (type.FindImplementationForInterfaceMember(member) is not { } implementation
                    || !SymbolEqualityComparer.Default.Equals(implementation.ContainingType, type))
                {
                    // Either nothing implements it — an abstract class may leave it — or
                    // a base type does, in which case the edge belongs to that type and
                    // is emitted when it is described.
                    continue;
                }

                if (DeclFor(member) is { } required && DeclFor(implementation) is { } provided)
                {
                    sink.Add(CodeIndex.Override, CodeIndex.OverrideFact(required.Fact, provided.Fact));
                }
            }
        }

        // A delegate's signature is on the method it invokes, which has no declaration
        // of its own for the walk to reach.
        if (type.DelegateInvokeMethod is { } invoke)
        {
            Parameters(declared, invoke.Parameters);
            TypeOf(declared, invoke.ReturnType);
        }
    }

    private void Parameters(Declared declared, IReadOnlyList<IParameterSymbol> parameters)
    {
        for (var index = 0; index < parameters.Count; index++)
        {
            if (!Known(parameters[index].Type))
            {
                continue;
            }

            sink.Add(
                CodeIndex.Param,
                CodeIndex.ParamFact(
                    declared.Fact,
                    index,
                    parameters[index].Name,
                    Clip(parameters[index].Type.ToDisplayString())));
        }
    }

    /// <summary>
    /// Whether the compiler resolved this type, rather than leaving the name it could
    /// not bind.
    /// </summary>
    /// <remarks>
    /// <para>
    /// <b>An unresolved type is not merely imprecise — it is a conflict waiting to
    /// happen.</b> A type and a doc comment are <i>values</i>, and `ops-I5` rejects a
    /// second value for a key that already has one. Resolved, a type displays the same
    /// everywhere: <c>System.Collections.Generic.List&lt;T&gt;</c>. Unresolved, it
    /// displays as whatever the source happened to write — so the same declaration
    /// reached from a run that resolved it and a run that did not is one key with two
    /// answers, and the server is right to fail the stream carrying the second.
    /// </para>
    /// <para>
    /// That is not hypothetical: it is the ordinary case when a checkout is indexed in
    /// slices (<c>--skip-files</c>), where a declaration is walked by one run and merely
    /// referenced by another.
    /// </para>
    /// </remarks>
    private static bool Known(ITypeSymbol type) => type.TypeKind != TypeKind.Error;

    /// <summary>
    /// The type a declaration says it is, as a <i>spelling</i> rather than as an
    /// identity.
    /// </summary>
    /// <remarks>
    /// `ReadOnlySpan&lt;byte&gt;` is what a reader wants shown and never what a query
    /// filters by — the identity is already in the index, since the type name in a
    /// declaration is an ordinary reference that the walk resolves like any other.
    /// </remarks>
    private void TypeOf(Declared declared, ITypeSymbol type)
    {
        if (Known(type))
        {
            sink.Add(CodeIndex.TypeOf, CodeIndex.TypeOfFact(declared.Fact, Clip(type.ToDisplayString())));
        }
    }

    private void Overrides(Declared declared, ISymbol? overridden)
    {
        if (overridden is not null && DeclFor(overridden) is { } target)
        {
            sink.Add(CodeIndex.Override, CodeIndex.OverrideFact(target.Fact, declared.Fact));
        }
    }

    /// <summary>
    /// The symbol a name really means, for the purpose of pointing at a declaration.
    /// </summary>
    private static ISymbol Canonical(ISymbol symbol)
    {
        // `List<int>.Add` and `List<T>.Add` are the same declaration.
        symbol = symbol.OriginalDefinition;

        // `items.Where(...)` calls a static method whose first parameter is `items`.
        if (symbol is IMethodSymbol { ReducedFrom: { } reduced })
        {
            symbol = reduced.OriginalDefinition;
        }

        // `get_Length` is not a declaration; `Length` is.
        if (symbol is IMethodSymbol { AssociatedSymbol: { } associated })
        {
            symbol = associated.OriginalDefinition;
        }

        // A member with no syntax of its own — a default constructor, a record's
        // generated `Equals` — is named by the type that produced it. A positional
        // record's properties are *not* caught here: each has a parameter to point at,
        // which is where someone reading the code would look.
        while (symbol.DeclaringSyntaxReferences.Length == 0 && symbol.ContainingType is { } containing)
        {
            symbol = containing.OriginalDefinition;
        }

        return symbol;
    }

    private Declared? Build(ISymbol symbol)
    {
        if (symbol.DeclaringSyntaxReferences.FirstOrDefault() is not { } declaration)
        {
            // Metadata: a type from a package or the framework. Nothing in this index
            // is it, and inventing a declaration for it would put a file fact in the
            // database naming a path that does not exist.
            return null;
        }

        if (Relative(declaration.SyntaxTree.FilePath) is not { } path)
        {
            return null;
        }

        // The whole declaration's extent, and separately where its *name* starts — a
        // viewer highlights the identifier and folds the body, which are two spans.
        var syntax = declaration.GetSyntax();
        var nameSpan = NameLocation(syntax).GetLineSpan();
        var wholeSpan = syntax.GetLocation().GetLineSpan();

        var line = nameSpan.StartLinePosition.Line + 1;
        var module = ModuleFor(path, NamespaceOf(symbol));
        var name = QualifiedName(symbol);
        var kind = KindOf(symbol);
        var first = true;

        var key = $"{module.Id}\0{line}\0{name}";

        if (_kinds.TryGetValue(key, out var settled))
        {
            // Not the first symbol to reach this key, which is the whole of what
            // `First` tells `Describe`: a type, a doc comment and a kind are all
            // *values*, and a second answer for a settled key is the same-key
            // different-value conflict `ops-I5` fails the stream over.
            first = false;

            if (settled != kind)
            {
                // Keep the first answer rather than send the server two. See `_kinds`.
                Conflicts++;
                kind = settled;
            }
        }
        else
        {
            _kinds[key] = kind;
        }

        var fact = CodeIndex.DeclFact(line, module.Fact, name, kind);
        var simple = SimpleName(symbol);

        sink.Add(CodeIndex.Decl, fact);

        // The span, only for the symbol that *owns* this key. A partial class reaches
        // here once per part and they disagree about the extent; `first` is already the
        // flag for "this is the answer that settled the key", and a span is an attribute
        // of a declaration exactly as its type and doc comment are.
        if (first)
        {
            sink.Add(CodeIndex.DeclSpan, CodeIndex.DeclSpanFact(
                fact,
                nameSpan.StartLinePosition.Character + 1,
                wholeSpan.EndLinePosition.Line + 1,
                wholeSpan.EndLinePosition.Character + 1));
        }

        // The same declaration keyed the other way round, so a prefix of a *name* is a
        // range rather than a filter over every declaration in the database. A
        // declaration's own key begins with its module, which is why this predicate
        // exists at all — and the name here is the short one, which is what someone
        // searching types.
        sink.Add(CodeIndex.SearchByName, CodeIndex.SearchFact(simple, fact));

        // And once more folded, because focus has no `toLower` to apply at read time —
        // invariant rather than current-culture, since the server compares bytes and
        // has no notion of a culture.
        sink.Add(CodeIndex.SearchByLowerName,
            CodeIndex.SearchLowerFact(simple.ToLowerInvariant(), fact));

        Declarations++;
        return new Declared(fact, module, simple, kind, first);
    }

    private ApertureFact FileOf(string path)
    {
        Debug.Assert(_gate.IsHeldByCurrentThread, "the file memo is shared");

        if (_files.TryGetValue(path, out var known))
        {
            return known;
        }

        var fact = CodeIndex.FileFact(path);
        _files[path] = fact;
        sink.Add(CodeIndex.File, fact);

        // **What compiles this file** — here rather than in the walk, because a file
        // fact is also created for a file nobody walked: a declaration in another
        // project, reached through a reference, names one.
        var owners = projects.Owners(path);

        foreach (var project in owners)
        {
            sink.Add(CodeIndex.ProjectSource, CodeIndex.ProjectSourceFact(fact, project.Fact));
        }

        if (owners.Count == 0)
        {
            Unattributed++;
        }

        return fact;
    }

    private Module ModuleFor(string path, string name)
    {
        Debug.Assert(_gate.IsHeldByCurrentThread, "the module memo is shared");

        if (_modules.TryGetValue((path, name), out var known))
        {
            return known;
        }

        var fact = CodeIndex.ModuleFact(FileOf(path), name);
        var module = new Module(fact, _modules.Count);
        _modules[(path, name)] = module;
        sink.Add(CodeIndex.Module, fact);
        return module;
    }

    /// <summary>The path a fact names it by, or nothing if this file is not indexed.</summary>
    private string? Relative(string? absolute)
    {
        if (string.IsNullOrEmpty(absolute))
        {
            // Generated syntax with no file behind it — a source generator's output in
            // memory, or a tree parsed from a string.
            return null;
        }

        var relative = Path.GetRelativePath(root, absolute).Replace(Path.DirectorySeparatorChar, '/');

        // Build output, not source. `obj/` in particular holds the generated assembly
        // attributes every project has, which would be the same six declarations in
        // every project and none of them anything anyone wants to find.
        return relative.Contains("/obj/", StringComparison.Ordinal)
            || relative.Contains("/bin/", StringComparison.Ordinal)
            || relative.StartsWith("obj/", StringComparison.Ordinal)
            || relative.StartsWith("bin/", StringComparison.Ordinal)
            ? null
            : relative;
    }

    /// <summary>The namespace a symbol is declared in, spelled as it is written.</summary>
    private static string NamespaceOf(ISymbol symbol) =>
        symbol.ContainingNamespace is { IsGlobalNamespace: false } containing
            ? containing.ToDisplayString()
            : "<global>";

    /// <summary>
    /// Where the name is, rather than where the declaration starts.
    /// </summary>
    /// <remarks>
    /// A declaration's syntax node begins at its first attribute or modifier, so a
    /// documented method's line would be the line of its <c>[Obsolete]</c>. A row of
    /// this index is somewhere a person is meant to be able to open, so it points at
    /// the identifier.
    /// </remarks>
    private static Location NameLocation(SyntaxNode node) => node switch
    {
        BaseTypeDeclarationSyntax type => type.Identifier.GetLocation(),
        DelegateDeclarationSyntax @delegate => @delegate.Identifier.GetLocation(),
        MethodDeclarationSyntax method => method.Identifier.GetLocation(),
        ConstructorDeclarationSyntax constructor => constructor.Identifier.GetLocation(),
        DestructorDeclarationSyntax destructor => destructor.Identifier.GetLocation(),
        OperatorDeclarationSyntax @operator => @operator.OperatorToken.GetLocation(),
        ConversionOperatorDeclarationSyntax conversion => conversion.Type.GetLocation(),
        PropertyDeclarationSyntax property => property.Identifier.GetLocation(),
        IndexerDeclarationSyntax indexer => indexer.ThisKeyword.GetLocation(),
        EventDeclarationSyntax @event => @event.Identifier.GetLocation(),
        EnumMemberDeclarationSyntax member => member.Identifier.GetLocation(),
        VariableDeclaratorSyntax declarator => declarator.Identifier.GetLocation(),
        LocalFunctionStatementSyntax local => local.Identifier.GetLocation(),
        ParameterSyntax parameter => parameter.Identifier.GetLocation(),
        _ => node.GetLocation(),
    };

    /// <summary>The name someone searching would type.</summary>
    private static string SimpleName(ISymbol symbol) => symbol switch
    {
        IMethodSymbol { MethodKind: MethodKind.Constructor } => "ctor",
        IMethodSymbol { MethodKind: MethodKind.StaticConstructor } => "cctor",
        IMethodSymbol { MethodKind: MethodKind.Destructor } => "finalize",
        IMethodSymbol { MethodKind: MethodKind.UserDefinedOperator or MethodKind.Conversion } method =>
            method.Name.StartsWith("op_", StringComparison.Ordinal) ? method.Name[3..] : method.Name,
        IPropertySymbol { IsIndexer: true } => "this[]",
        _ => symbol.Name,
    };

    /// <summary>
    /// The name qualified by the types it is nested in — <c>Store.Cursor.Next</c> — but
    /// not by its namespace, which the module it points at already names.
    /// </summary>
    /// <remarks>
    /// A constructor is <c>Store.ctor</c> rather than <c>Store</c> deliberately: a type
    /// and its constructor declared on one line would otherwise be one key with two
    /// kinds, which is a conflict the server is right to reject.
    /// </remarks>
    private static string QualifiedName(ISymbol symbol)
    {
        var qualified = new StringBuilder(SimpleName(symbol));

        for (var type = symbol.ContainingType; type is not null; type = type.ContainingType)
        {
            qualified.Insert(0, '.').Insert(0, type.Name);
        }

        return qualified.ToString();
    }

    /// <summary>
    /// The doc comment somebody wrote above a declaration, stripped of its slashes.
    /// </summary>
    /// <remarks>
    /// <para>
    /// The trivia rather than <c>GetDocumentationCommentXml</c>: that builds and
    /// formats an XML document per symbol, and what a search result wants to show is
    /// what a person typed. The tags are left in — they are part of what was written,
    /// and a consumer that wants only the summary can find it.
    /// </para>
    /// <para>
    /// <b>A partial type documented in two files gets one of them.</b> The declaration
    /// this reads is the one <c>src.Decl</c>'s line came from, so the fact and its
    /// value name the same place, which is the property that matters.
    /// </para>
    /// </remarks>
    private static string? DocComment(ISymbol symbol)
    {
        if (symbol.DeclaringSyntaxReferences.FirstOrDefault()?.GetSyntax() is not { } node)
        {
            return null;
        }

        // **A field's comment is above the declaration, not above the declarator.**
        // `int a, b;` is one declaration and two declarators, the symbol hangs off the
        // declarator, and the trivia is on the statement two levels up — so asking the
        // declarator finds nothing, silently, for every documented field in a codebase.
        if (node is VariableDeclaratorSyntax { Parent.Parent: BaseFieldDeclarationSyntax field })
        {
            node = field;
        }

        StringBuilder? text = null;

        foreach (var trivia in node.GetLeadingTrivia())
        {
            if (!trivia.IsKind(SyntaxKind.SingleLineDocumentationCommentTrivia)
                && !trivia.IsKind(SyntaxKind.MultiLineDocumentationCommentTrivia))
            {
                continue;
            }

            text ??= new StringBuilder();

            foreach (var line in trivia.ToFullString().Split('\n'))
            {
                var stripped = line.TrimStart().TrimStart('/', '*').Trim();

                if (stripped.Length > 0)
                {
                    if (text.Length > 0)
                    {
                        text.Append('\n');
                    }

                    text.Append(stripped);
                }
            }
        }

        return text is { Length: > 0 } ? Clip(text.ToString()) : null;
    }

    /// <summary>
    /// A string bounded, because a block is bounded.
    /// </summary>
    /// <remarks>
    /// A block carries up to <c>--batch</c> facts in one frame, and a frame's payload
    /// caps at 64 MiB — so a generated file with a megabyte on one line, or a doc
    /// comment holding an entire specification, is a stream failure rather than a large
    /// fact. Four thousand characters is past anything a person writes on a line and
    /// leaves the default batch two orders of magnitude clear of the cap.
    /// </remarks>
    private static string Clip(string text) =>
        text.Length <= MaxText ? text : text[..MaxText];

    private const int MaxText = 4096;

    private static string KindOf(ISymbol symbol) => symbol switch
    {
        INamedTypeSymbol type => type.TypeKind switch
        {
            TypeKind.Class => type.IsRecord ? "record" : "class",
            TypeKind.Struct => type.IsRecord ? "record struct" : "struct",
            TypeKind.Interface => "interface",
            TypeKind.Enum => "enum",
            TypeKind.Delegate => "delegate",
            _ => "type",
        },

        IMethodSymbol method => method.MethodKind switch
        {
            MethodKind.Constructor or MethodKind.StaticConstructor => "ctor",
            MethodKind.Destructor => "dtor",
            MethodKind.UserDefinedOperator or MethodKind.Conversion => "operator",
            MethodKind.LocalFunction => "local function",
            _ => "method",
        },

        IPropertySymbol property => property.IsIndexer ? "indexer" : "property",

        IFieldSymbol field =>
            field.ContainingType?.TypeKind == TypeKind.Enum ? "enum member"
            : field.IsConst ? "const"
            : "field",

        IEventSymbol => "event",

        _ => "declaration",
    };

    /// <summary>The first namespace the file declares, or the global one.</summary>
    private static string PrimaryNamespace(SyntaxNode syntax)
    {
        // A namespace declaration is a child of the compilation unit, so this is a scan
        // of the top level rather than of the file.
        foreach (var node in syntax.ChildNodes())
        {
            if (node is BaseNamespaceDeclarationSyntax declaration)
            {
                return declaration.Name.ToString();
            }
        }

        return "<global>";
    }
}
