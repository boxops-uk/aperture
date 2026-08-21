import { useState } from 'react'
import { useEngine } from '../engine'
import { paint } from './highlight'

/**
 * A fenced block from the book, **painted by the engine when there is one**.
 *
 * `sigla` and `schema` blocks are the two languages this repository owns, and a
 * regular expression that agrees with the lexer today is a regular expression
 * that disagrees with it after the next keyword. So when a demo elsewhere on
 * the page has brought the module in, these blocks are re-painted by the same
 * lexer the compiler runs — token for token, including the ones it rejects.
 * Until then, the fallback rules paint them, and they are only ever colours.
 */
export function Code({ lang, source }: { lang: string; source: string }) {
  // Observing, not demanding: a page of prose does not fetch a compiler.
  const { engine } = useEngine()
  const [copied, setCopied] = useState(false)

  const copy = () => {
    navigator.clipboard?.writeText(source).then(
      () => {
        setCopied(true)
        setTimeout(() => setCopied(false), 1400)
      },
      () => {
        // A browser that refuses the clipboard is not a failure worth saying.
      },
    )
  }

  const lexed = engine && (lang === 'sigla' || lang === 'schema')

  return (
    <figure className="code">
      <figcaption>
        <span className="lang">{lang}</span>
        <button
          type="button"
          className={copied ? 'copy done' : 'copy'}
          onClick={copy}
          aria-label="Copy code"
        >
          {copied ? 'copied' : 'copy'}
        </button>
      </figcaption>
      <pre>
        {lexed ? (
          <code className={`lang-${lang}`}>
            <Lexed source={source} schema={lang === 'schema'} />
          </code>
        ) : (
          <code
            className={`lang-${lang}`}
            dangerouslySetInnerHTML={{ __html: paint(source, lang) }}
          />
        )}
      </pre>
    </figure>
  )
}

/** The same painting the editors use: one span per token, from the lexer. */
function Lexed({ source, schema }: { source: string; schema: boolean }) {
  const { engine } = useEngine()
  if (!engine) return <>{source}</>
  const { tokens } = schema ? engine.lexSchema(source) : engine.lex(source)
  return (
    <>
      {tokens.map((token, index) => (
        <span key={index} className={`tok tok-${token.class}`}>
          {token.text}
        </span>
      ))}
    </>
  )
}
