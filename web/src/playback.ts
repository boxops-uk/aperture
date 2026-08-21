import { useEffect, useState } from 'react'

/**
 * Play: advance one transition at a time until the run ends.
 *
 * A timer rather than an animation frame, because the point is to be *slow
 * enough to read* — a machine transition every fifth of a second, which is
 * about as fast as a person can follow a register changing.
 *
 * Playing is **derived**, not stored: at the last step there is nowhere to go,
 * so the transport says "play" again without anyone having to switch it back.
 * A stored flag would have to be cleared from inside the effect that noticed,
 * and a reader would pay for it with a "pause" button that takes two clicks —
 * one to undo the state nothing had told them about.
 */
export function usePlayback(steps: number, at: number, onSeek: (at: number) => void) {
  const [wanted, setWanted] = useState(false)
  const playing = wanted && at < steps - 1

  useEffect(() => {
    if (!playing) return
    const timer = setTimeout(() => onSeek(at + 1), 220)
    return () => clearTimeout(timer)
  }, [playing, at, onSeek])

  return { playing, setPlaying: setWanted }
}
