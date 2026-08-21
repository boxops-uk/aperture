import { useEffect } from 'react'
import type { ReactNode } from 'react'

/**
 * A drawer over the page, for what is context rather than work.
 *
 * The schema lives here: a reader edits it rarely and reads it often, and
 * giving it a column of its own costs the width the database table needs. It
 * closes on Escape and on a click outside, because a drawer that traps you is
 * worse than a panel.
 */
export function Drawer({
  open,
  title,
  onClose,
  children,
}: {
  open: boolean
  title: ReactNode
  onClose: () => void
  children: ReactNode
}) {
  useEffect(() => {
    if (!open) return
    const escape = (event: KeyboardEvent) => {
      if (event.key === 'Escape') onClose()
    }
    document.addEventListener('keydown', escape)
    return () => document.removeEventListener('keydown', escape)
  }, [open, onClose])

  if (!open) return null

  return (
    <div className="drawer-backdrop" onClick={onClose}>
      <aside
        className="drawer"
        role="dialog"
        aria-label="schema"
        onClick={(event) => event.stopPropagation()}
      >
        <header>
          {title}
          <button type="button" className="close" onClick={onClose} title="close">
            ✕
          </button>
        </header>
        <div className="drawer-body">{children}</div>
      </aside>
    </div>
  )
}
