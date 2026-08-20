// Loading the engine.
//
// The module is built by `scripts/build-wasm.sh` into `src/wasm/`, which is
// **not** checked in: a binary in git is a binary somebody has to trust, and
// the build is one command. A checkout without it fails loudly here rather than
// silently falling back to a JavaScript imitation of the lexer — the imitation
// is the thing this replaces.
import init, { tokens, tree, version } from './wasm/fjord_wasm.js'
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

export type Engine = {
  version: string
  /** Bytes of the WebAssembly module, as delivered. */
  bytes: number
  lex: (source: string) => Tokens
  parse: (source: string) => Tree
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
    }
  })()
  return engine
}
