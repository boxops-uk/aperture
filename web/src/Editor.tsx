import { useRef } from 'react'
import type { TokenView } from './wasm'
import { type Highlight, within } from './span'

/**
 * A textarea with the real tokens painted underneath it.
 *
 * The overlay is what makes this the lexer's output rather than a picture of
 * it: the caret, the selection and the wrapping all belong to the textarea, and
 * every coloured span behind it is one token's `span` sliced out of the source.
 * The two only stay aligned because the token stream covers the source
 * exactly — which is what `token_spans_reproduce_the_source_exactly` asserts,
 * for both languages.
 *
 * Used for the query and for the schema, because the difference between them is
 * *which lexer produced the tokens* and nothing else a page can see.
 */
export function Editor({
  source,
  tokens,
  highlight,
  onChange,
  onHighlight,
  rows,
}: {
  source: string
  tokens: TokenView[]
  highlight: Highlight | null
  onChange: (next: string) => void
  onHighlight?: (highlight: Highlight | null) => void
  /** How tall to start. The schema is long; a query is not. */
  rows?: 'query' | 'schema'
}) {
  const painted = useRef<HTMLPreElement>(null)

  return (
    <div className={rows === 'schema' ? 'editor tall' : 'editor'}>
      <pre className="paint" ref={painted} aria-hidden="true">
        {tokens.map((token, index) => (
          <span
            key={index}
            className={
              within(token.span, highlight) ? `tok tok-${token.class} on` : `tok tok-${token.class}`
            }
            onMouseEnter={() => onHighlight?.({ span: token.span, node: null, view: null })}
            onMouseLeave={() => onHighlight?.(null)}
          >
            {token.text}
          </span>
        ))}
        {'\n'}
      </pre>
      <textarea
        className="input"
        spellCheck={false}
        value={source}
        onChange={(event) => onChange(event.target.value)}
        onScroll={(event) => {
          if (painted.current) {
            painted.current.scrollTop = event.currentTarget.scrollTop
            painted.current.scrollLeft = event.currentTarget.scrollLeft
          }
        }}
      />
    </div>
  )
}
