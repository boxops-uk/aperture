import type { DiagnosticView } from './wasm'
import { display } from './display'

/**
 * What a phase reported, in the order a reader meets it.
 *
 * The order is the engine's — `Diagnostics::in_source_order`, the same function
 * the terminal renders through — so this page and `fjord query` cannot disagree
 * about which fault comes first.
 */
export function Diagnostics({
  diagnostics,
  source,
}: {
  diagnostics: DiagnosticView[]
  source: string
}) {
  if (diagnostics.length === 0) return null

  return (
    <ul className="diagnostics">
      {diagnostics.map((diagnostic, index) => {
        const at = diagnostic.labels.find((label) => label.primary)?.span
        return (
          <li key={index}>
            {diagnostic.code && <code className="code">{diagnostic.code}</code>}
            <span>{diagnostic.message}</span>
            {at && (
              <span className="at">
                at {at.start}–{at.end}
                {at.end > at.start && (
                  <>
                    : <code>{display(source.slice(at.start, at.end))}</code>
                  </>
                )}
              </span>
            )}
          </li>
        )
      })}
    </ul>
  )
}
