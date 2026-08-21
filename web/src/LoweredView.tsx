import { Badge } from '@astryxdesign/core/Badge'
import { useMemo } from 'react'
import type { Lowered, LoweredNode } from './wasm'
import { type Highlight, litNodes } from './span'

/**
 * The lowered tree, and what typecheck made of it.
 *
 * Shown as the query's own shape rather than as one tree: a body is a list of
 * *statements*, not an expression, so flattening them under a single root would
 * invent a parent the engine does not have — and the statement is the unit
 * `reorder` moves, which is the next thing a reader will want to see.
 */
export function LoweredView({
  lowered,
  highlight,
  onHighlight,
}: {
  lowered: Lowered
  highlight: Highlight | null
  onHighlight: (highlight: Highlight | null) => void
}) {
  const lit = useMemo(() => litNodes(lowered, highlight, 'lowered'), [lowered, highlight])
  const byId = useMemo(
    () => new Map(lowered.nodes.map((node) => [node.id, node])),
    [lowered.nodes],
  )

  if (!lowered.schema_ok) {
    return (
      <div className="scroller">
        <p className="empty">
          no schema — a query resolves its names against one, so nothing after parsing
          can run until the schema on the left lowers
        </p>
      </div>
    )
  }

  if (lowered.head === null) {
    return (
      <div className="scroller">
        <p className="empty">nothing lowered — the query was refused before this phase</p>
      </div>
    )
  }

  const rows = (id: number, depth: number): { node: LoweredNode; depth: number }[] => {
    const node = byId.get(id)
    if (!node) return []
    return [
      { node, depth },
      ...node.children.flatMap((child) => rows(child, depth + 1)),
    ]
  }

  return (
    <div className="scroller lowered">
      <ol>
        <li className="part">
          <span className="label">head</span>
          {lowered.head_ty && <Badge variant="neutral" label={lowered.head_ty} />}
        </li>
        {rows(lowered.head, 0).map(({ node, depth }) => (
          <Row key={`h${node.id}`} node={node} depth={depth} lit={lit} onHighlight={onHighlight} />
        ))}

        {lowered.statements.map((statement, index) => (
          <li key={`s${index}`} className="contents">
            <ol>
              <li className="part">
                <span className="label">
                  {statement.kind.toLowerCase()}
                  {statement.op && <code className="op">{statement.op}</code>}
                </span>
              </li>
              {statement.nodes.flatMap((node) =>
                rows(node, 0).map(({ node, depth }) => (
                  <Row
                    key={`s${index}n${node.id}`}
                    node={node}
                    depth={depth}
                    lit={lit}
                    onHighlight={onHighlight}
                  />
                )),
              )}
            </ol>
          </li>
        ))}
      </ol>
    </div>
  )
}

function Row({
  node,
  depth,
  lit,
  onHighlight,
}: {
  node: LoweredNode
  depth: number
  lit: Set<number>
  onHighlight: (highlight: Highlight | null) => void
}) {
  return (
    <li
      className={lit.has(node.id) ? 'on' : undefined}
      style={{ paddingLeft: `${depth * 1.1 + 0.75}rem` }}
      onMouseEnter={() => onHighlight({ span: node.span, node: node.id, view: 'lowered' })}
      onMouseLeave={() => onHighlight(null)}
    >
      <span className="lead">
        <span className="kind">{node.kind}</span>
        {node.label !== null && <span className="text">{node.label}</span>}
        {node.ty !== null && <Badge variant="neutral" label={node.ty} />}
      </span>
      <span className="num">
        {node.span.start}–{node.span.end}
      </span>
    </li>
  )
}
