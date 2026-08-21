import { Fragment } from 'react'

/**
 * A decoded value, painted.
 *
 * The same vocabulary the source editor uses — a string is a string in both
 * panels — with one addition the query language has no token for: a
 * **reference** reads as `code.File#2` and is coloured as the fact it names,
 * because following one is the join a reader is looking for.
 *
 * Written against the parsed value rather than over its text: colouring
 * `JSON.stringify` output with regexes would be a second parser of a format
 * that is already in hand.
 */
export function Json({ value }: { value: unknown }) {
  if (value === null || value === undefined) return <span className="j-null">—</span>
  if (typeof value === 'number' || typeof value === 'boolean')
    return <span className="j-num">{String(value)}</span>

  if (typeof value === 'string') {
    // `code.File#2` — a fact, not a string that happens to look like one: the
    // view writes references this way and nothing else in a decoded row can.
    return /^[a-z][\w.]*#\d+$/.test(value) ? (
      <span className="j-ref">{value}</span>
    ) : (
      <span className="j-str">"{value}"</span>
    )
  }

  if (Array.isArray(value)) {
    return (
      <>
        <span className="j-pun">[</span>
        {value.map((item, index) => (
          <Fragment key={index}>
            {index > 0 && <span className="j-pun">, </span>}
            <Json value={item} />
          </Fragment>
        ))}
        <span className="j-pun">]</span>
      </>
    )
  }

  return (
    <>
      <span className="j-pun">{'{'}</span>
      {Object.entries(value as Record<string, unknown>).map(([key, item], index) => (
        <Fragment key={key}>
          {index > 0 && <span className="j-pun">, </span>}
          <span className="j-key">{key}</span>
          <span className="j-pun">: </span>
          <Json value={item} />
        </Fragment>
      ))}
      <span className="j-pun">{'}'}</span>
    </>
  )
}
