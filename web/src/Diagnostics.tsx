import { Banner } from '@astryxdesign/core/Banner'
import type { DiagnosticView } from './wasm'
import { display } from './display'

/**
 * What a phase reported, in the order a reader meets it.
 *
 * The order is the engine's — `Diagnostics::in_source_order`, the same function
 * the terminal renders through — so this page and `fjord query` cannot disagree
 * about which fault comes first.
 *
 * A `Banner`, because that is what the rest of the site says "something is
 * wrong" with. One line of it: the span is part of the sentence rather than
 * something to unfold, since a disclosure whose whole content is "at 8–30"
 * costs a click to say six characters.
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
        const where = at
          ? at.end > at.start
            ? ` — at ${at.start}–${at.end}: ${display(source.slice(at.start, at.end))}`
            : ` — at ${at.start}`
          : ''
        return (
          <li key={index}>
            <Banner
              status="error"
              // The code is the taxonomy and the thing a test asserts on, so it
              // is the title; the sentence is what a reader does about it.
              title={diagnostic.code ?? 'refused'}
              description={diagnostic.message + where}
            />
          </li>
        )
      })}
    </ul>
  )
}
