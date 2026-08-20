import { useEffect, useState } from 'react'
import { load, type Engine, type Lowered, type SchemaView, type Tokens, type Tree } from './wasm'
import type { Highlight } from './span'
import { Editor } from './Editor'
import { TokenTable } from './TokenTable'
import { TreeView } from './TreeView'
import { LoweredView } from './LoweredView'
import { PlanPane } from './PlanPane'
import { SchemaPane } from './SchemaPane'
import { Diagnostics } from './Diagnostics'
import './app.css'

/** What the front end says about the query, and what saying it cost. */
type Analysis = { tokens: Tokens; tree: Tree; lowered: Lowered; micros: number }

/** What the schema says about itself — recomputed only when the schema changes. */
type SchemaAnalysis = { view: SchemaView; tokens: Tokens }

/**
 * Everything the front end says about a query, and what the whole round trip
 * cost — every call, the JSON, and parsing it back — because that is what a
 * keystroke actually costs a page. The engine's own share is far smaller, and
 * printing that number would be measuring something the reader cannot see.
 *
 * `compile` re-reads the schema every time, because the module holds no state:
 * two strings in, JSON out, and no handle a page would have to free. That is
 * the floor, and it is a schema parse per keystroke — worth knowing, and worth
 * a handle if a bigger schema ever makes it hurt.
 *
 * Outside the component because it reads a clock: a measurement taken during
 * render is one that changes when React re-renders for reasons of its own.
 */
function analyse(engine: Engine, schemaSource: string, source: string): Analysis {
  const started = performance.now()
  const tokens = engine.lex(source)
  const tree = engine.parse(source)
  const lowered = engine.compile(schemaSource, source)
  return { tokens, tree, lowered, micros: (performance.now() - started) * 1000 }
}

/**
 * The schema's own views, which depend on the schema alone.
 *
 * Kept apart from the query's for the obvious reason: lexing 150 lines of
 * schema on every keystroke of a *query* is work whose input did not change.
 */
function analyseSchema(engine: Engine, schemaSource: string): SchemaAnalysis {
  return { view: engine.schema(schemaSource), tokens: engine.lexSchema(schemaSource) }
}

export default function App() {
  const [engine, setEngine] = useState<Engine | null>(null)
  const [failure, setFailure] = useState<string | null>(null)
  const [highlight, setHighlight] = useState<Highlight | null>(null)
  const [tab, setTab] = useState<'tokens' | 'tree' | 'lowered' | 'plan'>('plan')

  // Schema, query and analysis move together, updated by whatever changed
  // either text — a keystroke, a sample, or the engine finishing loading. The
  // schema's own views ride along, recomputed only when the schema itself did.
  const [state, setState] = useState<{
    schema: string
    source: string
    view: Analysis | null
    schemaView: SchemaAnalysis | null
  }>({ schema: '', source: '', view: null, schemaView: null })

  const show = (schema: string, source: string, engine: Engine | null) =>
    setState((current) => ({
      schema,
      source,
      view: engine ? analyse(engine, schema, source) : null,
      schemaView:
        engine && schema !== current.schema
          ? analyseSchema(engine, schema)
          : current.schemaView,
    }))

  useEffect(() => {
    load().then(
      (loaded) => {
        setEngine(loaded)
        const schema = loaded.sampleSchema
        const source = loaded.samples[0]?.source ?? ''
        setState({
          schema,
          source,
          view: analyse(loaded, schema, source),
          schemaView: analyseSchema(loaded, schema),
        })
      },
      (error: unknown) => setFailure(String(error)),
    )
  }, [])

  const { schema, source, view, schemaView } = state
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
            view={schemaView?.view ?? null}
            tokens={schemaView?.tokens ?? null}
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
            <button
              type="button"
              className={tab === 'plan' ? 'tab on' : 'tab'}
              onClick={() => setTab('plan')}
            >
              plan<span className="count">{view?.lowered.plan?.levels ?? 0}</span>
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
          {tab === 'plan' && (
            <PlanPane
              plan={view?.lowered.plan ?? null}
              refused={(view?.lowered.diagnostics.length ?? 0) > 0}
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
