// Loading the engine.
//
// The module is built by `scripts/build-wasm.sh` into `src/wasm/`, which is
// **not** checked in: a binary in git is a binary somebody has to trust, and
// the build is one command. A checkout without it fails loudly here rather than
// silently falling back to a JavaScript imitation of the lexer — the imitation
// is the thing this replaces.
import init, {
  compile,
  database,
  run,
  sample_schema,
  samples,
  schema,
  schema_tokens,
  tokens,
  trace,
  tree,
  version,
} from './wasm/fjord_wasm.js'
import wasmUrl from './wasm/fjord_wasm_bg.wasm?url'

export type Span = { start: number; end: number }

export type TokenClass =
  | 'keyword'
  | 'predicate'
  | 'namespace'
  | 'variable'
  | 'field'
  | 'number'
  | 'string'
  | 'wildcard'
  | 'comment'
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

export type StepView = {
  index: number
  /** `Level`, `Derive` or `Test` — what the machine does with it. */
  kind: string
  /** The register it fills; absent for a test, which binds nothing. */
  register: string | null
  /** Its number among *loop levels*, which is what a resume cursor pairs against. */
  level: number | null
  /** `scan`, `seek`, `fetch`, `absent`, `derive`, `compare` — one per source. */
  access: string[]
  predicates: string[]
  /** Rows this step read and then dropped. */
  residuals: number
  /** The step as the engine prints it — the same text `fjord query --plan` shows. */
  text: string
}

export type PlanView = {
  /** The identity a resume cursor carries, in hex. */
  fingerprint: string
  levels: number
  steps_count: number
  registers: number
  steps: StepView[]
  head: string
}

export type Lowered = {
  schema_ok: boolean
  head: number | null
  head_ty: string | null
  statements: StatementView[]
  nodes: LoweredNode[]
  /** What the query compiles to — absent whenever anything was reported. */
  plan: PlanView | null
  diagnostics: DiagnosticView[]
}

export type Sample = { label: string; source: string; rows: number | null }

export type RowView = { at: number; value: unknown }

export type Rows = {
  rows: RowView[]
  /** Rows pulled per plan step — matched or skipped. */
  examined: number[]
  examined_total: number
  truncated: boolean
  diagnostics: DiagnosticView[]
}

export type RegisterView = {
  address: number
  /** `fact`, `value`, or `empty`. */
  kind: string
  /** The stored key this row is, in hex — what the database table matches on. */
  key: string | null
  /** `code.Decl#4`, for a register holding a row. */
  fact: string | null
  value: unknown
}

export type Rejection = { step: number; residual: number; row: RegisterView }

/** The range a level opened over — bytes, because that is what a bound is. */
export type Scanning = {
  step: number
  lo: string
  hi: string | null
  /** The fact a point read named, for a level that fetches rather than scans. */
  fetch: string | null
}

export type RowBytes = {
  fact: string
  /** The whole stored key, with the predicate prefix, in hex. */
  key: string
  decoded: unknown
  value: string | null
  value_decoded: unknown
}

export type PredicateRows = { id: number; name: string; ty: string; rows: RowBytes[] }

export type Database = { predicates: PredicateRows[]; facts: number }

export type TraceStep = {
  at: number
  /** `step`, `yield`, `reject`, or `done`. */
  event: string
  depth: number
  /** Only the registers this transition changed — the page folds them. */
  registers: RegisterView[]
  row: unknown
  rejected: Rejection | null
  scanning: Scanning | null
  examined: number[]
}

export type Trace = {
  steps: TraceStep[]
  rows: number
  examined_total: number
  /** Whether the run stopped at the cap rather than because it was done. */
  truncated: boolean
  diagnostics: DiagnosticView[]
}

export type Engine = {
  version: string
  /** Bytes of the WebAssembly module, as delivered. */
  bytes: number
  lex: (source: string) => Tokens
  parse: (source: string) => Tree
  /** Read a schema, which everything after parsing resolves names against. */
  schema: (source: string) => SchemaView
  /** Lex a schema — a second language, with its own lexer. */
  lexSchema: (source: string) => Tokens
  /** The whole front end: lex, parse, lower, typecheck, flatten, reorder. */
  compile: (schema: string, query: string) => Lowered
  /** Run the query against the demo database. */
  run: (schema: string, query: string) => Rows
  /** The whole run, one transition at a time. */
  trace: (schema: string, query: string) => Trace
  /** Every stored row, as bytes and as a fact, in the order a scan meets them. */
  database: (schema: string) => Database
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
      lexSchema: (source: string) => JSON.parse(schema_tokens(source)) as Tokens,
      compile: (schemaSource: string, query: string) =>
        JSON.parse(compile(schemaSource, query)) as Lowered,
      run: (schemaSource: string, query: string) =>
        JSON.parse(run(schemaSource, query)) as Rows,
      trace: (schemaSource: string, query: string) =>
        JSON.parse(trace(schemaSource, query)) as Trace,
      database: (schemaSource: string) => JSON.parse(database(schemaSource)) as Database,
      sampleSchema: sample_schema(),
      samples: JSON.parse(samples()) as Sample[],
    }
  })()
  return engine
}
