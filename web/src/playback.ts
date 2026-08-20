import { useEffect, useState } from 'react'

/**
 * Play: advance one transition at a time until the run ends.
 *
 * A timer rather than an animation frame, because the point is to be *slow
 * enough to read* — a machine transition every fifth of a second, which is
 * about as fast as a person can follow a register changing.
 *
 * Stopping at the end is the timer's business rather than an effect's: the
 * effect schedules a tick only while there is somewhere left to go, so the run
 * simply stops being scheduled.
 */
export function usePlayback(steps: number, at: number, onSeek: (at: number) => void) {
  const [playing, setPlaying] = useState(false)
  const more = at < steps - 1

  useEffect(() => {
    if (!playing || !more) return
    const timer = setTimeout(() => onSeek(at + 1), 220)
    return () => clearTimeout(timer)
  }, [playing, more, at, onSeek])

  return { playing: playing && more, setPlaying }
}
