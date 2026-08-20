import type { Span, Tree } from './wasm'

/**
 * What the cursor is on: a range of the source, and — when the cursor is on a
 * tree node — which node.
 *
 * The node is carried because a span cannot stand in for one. A chain of
 * single-child rules (`ImplicitBindStmt → Pattern → Sum → Fact → FactPattern`)
 * all cover exactly the same bytes, so *any* test over spans lights the whole
 * chain when the cursor is on its last link. Which node the cursor is on is
 * knowledge only the tree has, and only the tree needs.
 */
export type Highlight = { span: Span; node: number | null }

/**
 * Whether `span` is inside the highlight — how the source and the token table
 * decide, since neither has a node to compare against.
 *
 * **Containment, not overlap.** A node's span covers its children's, so an
 * overlap test would light every ancestor of whatever the cursor is on, up to
 * `Root`. That is true and useless: the path upwards is already obvious from
 * the indentation, and what a reader wants to see is what this node is *made
 * of*.
 */
export function within(span: Span, highlight: Highlight | null): boolean {
  if (!highlight) return false
  const { start, end } = highlight.span
  // An empty span — a rule that matched nothing, which is how a missing operand
  // shows up — marks a position rather than a range, so it counts as inside
  // wherever it sits.
  if (span.start === span.end) return start <= span.start && span.start <= end
  return start <= span.start && span.end <= end
}

/**
 * The nodes a tree row highlight covers: the one under the cursor and its
 * descendants, or — when the cursor is in the source — whatever sits inside the
 * bytes it is over.
 */
export function litNodes(tree: Tree, highlight: Highlight | null): Set<number> {
  const lit = new Set<number>()
  if (!highlight) return lit

  if (highlight.node !== null) {
    const walk = (id: number) => {
      lit.add(id)
      for (const child of tree.nodes[id]?.children ?? []) walk(child)
    }
    walk(highlight.node)
    return lit
  }

  for (const node of tree.nodes) {
    if (within(node.span, highlight)) lit.add(node.id)
  }
  return lit
}
