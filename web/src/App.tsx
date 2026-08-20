import { useEffect, useState } from 'react'
import { load, type Engine, type Span, type Tokens, type Tree } from './wasm'
import { Editor } from './Editor'
import { TokenTable } from './TokenTable'
import { TreeView } from './TreeView'
import { Diagnostics } from './Diagnostics'
import './app.css'

/**
 * Sigla worth typing at, taken from the engine's own corpus — where each one is
 * already classified and its answer already asserted. A demo carrying examples
 * of its own would be a second statement of the language that nothing checks,
 * and the first version of this page proved the point: every sample it shipped
 * was missing its head, which the lexer was perfectly happy to tokenise.
 */
const SAMPLES: { label: string; source: string }[] = [
  { label: 'a join', source: 'X where test.Edge {from = X, to = Y}; test.Node {id = Y}' },
  { label: 'a record head', source: '{a = X, b = Y} where test.Foo {name = X, id = Y}' },
  { label: 'a constraint', source: 'X where test.Name X; X = "a"..' },
  { label: 'a denial', source: 'X where test.Name X; X != "abc"' },
  { label: 'a negation', source: 'X where test.Foo {id = X}; !test.Bar {id = X}' },
  { label: 'a subquery', source: 'X where X = (Y where test.Foo {id = Y})' },
  { label: 'junk', source: 'X where X = }' },
]

type Analysis = { tokens: Tokens; tree: Tree; micros: number }

/**
 * Lex, parse, and time the whole round trip — both calls, the JSON, and parsing
 * it back — because that is what a keystroke actually costs a page. The engine's
 * own share is far smaller, and printing that number would be measuring
 * something the reader cannot see.
 *
 * Outside the component because it reads a clock: a measurement taken during
 * render is one that changes when React re-renders for reasons of its own.
 */
function analyse(engine: Engine, source: string): Analysis {
  const started = performance.now()
  const tokens = engine.lex(source)
  const tree = engine.parse(source)
  return { tokens, tree, micros: (performance.now() - started) * 1000 }
}

export default function App() {
  const [engine, setEngine] = useState<Engine | null>(null)
  const [failure, setFailure] = useState<string | null>(null)
  const [highlight, setHighlight] = useState<Span | null>(null)
  const [tab, setTab] = useState<'tokens' | 'tree'>('tokens')

  // Source and analysis move together, updated by whatever changed the source —
  // a keystroke, a sample, or the engine finishing loading.
  const [state, setState] = useState<{ source: string; view: Analysis | null }>({
    source: SAMPLES[0].source,
    view: null,
  })

  const show = (source: string, engine: Engine | null) =>
    setState({ source, view: engine ? analyse(engine, source) : null })

  useEffect(() => {
    load().then(
      (loaded) => {
        setEngine(loaded)
        setState((current) => ({ ...current, view: analyse(loaded, current.source) }))
      },
      (error: unknown) => setFailure(String(error)),
    )
  }, [])

  const { source, view } = state
  // The parse reports what the lexer already said about a bad byte, so showing
  // both would print every fault twice.
  const diagnostics = view ? view.tree.diagnostics : []

  return (
    <div className="page">
      <header className="top">
        <div className="brand">
          <span className="mark" aria-hidden="true" />
          <div>
            <h1>sigla, in your browser</h1>
            <p>
              The tokens and the tree below come from <code>fjord_engine</code> compiled
              to WebAssembly — the same lexer and the same generated parser the server
              and the shell run, not a re-implementation of either.
            </p>
          </div>
        </div>
        <Status engine={engine} failure={failure} micros={view?.micros} />
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
            tokens={view?.tokens.tokens ?? []}
            highlight={highlight}
            onChange={(next) => show(next, engine)}
            onHighlight={setHighlight}
          />
          <Diagnostics diagnostics={diagnostics} source={source} />
        </section>

        <section className="pane">
          <div className="tabs">
            <button
              type="button"
              className={tab === 'tokens' ? 'tab on' : 'tab'}
              onClick={() => setTab('tokens')}
            >
              tokens<span className="count">{view?.tokens.tokens.length ?? 0}</span>
            </button>
            <button
              type="button"
              className={tab === 'tree' ? 'tab on' : 'tab'}
              onClick={() => setTab('tree')}
            >
              parse tree<span className="count">{view?.tree.nodes.length ?? 0}</span>
            </button>
          </div>

          {tab === 'tokens' ? (
            <TokenTable
              tokens={view?.tokens.tokens ?? []}
              highlight={highlight}
              onHighlight={setHighlight}
            />
          ) : (
            <TreeView
              tree={view?.tree ?? { root: null, nodes: [], diagnostics: [] }}
              highlight={highlight}
              onHighlight={setHighlight}
            />
          )}
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
        <dt>round trip</dt>
        <dd>{micros === undefined ? '—' : `${micros.toFixed(0)} µs`}</dd>
      </div>
    </dl>
  )
}
