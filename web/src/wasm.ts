// Loading the engine.
//
// The module is built by `scripts/build-wasm.sh` into `src/wasm/`, which is
// **not** checked in: a binary in git is a binary somebody has to trust, and
// the build is one command. A checkout without it fails loudly here rather than
// silently falling back to a JavaScript imitation of the lexer — the imitation
// is the thing this replaces.
import init, {
  compile,
  sample_schema,
  samples,
  schema,
  tokens,
  tree,
  version,
} from './wasm/fjord_wasm.js'
import wasmUrl from './wasm/fjord_wasm_bg.wasm?url'

export type Span = { start: number; end: number }

export type TokenClass =
  | 'keyword'
  | 'predicate'
  | 'variable'
  | 'field'
  | 'number'
  | 'string'
  | 'wildcard'
  | 'punctuation'
  | 'whitespace'
  | 'error'

export type TokenView = {
  kind: string
  class: TokenClass
  span: Span
  text: string
}

export type Label = { span: Span; primary: boolean }

export type DiagnosticView = {
  code: string | null
  message: string
  labels: Label[]
}

export type Tokens = { tokens: TokenView[]; diagnostics: DiagnosticView[] }

export type TreeNode = {
  id: number
  /** The grammar rule (`Stmt`, `FactPattern`) or the token (`QId`, `LBrace`). */
  kind: string
  token: boolean
  /** A token's text; absent for a rule, whose text is its span of the source. */
  label: string | null
  span: Span
  children: number[]
}

/** `root` is null when the parse was refused outright rather than recovered. */
export type Tree = { root: number | null; nodes: TreeNode[]; diagnostics: DiagnosticView[] }

export type PredicateView = { id: number; name: string; ty: string }

/** `ok` is false when the schema text does not lower — half a schema is refused. */
export type SchemaView = {
  ok: boolean
  predicates: PredicateView[]
  diagnostics: DiagnosticView[]
}

export type LoweredNode = {
  id: number
  /** The construct: `Var`, `Record`, `Access`, `Fact`, `Select`, `Arith`… */
  kind: string
  /** A variable's name, a literal's value, the field read, the predicate matched. */
  label: string | null
  /** The type inference reached for it, in schema notation. */
  ty: string | null
  span: Span
  children: number[]
}

export type StatementView = { kind: string; op: string | null; nodes: number[] }

export type Lowered = {
  schema_ok: boolean
  head: number | null
  head_ty: string | null
  statements: StatementView[]
  nodes: LoweredNode[]
  diagnostics: DiagnosticView[]
}

export type Sample = { label: string; source: string }

export type Engine = {
  version: string
  /** Bytes of the WebAssembly module, as delivered. */
  bytes: number
  lex: (source: string) => Tokens
  parse: (source: string) => Tree
  /** Read a schema, which everything after parsing resolves names against. */
  schema: (source: string) => SchemaView
  /** The whole front end: lex, parse, lower, typecheck, flatten, reorder. */
  compile: (schema: string, query: string) => Lowered
  /** What the site opens with — both tested in the Rust suite, not invented here. */
  sampleSchema: string
  samples: Sample[]
}

let engine: Promise<Engine> | null = null

/** The engine, loaded once and shared. */
export function load(): Promise<Engine> {
  engine ??= (async () => {
    await init({ module_or_path: wasmUrl })
    const response = await fetch(wasmUrl)
    const bytes = (await response.arrayBuffer()).byteLength
    return {
      version: version(),
      bytes,
      lex: (source: string) => JSON.parse(tokens(source)) as Tokens,
      parse: (source: string) => JSON.parse(tree(source)) as Tree,
      schema: (source: string) => JSON.parse(schema(source)) as SchemaView,
      compile: (schemaSource: string, query: string) =>
        JSON.parse(compile(schemaSource, query)) as Lowered,
      sampleSchema: sample_schema(),
      samples: JSON.parse(samples()) as Sample[],
    }
  })()
  return engine
}
