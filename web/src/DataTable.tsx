import { useEffect, useMemo, useState } from 'react'
import type { Database, RowBytes } from './wasm'
import { inRange, type Moment } from './run'
import { Json } from './Json'
import { plain } from './plain'
import { useColumns } from './columns'
import { Badge } from '@astryxdesign/core/Badge'
import { Icon } from '@astryxdesign/core/Icon'

/**
 * **The database, as the machine sees it** — every stored row, in key order, as
 * bytes *and* as a fact, with the range the current scan is walking shaded
 * across it.
 *
 * This is the panel the plan's numbers are about. A seek is a byte prefix and a
 * scan is a range over the same order, so `[lo, hi)` is a *band* here and
 * nothing at all against decoded values — which is why the hex is a column and
 * not a debugging afterthought.
 *
 * **One table, sectioned by predicate**, rather than a card each: the columns
 * then line up down the whole page, which is the only way comparing bytes
 * across predicates works. Sections collapse, columns are draggable, and every
 * cell truncates rather than wrapping — a row that grows to three lines moves
 * every row under it, which is exactly what a reader watching one row cannot
 * afford.
 *
 * Four states a row can be in, and between them they are the whole story of a
 * query: outside the range the scan will never read; inside it and not yet
 * reached; **read and dropped** by a residual; and **held**, which is the row a
 * register is standing on.
 *
 * Stepping **folds the predicates the machine is not touching**. A join is
 * standing in two places at once and its two predicates are rarely neighbours,
 * so scrolling to *the* row is a question with no answer; folding the rest
 * brings whatever matters onto one screen without picking a winner between
 * them, and a reader who wants a folded one back need only click it.
 */
/** What each column is worth of the pane, before anybody says otherwise. */
const COLUMNS = [0.15, 0.34, 0.33, 0.18]

export function DataTable({
  database,
  moment,
  at,
  collapsed,
}: {
  database: Database | null
  moment: Moment | null
  /** The step being shown: a move is a step *or* a different query. */
  at: number
  /** There is no query: fold the whole table rather than leaving it open on
   *  the predicates the last one happened to be about. */
  collapsed?: boolean
}) {
  // The key is the long one and the reason the pane is wide; the fact and the
  // value are a name and a short string.
  const { fractions, start, measure } = useColumns(COLUMNS)
  const [closed, setClosed] = useState(new Set<number>())

  // Which predicate the current scan is in: a stored key begins with its
  // predicate's id, so the range says so in its first four bytes.
  const scanningIn = moment?.scanning && !moment.scanning.fetch ? predicateOf(moment.scanning.lo) : null

  // Everything this step is about: the range being walked, the row being
  // fetched, the rows the registers stand on, and the row a residual just
  // dropped. Disjoint, in general — which is the point.
  const relevant = useMemo(() => {
    const ids = new Set<number>()
    const add = (id: number | null) => id !== null && ids.add(id)
    if (moment?.scanning) add(predicateOf(moment.scanning.fetch ?? moment.scanning.lo))
    for (const key of moment?.held ?? []) add(predicateOf(key))
    if (moment?.dropped) add(predicateOf(moment.dropped))
    return ids
  }, [moment])

  // On a move, and only on a move: fold what this step is not about. Whatever
  // the reader opened by hand survives until the machine moves somewhere else.
  //
  // Keyed on *which* predicates matter rather than on the step number: a new
  // query starts at step 0 like the last one did, and a table that only
  // re-folds when the number changes would still be folded around the query
  // before it.
  const predicates = database?.predicates
  const matters = [...relevant].sort((a, b) => a - b).join(',')
  useEffect(() => {
    if (!predicates) return
    if (collapsed) {
      setClosed(new Set(predicates.map((it) => it.id)))
      return
    }
    if (relevant.size === 0) return
    setClosed(new Set(predicates.map((it) => it.id).filter((id) => !relevant.has(id))))
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [at, matters, predicates, collapsed])

  if (!database) return null

  const columns = ['fact', 'stored key', 'decoded', 'value']

  return (
    <section className="data">
      <h2>
        database
        <span className="count">
          <Badge variant="neutral" label={`${database.facts} facts`} />
        </span>
        {moment?.scanning && !moment.scanning.fetch && (
          <span className="range">
            scanning <code>{moment.scanning.lo}</code> …{' '}
            <code>{moment.scanning.hi ?? 'end of predicate'}</code>
          </span>
        )}
        {moment?.scanning?.fetch && (
          <span className="range">
            fetching one row — <code>{moment.scanning.fetch}</code>
          </span>
        )}
      </h2>

      <div className="scroll" ref={measure}>
        <table>
          <colgroup>
            {fractions.map((fraction, index) => (
              <col key={index} style={{ width: `${fraction * 100}%` }} />
            ))}
          </colgroup>
          <thead>
            <tr>
              {columns.map((name, index) => (
                <th key={name}>
                  <span>{name}</span>
                  {index < columns.length - 1 && (
                    <span
                      className="handle"
                      role="separator"
                      aria-label={`resize ${name}`}
                      onPointerDown={(event) => {
                        event.preventDefault()
                        start(index, event.clientX)
                      }}
                    />
                  )}
                </th>
              ))}
            </tr>
          </thead>

          {database.predicates.map((predicate) => {
            const open = !closed.has(predicate.id)
            const scanning = scanningIn === predicate.id

            return (
              <tbody key={predicate.id} className={scanning ? 'scanning' : undefined}>
                <tr className="section">
                  <th colSpan={columns.length}>
                    <button
                      type="button"
                      onClick={() =>
                        setClosed((current) => {
                          const next = new Set(current)
                          if (!next.delete(predicate.id)) next.add(predicate.id)
                          return next
                        })
                      }
                      aria-expanded={open}
                    >
                      <code className="name">{predicate.name}</code>
                      {/* A badge, not a bare number: at the end of a row whose
                          last column is the value, a loose count reads as one. */}
                      <span className="count">
                        <Badge
                          label={`${predicate.rows.length} rows`}
                          variant={relevant.has(predicate.id) ? 'red' : 'neutral'}
                        />
                      </span>
                      {/* The same indicator, on the same side, as the accordion
                          on the other half of the page: one page, one language
                          for "this opens". */}
                      <span className={open ? 'arrow open' : 'arrow'}>
                        <Icon icon="chevronDown" size="sm" color="secondary" />
                      </span>
                    </button>
                  </th>
                </tr>

                {open && predicate.rows.map((row) => <Row key={row.key} row={row} moment={moment} />)}
              </tbody>
            )
          })}
        </table>
      </div>
    </section>
  )
}

function Row({ row, moment }: { row: RowBytes; moment: Moment | null }) {
  const held = moment?.held.has(row.key) ?? false
  const dropped = moment?.dropped === row.key
  const seen = moment?.droppedSoFar.has(row.key) ?? false
  const within = inRange(row.key, moment?.scanning ?? null)

  return (
    <tr
      className={[within ? 'within' : '', held ? 'held' : '', dropped ? 'dropped' : seen ? 'seen' : '']
        .filter(Boolean)
        .join(' ')}
    >
      <td className="fact" title={row.fact}>
        {row.fact}
      </td>
      <td className="bytes" title={row.key}>
        <Bytes hex={row.key} scanning={moment?.scanning ?? null} />
      </td>
      <td className="decoded" title={plain(row.decoded)}>
        <Json value={row.decoded} />
      </td>
      <td className="value" title={plain(row.value_decoded)}>
        {row.value !== null && <Json value={row.value_decoded} />}
      </td>
    </tr>
  )
}

/** The predicate a stored key belongs to: its id is the key's first four bytes. */
function predicateOf(hex: string): number | null {
  return hex.length >= 8 ? Number.parseInt(hex.slice(0, 8), 16) : null
}

/**
 * The stored key, with the bytes the current scan **pinned** marked off from
 * the ones it left free.
 *
 * That boundary is the whole cost model in one place: everything left of it the
 * seek jumped straight to, everything right of it the scan walks.
 */
function Bytes({ hex, scanning }: { hex: string; scanning: Moment['scanning'] }) {
  if (!scanning || scanning.fetch || !inRange(hex, scanning)) return <>{hex}</>

  let shared = 0
  while (shared < scanning.lo.length && hex[shared] === scanning.lo[shared]) shared++
  // Whole bytes only: half a byte pinned is not a thing a seek can do.
  shared -= shared % 2

  return (
    <>
      <span className="pinned">{hex.slice(0, shared)}</span>
      {hex.slice(shared)}
    </>
  )
}
