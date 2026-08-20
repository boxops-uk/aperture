import type { TokenView } from './wasm'
import { type Highlight, within } from './span'
import { display } from './display'

export function TokenTable({
  tokens,
  highlight,
  onHighlight,
}: {
  tokens: TokenView[]
  highlight: Highlight | null
  onHighlight: (highlight: Highlight | null) => void
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
              className={within(token.span, highlight) ? 'on' : undefined}
              onMouseEnter={() => onHighlight({ span: token.span, node: null, view: null })}
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
