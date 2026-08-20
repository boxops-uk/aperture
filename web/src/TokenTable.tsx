import type { Span, TokenView } from './wasm'
import { overlaps } from './span'
import { display } from './display'

export function TokenTable({
  tokens,
  highlight,
  onHighlight,
}: {
  tokens: TokenView[]
  highlight: Span | null
  onHighlight: (span: Span | null) => void
}) {
  return (
    <div className="scroller">
      <table>
        <thead>
          <tr>
            <th className="num">span</th>
            <th>kind</th>
            <th>class</th>
            <th>text</th>
          </tr>
        </thead>
        <tbody>
          {tokens.map((token, index) => (
            <tr
              key={index}
              className={overlaps(token.span, highlight) ? 'on' : undefined}
              onMouseEnter={() => onHighlight(token.span)}
              onMouseLeave={() => onHighlight(null)}
            >
              <td className="num">
                {token.span.start}–{token.span.end}
              </td>
              <td className="kind">{token.kind}</td>
              <td>
                <span className={`pill tok-${token.class}`}>{token.class}</span>
              </td>
              <td className="text">{display(token.text)}</td>
            </tr>
          ))}
        </tbody>
      </table>
      {tokens.length === 0 && <p className="empty">nothing to lex yet</p>}
    </div>
  )
}
