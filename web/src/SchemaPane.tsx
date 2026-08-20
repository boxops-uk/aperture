import { useState } from 'react'
import type { SchemaView, Tokens } from './wasm'
import { Diagnostics } from './Diagnostics'
import { Editor } from './Editor'

/**
 * The schema, as text, because that is the only form a browser can hold one in.
 *
 * Collapsed by default: it is the *context* for the query rather than the thing
 * a reader is editing, and the sample is 150 lines. Editing it recompiles the
 * query, which is the point — change `src.Decl`'s fields and watch the query
 * that read them stop typechecking.
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
  const [open, setOpen] = useState(false)

  return (
    <section className="schema">
      <button type="button" className="disclosure" onClick={() => setOpen(!open)}>
        <span className={open ? 'arrow open' : 'arrow'} aria-hidden="true">
          ▸
        </span>
        <span className="what">schema</span>
        {view && (
          <span className={view.ok ? 'summary' : 'summary bad'}>
            {view.ok
              ? `${view.predicates.length} predicates`
              : `${view.diagnostics.length} problem${view.diagnostics.length === 1 ? '' : 's'}`}
          </span>
        )}
      </button>

      {open && (
        <>
          <Editor
            source={source}
            tokens={tokens?.tokens ?? []}
            highlight={null}
            onChange={onChange}
            rows="schema"
          />
          {view && <Diagnostics diagnostics={view.diagnostics} source={source} />}
          {view?.ok && (
            <ul className="predicates">
              {view.predicates.map((predicate) => (
                <li key={predicate.id}>
                  <code className="name">{predicate.name}</code>
                  <code className="ty">{predicate.ty}</code>
                </li>
              ))}
            </ul>
          )}
        </>
      )}
    </section>
  )
}
