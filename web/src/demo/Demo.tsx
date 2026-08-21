import { useMemo, useState } from 'react'
import { useEngine } from '../engine'
import { fold } from '../run'
import { usePlayback } from '../playback'
import { route } from '../book/markdown'
import type { Demo as Spec } from '../book/markdown'
import type { Highlight } from '../span'
import { Editor } from '../Editor'
import { Diagnostics } from '../Diagnostics'
import { TokenTable } from '../TokenTable'
import { TreeView } from '../TreeView'
import { LoweredView } from '../LoweredView'
import { PlanPane } from '../PlanPane'
import { RunPane } from '../RunPane'
import { DataTable } from '../DataTable'
import { SchemaPane } from '../SchemaPane'
import { Transport } from '../Transport'

/**
 * **A demo in the middle of the prose** — the real engine, compiled to
 * WebAssembly, doing the thing the paragraph above it just described.
 *
 * Every one of these was a code block with an answer typed underneath it, and
 * a typed answer is a claim that rots quietly: the lexer gains a token kind,
 * the planner learns to reorder one more shape, and the page goes on saying
 * what used to happen. Here the page has no answers in it. It has the engine.
 *
 * Each is editable, because the second question a reader has is always "what
 * about…", and the honest way to answer it is to let them type it.
 */
const WHAT: Record<string, string> = {
  lex: 'the lexer, on every keystroke',
  parse: 'the parser, on every keystroke',
  types: 'the typechecker, against the schema',
  plan: 'the plan this compiles to',
  run: 'the machine, one transition at a time',
  store: 'the rows this reads, as stored bytes',
  schema: 'the schema, as the engine reads it',
}

export function Demo({ demo }: { demo: Spec }) {
  // A demo is why the module is fetched at all — the prose around it is not.
  const { engine, failure } = useEngine(true)
  const schemaDemo = demo.kind === 'schema'

  const [query, setQuery] = useState(schemaDemo ? '' : demo.query)
  const [schema, setSchema] = useState(schemaDemo ? demo.query : demo.schema)
  const [highlight, setHighlight] = useState<Highlight | null>(null)
  const [at, setAt] = useState(0)

  // No schema in the block means the demo database's own — the one every
  // sample on this site is written against, and the only one with rows behind
  // it.
  const schemaSource = schema || engine?.sampleSchema || ''

  const analysis = useMemo(() => {
    if (!engine) return null
    try {
      const stepping = demo.kind === 'run' || demo.kind === 'store'
      const compiled =
        demo.kind === 'types' || demo.kind === 'plan' || stepping
          ? engine.compile(schemaSource, query)
          : null
      return {
        tokens: schemaDemo ? engine.lexSchema(schemaSource) : engine.lex(query),
        tree: demo.kind === 'parse' ? engine.parse(query) : null,
        lowered: compiled,
        trace: stepping ? engine.trace(schemaSource, query) : null,
        database: demo.kind === 'store' ? engine.database(schemaSource) : null,
        schemaView: schemaDemo ? engine.schema(schemaSource) : null,
        broke: null as string | null,
      }
    } catch (error: unknown) {
      // A demo that throws is a demo, not the page it is on.
      return {
        tokens: null,
        tree: null,
        lowered: null,
        trace: null,
        database: null,
        schemaView: null,
        broke: String(error),
      }
    }
  }, [engine, query, schemaSource, demo.kind, schemaDemo])

  const trace = analysis?.trace ?? null
  const playback = usePlayback(trace?.steps.length ?? 0, at, setAt)
  const moment = useMemo(() => (trace ? fold(trace, at) : null), [trace, at])

  // A new query is a new run, and the old play head means nothing against it.
  const retype = (next: string) => {
    playback.setPlaying(false)
    setAt(0)
    if (schemaDemo) setSchema(next)
    else setQuery(next)
  }

  const here = trace ? Math.min(at, trace.steps.length - 1) : 0
  const step = trace?.steps[here]

  return (
    <section className={`demo demo-${demo.kind}`}>
      <header className="demo-head">
        <span className="demo-what">{WHAT[demo.kind] ?? 'the engine, live'}</span>
        <a className="demo-open" href={playgroundLink(schemaDemo ? '' : query, demo.schema)}>
          open in the playground
        </a>
      </header>

      {failure && (
        <p className="demo-broken">
          the engine did not load — <code>{failure}</code>
        </p>
      )}

      {!engine && !failure && <p className="demo-waiting">loading the engine…</p>}

      {engine && analysis && (
        <>
          {schemaDemo ? (
            <SchemaPane
              source={schemaSource}
              view={analysis.schemaView}
              tokens={analysis.tokens}
              onChange={retype}
            />
          ) : (
            <Editor
              source={query}
              tokens={analysis.tokens?.tokens ?? []}
              highlight={highlight}
              onChange={retype}
              onHighlight={setHighlight}
            />
          )}

          {/* A demo written against a schema of its own says which one: the
              query resolves its names against it, and a reader cannot check a
              plan without knowing the key order it was planned for. */}
          {demo.schema && !schemaDemo && (
            <details className="demo-schema">
              <summary>against this schema</summary>
              <pre>
                <code>{demo.schema}</code>
              </pre>
            </details>
          )}

          {analysis.broke && (
            <p className="demo-broken">
              the engine refused this outright — <code>{analysis.broke}</code>
            </p>
          )}

          {demo.kind === 'lex' && analysis.tokens && (
            <>
              <TokenTable
                tokens={analysis.tokens.tokens}
                highlight={highlight}
                onHighlight={setHighlight}
              />
              <Diagnostics diagnostics={analysis.tokens.diagnostics} source={query} />
            </>
          )}

          {demo.kind === 'parse' && analysis.tree && (
            <>
              <TreeView tree={analysis.tree} highlight={highlight} onHighlight={setHighlight} />
              <Diagnostics diagnostics={analysis.tree.diagnostics} source={query} />
            </>
          )}

          {demo.kind === 'types' && analysis.lowered && (
            <>
              <LoweredView
                lowered={analysis.lowered}
                highlight={highlight}
                onHighlight={setHighlight}
              />
              <Diagnostics diagnostics={analysis.lowered.diagnostics} source={query} />
            </>
          )}

          {demo.kind === 'plan' && analysis.lowered && (
            <>
              <PlanPane
                plan={analysis.lowered.plan}
                refused={analysis.lowered.diagnostics.length > 0}
                active={null}
                examined={[]}
              />
              <Diagnostics diagnostics={analysis.lowered.diagnostics} source={query} />
            </>
          )}

          {demo.kind === 'run' && moment && (
            <>
              <RunPane
                trace={trace}
                plan={analysis.lowered?.plan ?? null}
                at={at}
                moment={moment}
                onSeek={setAt}
                playback={playback}
              />
              <Diagnostics diagnostics={analysis.lowered?.diagnostics ?? []} source={query} />
            </>
          )}

          {demo.kind === 'store' && (
            <>
              {trace && trace.steps.length > 0 && (
                <Transport trace={trace} at={at} onSeek={setAt} playback={playback} />
              )}
              <DataTable database={analysis.database} moment={moment} at={at} />
              {step && (
                <p className="demo-note">
                  {step.scanning
                    ? step.scanning.fetch
                      ? `one row, by reference — ${step.scanning.fetch}`
                      : 'the shaded band is the range this level walks'
                    : 'step the run to watch the ranges move'}
                </p>
              )}
              <Diagnostics diagnostics={analysis.lowered?.diagnostics ?? []} source={query} />
            </>
          )}
        </>
      )}
    </section>
  )
}

/** The same query, in the workbench, with everything at once. */
function playgroundLink(query: string, schema: string): string {
  const params = new URLSearchParams()
  if (query) params.set('q', query)
  if (schema) params.set('schema', schema)
  const search = params.toString()
  return route('playground') + (search ? `?${search}` : '')
}
