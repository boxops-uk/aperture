using System.Text;

using Aperture.Client;

using Microsoft.CodeAnalysis;
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
internal sealed class Indexer(Options options, FactSink sink, string root)
{
    /// <summary>A module fact, and a small integer to key sets and maps by.</summary>
    private sealed record Module(ApertureFact Fact, int Id);

    /// <summary>A declaration fact, the module it was declared in, and how often it was named.</summary>
    /// <remarks>
    /// <paramref name="Uses"/> is counted for one reason only: to have a name worth
    /// querying at the end of a run. Which declaration a repository leans on hardest is
    /// not knowable in advance, and a smoke query against an arbitrary name can quietly
    /// return nothing and look like it worked.
    /// </remarks>
    private sealed record Declared(ApertureFact Fact, Module Module, string Name)
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

    /// <summary>The most-referenced declaration's short name — something to query for.</summary>
    public string? SampleName { get; private set; }

    private int _sampleUses;

    public bool Exhausted => options.MaxFiles > 0 && Files >= options.MaxFiles;

    /// <summary>Index one project, reporting each file as it is finished.</summary>
    public void Index(LoadedProject project, Action<string>? onFile = null)
    {
        _declarations = new Dictionary<ISymbol, Declared?>(SymbolEqualityComparer.Default);

        foreach (var tree in project.Compilation.SyntaxTrees)
        {
            if (Exhausted)
            {
                return;
            }

            var path = Relative(tree.FilePath);

            if (path is null || !_walked.Add(path))
            {
                continue;
            }

            IndexTree(project.Compilation.GetSemanticModel(tree), tree, path);

            Files++;
            onFile?.Invoke(path);
        }
    }

    private void IndexTree(SemanticModel model, SyntaxTree tree, string path)
    {
        var syntax = tree.GetRoot();
        var file = FileOf(path);

        // The file's own module, for the dependency edges below. A file may declare
        // more than one namespace — each declaration is filed under its own — but the
        // edge "this file depends on that one" needs a single end, and the first
        // namespace in the file is the one that names it.
        var here = ModuleFor(path, PrimaryNamespace(syntax));

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

    private void Declare(SemanticModel model, SyntaxNode node)
    {
        if (model.GetDeclaredSymbol(node) is { } symbol)
        {
            // The fact is emitted by `DeclFor` the first time the symbol is reached, by
            // whichever path reaches it first. Here that is its own declaration.
            DeclFor(symbol);
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
            Unresolved++;
            return;
        }

        // Namespaces, locals, parameters, type parameters, labels, ranges: real symbols
        // that are not declarations this index holds.
        if (symbol.Kind is not (SymbolKind.NamedType or SymbolKind.Method
            or SymbolKind.Property or SymbolKind.Field or SymbolKind.Event))
        {
            return;
        }

        if (DeclFor(symbol) is not { } target)
        {
            External++;
            return;
        }

        var at = name.Identifier.GetLocation().GetLineSpan().StartLinePosition;
        sink.Add(CodeIndex.Ref, CodeIndex.RefFact(at.Line + 1, at.Character + 1, file, target.Fact));
        References++;

        if (++target.Uses > _sampleUses)
        {
            _sampleUses = target.Uses;
            SampleName = target.Name;
        }

        // The dependency edge the reference implies. In C# a `using` names a namespace,
        // which is declared across many files and says nothing about which of them this
        // one needs; what carries that is where the names actually resolved to.
        if (target.Module.Id != here.Id)
        {
            var edge = ((long)here.Id << 32) | (uint)target.Module.Id;

            if (_imports.Add(edge))
            {
                sink.Add(CodeIndex.Import, CodeIndex.ImportFact(here.Fact, target.Module.Fact));
            }
        }
    }

    /// <summary>The declaration a symbol names, or nothing if it is not in this index.</summary>
    private Declared? DeclFor(ISymbol symbol)
    {
        symbol = Canonical(symbol);

        if (_declarations.TryGetValue(symbol, out var known))
        {
            return known;
        }

        var built = Build(symbol);
        _declarations[symbol] = built;
        return built;
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

        var line = NameLocation(declaration.GetSyntax()).GetLineSpan().StartLinePosition.Line + 1;
        var module = ModuleFor(path, NamespaceOf(symbol));
        var name = QualifiedName(symbol);
        var kind = KindOf(symbol);

        var key = $"{module.Id} {line} {name}";

        if (_kinds.TryGetValue(key, out var settled))
        {
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

        // The same declaration keyed the other way round, so a prefix of a *name* is a
        // range rather than a filter over every declaration in the database. A
        // declaration's own key begins with its module, which is why this predicate
        // exists at all — and the name here is the short one, which is what someone
        // searching types.
        sink.Add(CodeIndex.SearchByName, CodeIndex.SearchFact(simple, fact));

        Declarations++;
        return new Declared(fact, module, simple);
    }

    private ApertureFact FileOf(string path)
    {
        if (_files.TryGetValue(path, out var known))
        {
            return known;
        }

        var fact = CodeIndex.FileFact(path);
        _files[path] = fact;
        sink.Add(CodeIndex.File, fact);
        return fact;
    }

    private Module ModuleFor(string path, string name)
    {
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
