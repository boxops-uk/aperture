import { useRef } from 'react'
import type { Span, TokenView } from './wasm'
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
 *
 * The wavy underline is drawn from the **diagnostics**, not from the token
 * classes. A byte the lexer refused is one kind of fault and gets a class of its
 * own; an unknown predicate, a type mismatch and a range restriction are the
 * others, and they are perfectly good tokens — so a squiggle that came from the
 * class only ever appeared under a bad string.
 */
export function Editor({
  source,
  tokens,
  highlight,
  onChange,
  onHighlight,
  rows,
  flaws = [],
}: {
  source: string
  tokens: TokenView[]
  highlight: Highlight | null
  onChange: (next: string) => void
  onHighlight?: (highlight: Highlight | null) => void
  /** How tall to start. The schema is long; a query is not. */
  rows?: 'query' | 'schema'
  /** The spans every phase reported a fault at, underlined where they are. */
  flaws?: Span[]
}) {
  const painted = useRef<HTMLPreElement>(null)
  const faulty = (span: Span) =>
    flaws.some((flaw) => span.start < Math.max(flaw.end, flaw.start + 1) && flaw.start < span.end)

  return (
    <div className={rows === 'schema' ? 'editor tall' : 'editor'}>
      <pre className="paint" ref={painted} aria-hidden="true">
        {tokens.map((token, index) => (
          <span
            key={index}
            className={[
              `tok tok-${token.class}`,
              within(token.span, highlight) ? 'on' : '',
              faulty(token.span) ? 'faulty' : '',
            ]
              .filter(Boolean)
              .join(' ')}
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
