import { useCallback, useEffect, useRef, useState } from 'react'
import type { ReactNode } from 'react'

/**
 * A resizable two-column split.
 *
 * The fraction is state rather than a CSS variable set by the browser, because
 * the two sides are different *kinds* of thing — a stack of views and a table
 * of bytes — and which one deserves the width depends entirely on what a reader
 * is doing at that moment.
 *
 * Pointer events rather than mouse events, so a trackpad drag and a touch drag
 * are the same code, and pointer capture so a fast drag that leaves the grip
 * keeps resizing rather than stopping dead.
 */
export function Split({
  fraction,
  onFraction,
  left,
  right,
}: {
  fraction: number
  onFraction: (fraction: number) => void
  left: ReactNode
  right: ReactNode
}) {
  const frame = useRef<HTMLDivElement>(null)
  const [dragging, setDragging] = useState(false)

  const move = useCallback(
    (clientX: number) => {
      const box = frame.current?.getBoundingClientRect()
      if (!box) return
      // Clamped so neither side can be dragged away entirely: a pane you cannot
      // see is one you cannot drag back.
      const next = Math.min(0.75, Math.max(0.25, (clientX - box.left) / box.width))
      onFraction(next)
    },
    [onFraction],
  )

  useEffect(() => {
    if (!dragging) return
    const onMove = (event: PointerEvent) => move(event.clientX)
    const onUp = () => setDragging(false)
    document.addEventListener('pointermove', onMove)
    document.addEventListener('pointerup', onUp)
    return () => {
      document.removeEventListener('pointermove', onMove)
      document.removeEventListener('pointerup', onUp)
    }
  }, [dragging, move])

  return (
    <div
      className={dragging ? 'split dragging' : 'split'}
      ref={frame}
      style={{ gridTemplateColumns: `${fraction}fr 1.15rem ${1 - fraction}fr` }}
    >
      <div className="side">{left}</div>
      <div
        className="grip"
        role="separator"
        aria-orientation="vertical"
        aria-label="resize"
        tabIndex={0}
        onPointerDown={() => setDragging(true)}
        style={{ touchAction: 'none' }}
        onKeyDown={(event) => {
          // Keyboard too: a split that only a mouse can move is one a keyboard
          // user cannot use at all.
          if (event.key === 'ArrowLeft') onFraction(Math.max(0.25, fraction - 0.05))
          if (event.key === 'ArrowRight') onFraction(Math.min(0.75, fraction + 0.05))
        }}
      />
      <div className="side">{right}</div>
    </div>
  )
}
