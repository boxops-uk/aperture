import { useCallback, useEffect, useRef, useState } from 'react'

/** The narrowest a column may be dragged, as a fraction of the pane. */
const FLOOR = 0.06

/**
 * Column widths as **fractions of the pane**, dragged between neighbours.
 *
 * Resizing divides the space rather than growing the table: the last column's
 * right edge stays pinned to the pane's, so a reader never loses a column off
 * the side and there is no horizontal scrollbar to fight. Taking width for one
 * column therefore means giving it up from the next, which is also the honest
 * model — the pane is what there is.
 *
 * Fractions rather than pixels so a resize of the pane needs no arithmetic at
 * all: the same fractions of a different width are still the same layout, and a
 * reader's preference survives dragging the split.
 */
export function useColumns(initial: number[]) {
  const [fractions, setFractions] = useState(initial)
  const [room, setRoom] = useState(0)
  const held = useRef<{ index: number; from: number; pair: [number, number] } | null>(null)

  const measure = useCallback((box: HTMLElement | null) => {
    if (!box) return
    setRoom(box.clientWidth)
  }, [])

  const start = useCallback(
    (index: number, clientX: number) => {
      held.current = {
        index,
        from: clientX,
        pair: [fractions[index], fractions[index + 1]],
      }
    },
    [fractions],
  )

  useEffect(() => {
    const move = (event: PointerEvent) => {
      const drag = held.current
      if (!drag || room === 0) return

      const [left, right] = drag.pair
      const both = left + right
      // Clamped against *both* floors: the pair keeps its total, so widening
      // one to the limit is narrowing the other to it.
      const wanted = left + (event.clientX - drag.from) / room
      const next = Math.min(both - FLOOR, Math.max(FLOOR, wanted))

      setFractions((current) => {
        const updated = [...current]
        updated[drag.index] = next
        updated[drag.index + 1] = both - next
        return updated
      })
    }
    const up = () => {
      held.current = null
    }

    document.addEventListener('pointermove', move)
    document.addEventListener('pointerup', up)
    return () => {
      document.removeEventListener('pointermove', move)
      document.removeEventListener('pointerup', up)
    }
  }, [room])

  return { fractions, start, measure, room }
}
