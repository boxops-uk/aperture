import { Badge } from '@astryxdesign/core/Badge'
import { Transport } from './Transport'
import type { PlanView, Trace } from './wasm'
import type { Moment } from './run'

/**
 * **The debugger** — the run, one transition at a time.
 *
 * The whole trace arrives in one call, so stepping is a local index into an
 * array: forwards, backwards, to the next row, to the end, all instant and none
 * of them asking the engine again. What the panels show is a *fold* of the
 * steps up to that index, because each step carries only what it changed.
 *
 * The machine's own vocabulary is kept: a **step** is one transition of the
 * executor's loop, a **yield** is the moment a run would hand a row back, and a
 * **reject** is a row a residual read and dropped — the rows that are invisible
 * in the answer and are the whole difference between a seek and a scan.
 */
export function RunPane({
  trace,
  plan,
  at,
  moment,
  onSeek,
  playback,
}: {
  trace: Trace | null
  plan: PlanView | null
  at: number
  moment: Moment
  onSeek: (at: number) => void
  playback: { playing: boolean; setPlaying: (playing: boolean) => void }
}) {
  if (!trace || trace.steps.length === 0) {
    return (
      <div className="scroller">
        <p className="empty">
          {trace?.diagnostics.length
            ? 'nothing to run — the query was refused'
            : 'nothing to run yet'}
        </p>
      </div>
    )
  }

  const here = Math.min(at, trace.steps.length - 1)
  const step = trace.steps[here]
  const state = moment

  return (
    <div className="scroller run">
      <Transport trace={trace} at={at} onSeek={onSeek} playback={playback} />

      <p className={`event ${step.event}`}>
        <Badge
          variant={
            step.event === 'yield'
              ? 'green'
              : step.event === 'reject'
                ? 'red'
                : step.event === 'done'
                  ? 'blue'
                  : 'neutral'
          }
          label={step.event}
        />
        {step.event === 'yield' && <span className="said">answered {show(step.row)}</span>}
        {step.event === 'scan' && step.scanning && (
          <span className="said">
            {step.scanning.fetch
              ? `one row, by reference — ${step.scanning.fetch}`
              : `over ${step.scanning.lo}…${step.scanning.hi ?? 'the end'}`}
          </span>
        )}
        {step.event === 'reject' && step.rejected && (
          <span className="said">
            read {step.rejected.row.fact} and dropped it —{' '}
            <code>{residualOf(plan, step.rejected.step, step.rejected.residual)}</code>
          </span>
        )}
        {step.event === 'step' && (
          <span className="said">
            {step.depth >= (plan?.steps_count ?? 0)
              ? 'standing on the head'
              : `at ${plan?.steps[step.depth]?.kind.toLowerCase() ?? 'step'} ${step.depth}`}
          </span>
        )}
        {step.event === 'done' && <span className="said">every level drained</span>}
      </p>

      <dl className="shape">
        <div>
          <dt>rows so far</dt>
          <dd>{state.rows.length}</dd>
        </div>
        <div>
          <dt>examined</dt>
          <dd>{step.examined.reduce((a, b) => a + b, 0)}</dd>
        </div>
        <div>
          <dt>depth</dt>
          <dd>{step.depth}</dd>
        </div>
      </dl>

      <section className="registers">
        <h3>registers</h3>
        {state.registers.length === 0 && <p className="empty">none bound yet</p>}
        <ul>
          {state.registers.map((register) => (
            <li key={register.address} className={register.written ? 'written' : undefined}>
              <code className="address">r{register.address}</code>
              {register.fact && <code className="fact">{register.fact}</code>}
              <code className="value">{show(register.value)}</code>
            </li>
          ))}
        </ul>
      </section>

      <section className="yielded">
        <h3>
          yielded<span className="count">{state.rows.length}</span>
        </h3>
        {state.rows.length === 0 && <p className="empty">nothing yet</p>}
        <ol>
          {state.rows.map((row, index) => (
            <li key={index} className={step.event === 'yield' && index === state.rows.length - 1 ? 'fresh' : undefined}>
              <code>{show(row)}</code>
            </li>
          ))}
        </ol>
      </section>

      {trace.truncated && (
        <p className="capped">
          the run was cut off at {trace.steps.length} transitions — it is longer than this
        </p>
      )}
    </div>
  )
}

/** Which filter dropped a row, in the plan's own words. */
function residualOf(plan: PlanView | null, step: number, residual: number): string {
  const lines = plan?.steps[step]?.text.split('\n').filter((line) => line.includes('where')) ?? []
  return lines[residual]?.trim() ?? `residual ${residual}`
}

function show(value: unknown): string {
  return typeof value === 'string' ? `"${value}"` : JSON.stringify(value) ?? '—'
}

