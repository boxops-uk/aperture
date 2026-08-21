import { Badge } from '@astryxdesign/core/Badge'
import type { PlanView } from './wasm'

/**
 * The plan: what the query does, and in what order.
 *
 * The text of each step is the engine's own — `print::steps`, the same renderer
 * `fjord query --plan` shows — and what this adds around it is the structure a
 * reader wants to count: which register a step fills, whether it scans or seeks,
 * how many rows it reads and drops.
 *
 * **Levels are not steps**, and the header says both. A resume cursor holds one
 * row per *level*; a derive and a test bind nothing and take no cursor entry.
 *
 * While a run is being stepped the step the machine is standing at is lit, and
 * each step carries what it has read *so far* — so the plan stops being a
 * description and becomes the thing being executed in front of you.
 */
export function PlanPane({
  plan,
  refused,
  active,
  examined,
}: {
  plan: PlanView | null
  refused: boolean
  /** The step the machine is standing at, while a run is being stepped. */
  active: number | null
  /** Rows examined per step, as they stand at that moment. */
  examined: number[]
}) {
  if (!plan) {
    return (
      <div className="scroller">
        <p className="empty">
          {refused
            ? 'no plan — the query was refused, and a plan exists exactly when nothing was reported'
            : 'no plan yet'}
        </p>
      </div>
    )
  }

  return (
    <div className="scroller plan">
      <ol className="steps">
        {plan.steps.map((step) => (
          <li key={step.index} className={step.index === active ? 'on' : undefined}>
            <div className="head">
              <span className="index">{step.level === null ? '·' : step.level}</span>
              {/* Every chip on this page is the same pill: what varies is the
                  colour, and colour is the only thing that should. */}
              <Badge variant={step.kind === 'Level' ? 'blue' : 'neutral'} label={step.kind} />
              {step.access.map((access, at) => (
                <Badge key={at} variant={access === 'seek' ? 'green' : 'neutral'} label={access} />
              ))}
              {step.residuals > 0 && (
                <Badge
                  variant="red"
                  label={`${step.residuals} residual${step.residuals === 1 ? '' : 's'}`}
                />
              )}
              {/* What this step has actually read, so far — the outcome beside
                  the intent, which is the pair worth reading together. */}
              {(examined[step.index] ?? 0) > 0 && (
                <Badge variant="neutral" label={`${examined[step.index]} read`} />
              )}
            </div>
            {/* The printer indents its whole block by two spaces, which is
                right in a terminal and wrong inside a box that is already
                indented. Nothing else about the text is touched. */}
            <pre>{step.text.replace(/^ {2}/gm, '')}</pre>
          </li>
        ))}
        <li className="head-row">
          <div className="head">
            <span className="index">→</span>
            <Badge variant="neutral" label="head" />
          </div>
          <pre>{plan.head}</pre>
        </li>
      </ol>
    </div>
  )
}
