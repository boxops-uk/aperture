import { useMemo, useState } from 'react'
import type { Tree, TreeNode } from './wasm'
import { type Highlight, litNodes } from './span'
import { display } from './display'

/**
 * The parse tree, indented.
 *
 * The arena arrives parent-before-children, so a walk from the root is the
 * reading order — no sorting, no second pass. What a page adds is one
 * presentational decision: whitespace leaves are hidden by default, because a
 * tree that shows them is mostly whitespace. They are in the view either way;
 * the grammar's `skip Whitespace` keeps trivia out of what the parser matches
 * on, not out of the tree.
 */
export function TreeView({
  tree,
  highlight,
  onHighlight,
}: {
  tree: Tree
  highlight: Highlight | null
  onHighlight: (highlight: Highlight | null) => void
}) {
  const [trivia, setTrivia] = useState(false)
  const lit = useMemo(() => litNodes(tree, highlight, 'tree'), [tree, highlight])

  if (tree.root === null) {
    return (
      <div className="scroller">
        <p className="empty">
          {tree.diagnostics.length === 0
            ? 'nothing to parse yet'
            : 'no tree — the parse was refused rather than recovered'}
        </p>
      </div>
    )
  }

  const rows: { node: TreeNode; depth: number }[] = []
  const walk = (id: number, depth: number) => {
    const node = tree.nodes[id]
    if (!trivia && node.token && node.label !== null && node.label.trim() === '') return
    rows.push({ node, depth })
    for (const child of node.children) walk(child, depth + 1)
  }
  walk(tree.root, 0)

  return (
    <div className="scroller tree">
      <label className="trivia">
        <input type="checkbox" checked={trivia} onChange={(e) => setTrivia(e.target.checked)} />
        show whitespace leaves
      </label>
      <ol>
        {rows.map(({ node, depth }) => (
          <li
            key={node.id}
            className={lit.has(node.id) ? 'on' : undefined}
            style={{ paddingLeft: `${depth * 1.1 + 0.75}rem` }}
            onMouseEnter={() => onHighlight({ span: node.span, node: node.id, view: 'tree' })}
            onMouseLeave={() => onHighlight(null)}
          >
            <span className="lead">
              <span className={node.token ? 'kind leaf' : 'kind'}>{node.kind}</span>
              {node.label !== null && <span className="text">{display(node.label)}</span>}
            </span>
            <span className="num">
              {node.span.start}–{node.span.end}
            </span>
          </li>
        ))}
      </ol>
    </div>
  )
}
