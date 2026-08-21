import { Banner } from '@astryxdesign/core/Banner'
import { Code } from '@astryxdesign/core/Code'
import { Text } from '@astryxdesign/core/Text'
import { HStack, VStack } from '@astryxdesign/core/Stack'
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
 * wrong" with: a callout in the book and a refused query in the workbench are
 * the same kind of statement, and were two different-looking boxes.
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
            <Banner
              status="error"
              // The code is the taxonomy and the thing a test asserts on, so it
              // is the title; the sentence is what a reader does about it.
              title={diagnostic.code ?? 'refused'}
              description={diagnostic.message}
            >
              {at && (
                <VStack gap={1}>
                  <HStack gap={2} align="center" wrap="wrap">
                    <Text type="supporting">
                      at {at.start}–{at.end}
                    </Text>
                    {at.end > at.start && <Code>{display(source.slice(at.start, at.end))}</Code>}
                  </HStack>
                </VStack>
              )}
            </Banner>
          </li>
        )
      })}
    </ul>
  )
}
