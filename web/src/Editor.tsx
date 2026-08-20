import { useRef } from 'react'
import type { Span, TokenView } from './wasm'
import { overlaps } from './span'

/**
 * A textarea with the real tokens painted underneath it.
 *
 * The overlay is what makes this the lexer's output rather than a picture of
 * it: the caret, the selection and the wrapping all belong to the textarea, and
 * every coloured span behind it is one token's `span` sliced out of the source.
 * The two only stay aligned because the token stream covers the source
 * exactly — which is what `token_spans_reproduce_the_source_exactly` asserts.
 */
export function Editor({
  source,
  tokens,
  highlight,
  onChange,
  onHighlight,
}: {
  source: string
  tokens: TokenView[]
  highlight: Span | null
  onChange: (next: string) => void
  onHighlight: (span: Span | null) => void
}) {
  const painted = useRef<HTMLPreElement>(null)

  return (
    <div className="editor">
      <pre className="paint" ref={painted} aria-hidden="true">
        {tokens.map((token, index) => (
          <span
            key={index}
            className={
              overlaps(token.span, highlight) ? `tok tok-${token.class} on` : `tok tok-${token.class}`
            }
            onMouseEnter={() => onHighlight(token.span)}
            onMouseLeave={() => onHighlight(null)}
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
