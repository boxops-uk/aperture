import { useCallback } from 'react'
import { CodeBlock } from '@astryxdesign/core/CodeBlock'
import { useEngine } from '../engine'
import { paints, tokenize, type SyntaxToken } from './highlight'
import type { TokenClass } from '../wasm'

/**
 * A fenced block from the book, **painted by the engine when there is one**.
 *
 * `sigla` and `schema` are the two languages this repository owns, and a regular
 * expression that agrees with the lexer today is a regular expression that
 * disagrees with it after the next keyword. So when a demo elsewhere on the page
 * has brought the module in, these blocks are tokenized by the same lexer the
 * compiler runs — token for token, including the ones it rejects. Until then,
 * and for Rust and the plan printer, the fallback rules stand in.
 */

/** What the lexer calls a token, as a syntax token type. */
const AS: Record<TokenClass, string> = {
  keyword: 'keyword',
  predicate: 'function',
  namespace: 'type',
  // `constant`, not `variable`: the design system paints *unhighlighted* code
  // with `variable`, so that slot is the plain ink and a sigla variable takes
  // the next one along. The colours are the theme's; only the slots are here.
  variable: 'constant',
  field: 'property',
  number: 'number',
  string: 'string',
  wildcard: 'punctuation',
  comment: 'comment',
  punctuation: 'punctuation',
  whitespace: '',
  error: 'tag',
}

export function Code({ lang, source }: { lang: string; source: string }) {
  // Observing, not demanding: a page of prose does not fetch a compiler.
  const { engine } = useEngine()
  const ours = lang === 'sigla' || lang === 'schema'

  const tokenizer = useCallback(
    (code: string, language: string): SyntaxToken[] => {
      if (engine && (language === 'sigla' || language === 'schema')) {
        // The lexer measures in bytes and this string is indexed in UTF-16
        // units, so a block with a character outside ASCII would be painted one
        // token to the left of itself. Those fall back to the rules instead.
        if (!/[^\x20-\x7e\n\t]/.test(code)) {
          const { tokens } = language === 'schema' ? engine.lexSchema(code) : engine.lex(code)
          return tokens
            .filter((token) => AS[token.class])
            .map((token) => ({
              type: AS[token.class],
              start: token.span.start,
              end: token.span.end,
            }))
        }
      }
      return tokenize(code, language)
    },
    [engine],
  )

  const custom = ours || paints(lang)

  return (
    <CodeBlock
      code={source}
      language={lang === 'text' ? 'plaintext' : lang}
      tokenizer={custom ? tokenizer : undefined}
      // Which painter this block got. The highlights themselves are CSS ranges
      // rather than elements, so this is the only place the difference between
      // the engine's lexer and the fallback rules is visible from outside.
      data-painted={ours && engine ? 'engine' : 'rules'}
      width="100%"
      size="sm"
      maxHeight={520}
    />
  )
}
