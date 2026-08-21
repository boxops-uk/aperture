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
import { Toolbar } from '@astryxdesign/core/Toolbar'
import { Button } from '@astryxdesign/core/Button'
import { Selector } from '@astryxdesign/core/Selector'
import { Text } from '@astryxdesign/core/Text'
import { Spinner } from '@astryxdesign/core/Spinner'
import { VStack, HStack } from '@astryxdesign/core/Stack'
import { Layout as Panes, LayoutContent, LayoutPanel } from '@astryxdesign/core/Layout'
import { ResizeHandle, useResizable } from '@astryxdesign/core/Resizable'
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
  // Which side deserves the width depends on what a reader is doing, so it is
  // theirs to decide — and it is remembered, because they decided it once.
  const database = useResizable({
    defaultSize: 620,
    minSizePx: 380,
    maxSizePx: 1100,
    autoSaveId: 'fjord-database',
  })
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
    <VStack height="100%" gap={0} className="page">
      {/* One row, and no taller than what is in it: the schema, and the three
          numbers worth watching. Everything the prose used to say is
          demonstrated by the panels underneath it. */}
      <Toolbar
        label="Playground"
        size="sm"
        // A select rather than fourteen buttons: the samples are a list to pick
        // from, and a list that does not fit on two rows is a list.
        startContent={
          <Selector
            label="Sample query"
            isLabelHidden
            size="sm"
            width={240}
            variant="ghost"
            placeholder="samples"
            value={engine?.samples.find((sample) => sample.source === source)?.label}
            options={(engine?.samples ?? []).map((sample) => ({
              value: sample.label,
              label: sample.label,
            }))}
            onChange={(label) => {
              const sample = engine?.samples.find((entry) => entry.label === label)
              if (sample) show(schema, sample.source)
            }}
          />
        }
        endContent={
          <HStack gap={4} align="center">
            <Button
              variant="secondary"
              size="sm"
              label="schema"
              onClick={() => setDrawer(true)}
              data-testid="schema"
              endContent={schemaView && !schemaView.view.ok ? <Text color="accent">✕</Text> : undefined}
            />
            <Status engine={engine} failure={failure} micros={view?.micros} />
          </HStack>
        }
      />

      {/* The views on the left, the database on the right, and a grip between
          them — which side deserves the width depends on what a reader is
          doing, so it is theirs to decide. */}
      <Panes
        content={
          <LayoutContent isScrollable padding={0}>
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
          </LayoutContent>
        }
        end={
          <>
            {/* The handle is a component beside the panel rather than a prop on
                it: the panel takes the width the hook holds, and the handle is
                what changes it. */}
            <ResizeHandle
              direction="horizontal"
              isReversed
              hasDivider
              resizable={database.props}
              label="Resize the database"
            />
            <LayoutPanel
              label="Database"
              width={database.size}
              isScrollable
              padding={0}
            >
              <DataTable database={schemaView?.database ?? null} moment={moment} at={at} />
            </LayoutPanel>
          </>
        }
      />

      <Drawer
        open={drawer}
        summary={
          schemaView?.view.ok
            ? `${schemaView.view.predicates.length} predicates`
            : `${schemaView?.view.diagnostics.length ?? 0} problem(s)`
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
    </VStack>
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
      <Text type="supporting" color="accent">
        the engine did not load — run scripts/build-wasm.sh
      </Text>
    )
  }
  if (!engine)
    return (
      <HStack gap={2} align="center">
        <Spinner size="sm" />
        <Text type="supporting">loading the engine…</Text>
      </HStack>
    )
  return (
    <HStack gap={4} align="center" wrap="nowrap">
      <Number label="fjord" value={engine.version} />
      <Number label="module" value={`${(engine.bytes / 1024).toFixed(1)} KiB`} />
      <Number
        label="round trip"
        value={micros === undefined ? '—' : `${micros.toFixed(0)} µs`}
      />
    </HStack>
  )
}

/** One of the three numbers worth watching, and what it is. */
function Number({ label, value }: { label: string; value: string }) {
  return (
    <HStack gap={1} align="center" wrap="nowrap">
      <Text type="supporting" textWrap="nowrap">
        {label}
      </Text>
      <Text type="label" hasTabularNumbers textWrap="nowrap">
        {value}
      </Text>
    </HStack>
  )
}
