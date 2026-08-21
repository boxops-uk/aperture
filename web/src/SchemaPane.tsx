import type { SchemaView, Tokens } from './wasm'
import { Diagnostics } from './Diagnostics'
import { Editor } from './Editor'

/**
 * The schema, as text, because that is the only form a browser can hold one in.
 *
 * It lives in a drawer rather than a column: a reader edits it rarely and reads
 * it often, and the width it would take is the width the database table needs.
 * Editing it recompiles the query, which is the point — change `code.Decl`'s
 * fields and watch the query that read them stop typechecking.
 */
export function SchemaPane({
  source,
  view,
  tokens,
  onChange,
}: {
  source: string
  view: SchemaView | null
  /** The schema language's own tokens — a second lexer, not a second reading. */
  tokens: Tokens | null
  onChange: (next: string) => void
}) {
  return (
    <section className="schema">
      <Editor
        source={source}
        tokens={tokens?.tokens ?? []}
        highlight={null}
        onChange={onChange}
        rows="schema"
      />
      {view && <Diagnostics diagnostics={view.diagnostics} source={source} />}
    </section>
  )
}
