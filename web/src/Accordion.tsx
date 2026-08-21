import type { ReactNode } from 'react'

/**
 * One section of the left-hand stack.
 *
 * An accordion rather than tabs, because the views are meant to be read
 * *against each other*: the plan beside the run it is executing, the lowered
 * tree beside the plan it produced. Tabs make that a memory exercise.
 */
export function Section({
  name,
  count,
  open,
  onToggle,
  children,
}: {
  name: string
  count?: ReactNode
  open: boolean
  onToggle: () => void
  children: ReactNode
}) {
  return (
    <section className={open ? 'section open' : 'section'}>
      <button type="button" className="section-head" onClick={onToggle} aria-expanded={open}>
        <span className={open ? 'arrow open' : 'arrow'} aria-hidden="true">
          ▸
        </span>
        <span className="what">{name}</span>
        {count !== undefined && <span className="count">{count}</span>}
      </button>
      {open && <div className="section-body">{children}</div>}
    </section>
  )
}
