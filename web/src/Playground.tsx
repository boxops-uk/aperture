import { useEffect, useMemo, useState } from 'react'
import {
  type Database,
  type Engine,
  type Lowered,
  type SchemaView,
  type Tokens,
  type Trace,
  type Tree,
} from './wasm'
import type { Highlight } from './span'
import { Editor } from './Editor'
import { TokenTable } from './TokenTable'
import { TreeView } from './TreeView'
import { LoweredView } from './LoweredView'
import { PlanPane } from './PlanPane'
import { RunPane } from './RunPane'
import { DataTable } from './DataTable'
import { Section } from './Accordion'
import { Drawer } from './Drawer'
import { Split } from './Split'
import { fold } from './run'
import { usePlayback } from './playback'
import { SchemaPane } from './SchemaPane'
import { Diagnostics } from './Diagnostics'
import { useEngine } from './engine'
import './app.css'

/** What the front end says about the query, and what saying it cost. */
type Analysis = { tokens: Tokens; tree: Tree; lowered: Lowered; trace: Trace; micros: number }

/** What the schema says about itself — recomputed only when the schema changes. */
type SchemaAnalysis = { view: SchemaView; tokens: Tokens; database: Database }

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
  // The whole run, in the same breath: a trace over the demo database is tens
  // of transitions, and fetching it now is what makes stepping instant.
  const trace = engine.trace(schemaSource, source)
  return { tokens, tree, lowered, trace, micros: (performance.now() - started) * 1000 }
}

/**
 * The schema's own views, which depend on the schema alone.
 *
 * Kept apart from the query's for the obvious reason: lexing 150 lines of
 * schema on every keystroke of a *query* is work whose input did not change.
 */
function analyseSchema(engine: Engine, schemaSource: string): SchemaAnalysis {
  return {
    view: engine.schema(schemaSource),
    tokens: engine.lexSchema(schemaSource),
    // The database depends on the schema and nothing else, so it is read here
    // rather than per keystroke of a query.
    database: engine.database(schemaSource),
  }
}

/**
 * **The workbench** — every view of one query at once, over a real database.
 *
 * The book's demos are one panel each, because a paragraph is about one thing.
 * This is the page for the question a paragraph cannot hold: what the plan has
 * to do with the bytes, and what the machine is standing on while it does it.
 * A demo hands its query over here through the URL, so "what about…" keeps the
 * query a reader was already looking at.
 */
export function Playground() {
  const { engine, failure } = useEngine(true)
  const [highlight, setHighlight] = useState<Highlight | null>(null)
  const [at, setAt] = useState(0)
  const [split, setSplit] = useState(0.52)
  const [drawer, setDrawer] = useState(false)
  // Open by default: the run, and the plan it is executing. The rest are there
  // when a reader goes looking rather than in the way while they are not.
  const [opened, setOpened] = useState(new Set(['run', 'plan']))

  const toggle = (name: string) =>
    setOpened((current) => {
      const next = new Set(current)
      if (!next.delete(name)) next.add(name)
      return next
    })

  // What the reader has typed, if they have typed anything. Everything else is
  // derived: the analysis is a fold of the two texts, so there is no second
  // copy of it to fall out of step with them, and nothing to seed once the
  // module lands.
  const [edited, setEdited] = useState<{ schema: string; source: string } | null>(null)

  // What the page opens with — the demo schema and the first sample, unless a
  // demo in the book handed over the query the reader was already reading.
  const opening = useMemo(() => {
    if (!engine) return { schema: '', source: '' }
    const params = new URLSearchParams(window.location.search)
    return {
      schema: params.get('schema') || engine.sampleSchema,
      source: params.get('q') || engine.samples[0]?.source || '',
    }
  }, [engine])

  const { schema, source } = edited ?? opening

  const view: Analysis | null = useMemo(
    () => (engine ? analyse(engine, schema, source) : null),
    [engine, schema, source],
  )
  // The schema's own views depend on the schema alone, so a keystroke in the
  // query does not lex 150 lines of schema again.
  const schemaView: SchemaAnalysis | null = useMemo(
    () => (engine ? analyseSchema(engine, schema) : null),
    [engine, schema],
  )

  const playback = usePlayback(view?.trace.steps.length ?? 0, at, setAt)

  // Every change of either text comes through here, so this is where a run in
  // progress ends: whatever was playing was playing something else.
  const show = (schema: string, source: string) => {
    setAt(0)
    playback.setPlaying(false)
    setEdited({ schema, source })
  }

  useEffect(() => {
    document.title = 'Playground · Fjord DB'
  }, [])

  // One fold, three panels: the transport, the plan and the table all show the
  // same moment.
  const moment = useMemo(() => (view ? fold(view.trace, at) : null), [view, at])

  const here = view?.trace.steps[Math.min(at, Math.max(view.trace.steps.length - 1, 0))]
  const examined = here?.examined ?? []
  // Which plan step the machine is standing at — `null` on the head, where it
  // is standing on no step at all.
  const standing =
    here && here.depth < (view?.lowered.plan?.steps_count ?? 0) ? here.depth : null
  // One list, from the compilation: it carries the lexer's and the parser's
  // faults as well as its own, in the order `Diagnostics::in_source_order` puts
  // them — so showing the parse's separately would print every one twice.
  const diagnostics = view ? view.lowered.diagnostics : []

  return (
    <div className="page">
      {/* One row, and no taller than what is in it: a title, the schema, and the
          three numbers worth watching. Everything the prose used to say is
          demonstrated by the panels underneath it. */}
      <header className="top">
        <h1>Playground</h1>
        <div className="tools">
          <button type="button" className="chip schema-open" onClick={() => setDrawer(true)}>
            schema
            {schemaView && !schemaView.view.ok && <span className="bad"> ✕</span>}
          </button>
          <Status engine={engine} failure={failure} micros={view?.micros} />
        </div>
      </header>

      <div className="samples">
        {(engine?.samples ?? []).map((sample) => (
          <button
            key={sample.label}
            type="button"
            className={source === sample.source ? 'chip on' : 'chip'}
            onClick={() => show(schema, sample.source)}
          >
            {sample.label}
          </button>
        ))}
      </div>

      {/* A split: the views on the left, the database on the right, and a grip
          between them — which side deserves the width depends on what a reader
          is doing, so it is theirs to decide. */}
      <Split
        fraction={split}
        onFraction={setSplit}
        left={
          <div className="stack">
            <div className="pane">
              <h2>source</h2>
              <Editor
                source={source}
                tokens={view?.tokens.tokens ?? []}
                highlight={highlight}
                onChange={(next) => show(schema, next)}
                onHighlight={setHighlight}
              />
              <Diagnostics diagnostics={diagnostics} source={source} />
            </div>

            {/* An accordion, not tabs: the plan is meant to be read *against*
                the run that is executing it. */}
            <Section
              name="run"
              count={`${view?.trace.rows ?? 0} rows`}
              open={opened.has('run')}
              onToggle={() => toggle('run')}
            >
              {moment && (
                <RunPane
                  trace={view?.trace ?? null}
                  plan={view?.lowered.plan ?? null}
                  at={at}
                  moment={moment}
                  onSeek={setAt}
                  playback={playback}
                />
              )}
            </Section>

            <Section
              name="plan"
              count={`${view?.lowered.plan?.levels ?? 0} levels`}
              open={opened.has('plan')}
              onToggle={() => toggle('plan')}
            >
              <PlanPane
                plan={view?.lowered.plan ?? null}
                refused={(view?.lowered.diagnostics.length ?? 0) > 0}
                active={standing}
                examined={examined}
              />
            </Section>

            <Section
              name="lowered"
              count={view?.lowered.nodes.length ?? 0}
              open={opened.has('lowered')}
              onToggle={() => toggle('lowered')}
            >
              {view && (
                <LoweredView
                  lowered={view.lowered}
                  highlight={highlight}
                  onHighlight={setHighlight}
                />
              )}
            </Section>

            <Section
              name="parse tree"
              count={view?.tree.nodes.length ?? 0}
              open={opened.has('tree')}
              onToggle={() => toggle('tree')}
            >
              <TreeView
                tree={view?.tree ?? { root: null, nodes: [], diagnostics: [] }}
                highlight={highlight}
                onHighlight={setHighlight}
              />
            </Section>

            <Section
              name="tokens"
              count={view?.tokens.tokens.length ?? 0}
              open={opened.has('tokens')}
              onToggle={() => toggle('tokens')}
            >
              <TokenTable
                tokens={view?.tokens.tokens ?? []}
                highlight={highlight}
                onHighlight={setHighlight}
              />
            </Section>
          </div>
        }
        right={<DataTable database={schemaView?.database ?? null} moment={moment} at={at} />}
      />

      <Drawer
        open={drawer}
        title={
          <>
            <span className="what">schema</span>
            {schemaView && (
              <span className={schemaView.view.ok ? 'summary' : 'summary bad'}>
                {schemaView.view.ok
                  ? `${schemaView.view.predicates.length} predicates`
                  : `${schemaView.view.diagnostics.length} problem(s)`}
              </span>
            )}
          </>
        }
        onClose={() => setDrawer(false)}
      >
        <SchemaPane
          source={schema}
          view={schemaView?.view ?? null}
          tokens={schemaView?.tokens ?? null}
          onChange={(next) => show(next, source)}
        />
      </Drawer>
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
