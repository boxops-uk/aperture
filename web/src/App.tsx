import { useEffect, useRef, useState } from 'react'
import { load, type Engine, type TokenView, type Tokens } from './wasm'

/** A lexed source, with what the round trip cost. */
type Lexed = Tokens & { micros: number }

/**
 * Lex, and time the whole round trip — the lex, the JSON, and parsing it back —
 * because that is what a keystroke actually costs a page. The lexer's own share
 * is far smaller, and printing that number would be measuring something the
 * reader cannot see.
 *
 * Outside the component because it reads a clock: a measurement taken during
 * render is one that changes when React re-renders for reasons of its own.
 */
function measure(engine: Engine, source: string): Lexed {
  const started = performance.now()
  const view = engine.lex(source)
  return { ...view, micros: (performance.now() - started) * 1000 }
}
import './app.css'

/** Sigla worth typing at, taken from the design book's own examples. */
const SAMPLES: { label: string; source: string }[] = [
  { label: 'a join', source: 'where {\n  src.File { name = Path },\n  src.Decl { file = F, name = Name }\n}' },
  { label: 'a constraint', source: 'where src.File { name = Path = "src/" .. }' },
  { label: 'a denial', source: 'where {\n  src.Decl { name = Name },\n  Name != "main"\n}' },
  { label: 'literals', source: 'where {\n  test.Count = 42,\n  test.Name = "a string with \\"escapes\\""\n}' },
  { label: 'a bad byte', source: 'where src.File { name = X } @' },
]

export default function App() {
  const [engine, setEngine] = useState<Engine | null>(null)
  const [failure, setFailure] = useState<string | null>(null)
  const [hovered, setHovered] = useState<number | null>(null)

  // Source and tokens move together, updated by whatever changed the source —
  // a keystroke, or the engine finishing loading. Not derived during render,
  // because lexing is what is being *measured*.
  const [state, setState] = useState<{ source: string; view: Lexed | null }>({
    source: SAMPLES[0].source,
    view: null,
  })

  const show = (source: string, engine: Engine | null) =>
    setState({ source, view: engine ? measure(engine, source) : null })

  useEffect(() => {
    load().then(
      (loaded) => {
        setEngine(loaded)
        setState((current) => ({ ...current, view: measure(loaded, current.source) }))
      },
      (error: unknown) => setFailure(String(error)),
    )
  }, [])

  const { source, view: lexed } = state

  return (
    <div className="page">
      <header className="top">
        <div className="brand">
          <span className="mark" aria-hidden="true" />
          <div>
            <h1>sigla, lexed in your browser</h1>
            <p>
              The tokens below come from <code>fjord_engine::lexer</code> compiled to
              WebAssembly — the same code the server and the shell run, not a
              re-implementation of it.
            </p>
          </div>
        </div>
        <Status engine={engine} failure={failure} micros={lexed?.micros} />
      </header>

      <div className="samples">
        {SAMPLES.map((sample) => (
          <button
            key={sample.label}
            type="button"
            className={source === sample.source ? 'chip on' : 'chip'}
            onClick={() => show(sample.source, engine)}
          >
            {sample.label}
          </button>
        ))}
      </div>

      <main className="panes">
        <section className="pane">
          <h2>source</h2>
          <Editor
            source={source}
            tokens={lexed?.tokens ?? []}
            hovered={hovered}
            onChange={(next) => show(next, engine)}
            onHover={setHovered}
          />
          <Diagnostics diagnostics={lexed?.diagnostics ?? []} source={source} />
        </section>

        <section className="pane">
          <h2>
            tokens<span className="count">{lexed ? lexed.tokens.length : 0}</span>
          </h2>
          <TokenTable
            tokens={lexed?.tokens ?? []}
            hovered={hovered}
            onHover={setHovered}
          />
        </section>
      </main>
    </div>
  )
}

function Status({
  engine,
  failure,
  micros,
}: {
  engine: Engine | null
  failure: string | null
  micros?: number
}) {
  if (failure) {
    return (
      <p className="status bad">
        the engine did not load — run <code>scripts/build-wasm.sh</code>
        <br />
        <span className="detail">{failure}</span>
      </p>
    )
  }
  if (!engine) return <p className="status">loading the engine…</p>
  return (
    <dl className="status">
      <div>
        <dt>fjord</dt>
        <dd>{engine.version}</dd>
      </div>
      <div>
        <dt>module</dt>
        <dd>{(engine.bytes / 1024).toFixed(1)} KiB</dd>
      </div>
      <div>
        <dt>lex + json</dt>
        <dd>{micros === undefined ? '—' : `${micros.toFixed(0)} µs`}</dd>
      </div>
    </dl>
  )
}

/**
 * A textarea with the real tokens painted underneath it.
 *
 * The overlay is what makes this the lexer's output rather than a picture of
 * it: the caret, the selection and the wrapping all belong to the textarea, and
 * every coloured span behind it is one token's `span` sliced out of the source.
 * The two only stay aligned because the token stream covers the source
 * exactly — which is what `token_spans_reproduce_the_source_exactly` asserts.
 */
function Editor({
  source,
  tokens,
  hovered,
  onChange,
  onHover,
}: {
  source: string
  tokens: TokenView[]
  hovered: number | null
  onChange: (next: string) => void
  onHover: (index: number | null) => void
}) {
  const painted = useRef<HTMLPreElement>(null)

  return (
    <div className="editor">
      <pre className="paint" ref={painted} aria-hidden="true">
        {tokens.map((token, index) => (
          <span
            key={index}
            className={
              index === hovered ? `tok tok-${token.class} on` : `tok tok-${token.class}`
            }
            onMouseEnter={() => onHover(index)}
            onMouseLeave={() => onHover(null)}
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

function TokenTable({
  tokens,
  hovered,
  onHover,
}: {
  tokens: TokenView[]
  hovered: number | null
  onHover: (index: number | null) => void
}) {
  return (
    <div className="tokens">
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
              className={index === hovered ? 'on' : undefined}
              onMouseEnter={() => onHover(index)}
              onMouseLeave={() => onHover(null)}
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

function Diagnostics({
  diagnostics,
  source,
}: {
  diagnostics: { code: string | null; message: string; labels: { span: { start: number; end: number }; primary: boolean }[] }[]
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
                at {at.start}–{at.end}: <code>{display(source.slice(at.start, at.end))}</code>
              </span>
            )}
          </li>
        )
      })}
    </ul>
  )
}

/** Whitespace has to be visible in a table, or a row reads as empty. */
function display(text: string): string {
  return text.replace(/\n/g, '⏎').replace(/\t/g, '⇥').replace(/ /g, '·')
}
