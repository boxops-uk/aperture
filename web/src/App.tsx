import { useEffect, useState } from 'react'
import { load, type Engine, type Lowered, type SchemaView, type Tokens, type Tree } from './wasm'
import type { Highlight } from './span'
import { Editor } from './Editor'
import { TokenTable } from './TokenTable'
import { TreeView } from './TreeView'
import { LoweredView } from './LoweredView'
import { SchemaPane } from './SchemaPane'
import { Diagnostics } from './Diagnostics'
import './app.css'

type Analysis = {
  tokens: Tokens
  tree: Tree
  lowered: Lowered
  schema: SchemaView
  micros: number
}

/**
 * Everything the front end says about a query, and what the whole round trip
 * cost — every call, the JSON, and parsing it back — because that is what a
 * keystroke actually costs a page. The engine's own share is far smaller, and
 * printing that number would be measuring something the reader cannot see.
 *
 * The schema is recompiled with the query rather than cached, and it is cheap
 * enough not to matter: the module holds no state, which is what keeps the
 * boundary a pair of strings instead of a handle nobody can free.
 *
 * Outside the component because it reads a clock: a measurement taken during
 * render is one that changes when React re-renders for reasons of its own.
 */
function analyse(engine: Engine, schemaSource: string, source: string): Analysis {
  const started = performance.now()
  const tokens = engine.lex(source)
  const tree = engine.parse(source)
  const schema = engine.schema(schemaSource)
  const lowered = engine.compile(schemaSource, source)
  return { tokens, tree, schema, lowered, micros: (performance.now() - started) * 1000 }
}

export default function App() {
  const [engine, setEngine] = useState<Engine | null>(null)
  const [failure, setFailure] = useState<string | null>(null)
  const [highlight, setHighlight] = useState<Highlight | null>(null)
  const [tab, setTab] = useState<'tokens' | 'tree' | 'lowered'>('lowered')

  // Schema, query and analysis move together, updated by whatever changed
  // either text — a keystroke, a sample, or the engine finishing loading.
  const [state, setState] = useState<{ schema: string; source: string; view: Analysis | null }>({
    schema: '',
    source: '',
    view: null,
  })

  const show = (schema: string, source: string, engine: Engine | null) =>
    setState({ schema, source, view: engine ? analyse(engine, schema, source) : null })

  useEffect(() => {
    load().then(
      (loaded) => {
        setEngine(loaded)
        const schema = loaded.sampleSchema
        const source = loaded.samples[0]?.source ?? ''
        setState({ schema, source, view: analyse(loaded, schema, source) })
      },
      (error: unknown) => setFailure(String(error)),
    )
  }, [])

  const { schema, source, view } = state
  // One list, from the compilation: it carries the lexer's and the parser's
  // faults as well as its own, in the order `Diagnostics::in_source_order` puts
  // them — so showing the parse's separately would print every one twice.
  const diagnostics = view ? view.lowered.diagnostics : []

  return (
    <div className="page">
      <header className="top">
        <div className="brand">
          <span className="mark" aria-hidden="true" />
          <div>
            <h1>sigla, in your browser</h1>
            <p>
              Everything below comes from <code>fjord_engine</code> compiled to
              WebAssembly — the same lexer, parser, and typechecker the server and the
              shell run. Edit the schema and the query stops typechecking, because it is
              the same schema the query is resolved against.
            </p>
          </div>
        </div>
        <Status engine={engine} failure={failure} micros={view?.micros} />
      </header>

      <div className="samples">
        {(engine?.samples ?? []).map((sample) => (
          <button
            key={sample.label}
            type="button"
            className={source === sample.source ? 'chip on' : 'chip'}
            onClick={() => show(schema, sample.source, engine)}
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
            onChange={(next) => show(schema, next, engine)}
            onHighlight={setHighlight}
          />
          <Diagnostics diagnostics={diagnostics} source={source} />
          <SchemaPane
            source={schema}
            view={view?.schema ?? null}
            onChange={(next) => show(next, source, engine)}
          />
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
            <button
              type="button"
              className={tab === 'lowered' ? 'tab on' : 'tab'}
              onClick={() => setTab('lowered')}
            >
              lowered<span className="count">{view?.lowered.nodes.length ?? 0}</span>
            </button>
          </div>

          {tab === 'tokens' && (
            <TokenTable
              tokens={view?.tokens.tokens ?? []}
              highlight={highlight}
              onHighlight={setHighlight}
            />
          )}
          {tab === 'tree' && (
            <TreeView
              tree={view?.tree ?? { root: null, nodes: [], diagnostics: [] }}
              highlight={highlight}
              onHighlight={setHighlight}
            />
          )}
          {tab === 'lowered' && view && (
            <LoweredView lowered={view.lowered} highlight={highlight} onHighlight={setHighlight} />
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
