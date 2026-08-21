import type { Trace } from './wasm'

/**
 * **The controls that move a run** — start, a row back, a transition either
 * way, on to the next row, play, the end, and a scrub bar over the whole trace.
 *
 * Two traps, both about a hand and a timer wanting the play head at once. Any
 * navigation stops the run, or a reader who steps back watches the machine
 * step forward over them. And the buttons keep their size as they change
 * state: the one being clicked is the one under the pointer.
 */
export function Transport({
  trace,
  at,
  onSeek,
  playback,
}: {
  trace: Trace
  at: number
  onSeek: (at: number) => void
  playback: { playing: boolean; setPlaying: (playing: boolean) => void }
}) {
  const here = Math.min(at, trace.steps.length - 1)
  const end = trace.steps.length - 1
  const seek = (to: number) => {
    playback.setPlaying(false)
    onSeek(to)
  }
  const nextYield = trace.steps.findIndex((step, index) => index > here && step.event === 'yield')
  const previousYield = findLast(trace.steps, here - 1, (step) => step.event === 'yield')

  return (
    // Labels rather than transport glyphs: ⏮ and ⏩ are not in every monospace
    // font, and a control that renders as a box is worse than a word.
    <div className="transport">
      <button type="button" onClick={() => seek(0)} disabled={here === 0} title="to the start">
        |◀ start
      </button>
      <button
        type="button"
        onClick={() => seek(previousYield)}
        disabled={previousYield < 0}
        title="back to the previous row"
      >
        ◀ row
      </button>
      <button
        type="button"
        onClick={() => seek(here - 1)}
        disabled={here === 0}
        title="back one transition"
      >
        ◀
      </button>
      <button type="button" onClick={() => seek(here + 1)} disabled={here >= end} title="one transition">
        ▶
      </button>
      <button
        type="button"
        onClick={() => seek(nextYield)}
        disabled={nextYield < 0}
        title="on to the next row — step over"
      >
        row ▶
      </button>
      <button
        type="button"
        className={playback.playing ? 'play playing' : 'play'}
        onClick={() => {
          if (playback.playing) return playback.setPlaying(false)
          // Play from the end is play from the start: there is nowhere else for
          // it to mean.
          if (here >= end) onSeek(0)
          playback.setPlaying(true)
        }}
        title={playback.playing ? 'pause' : here >= end ? 'play again from the start' : 'play'}
      >
        {playback.playing ? 'pause' : 'play'}
      </button>
      <button type="button" onClick={() => seek(end)} disabled={here >= end} title="to the end">
        end ▶|
      </button>

      <input
        type="range"
        min={0}
        max={end}
        value={here}
        onChange={(event) => seek(Number(event.target.value))}
        aria-label="step"
      />
      <span className="count">
        {here + 1}/{trace.steps.length}
      </span>
    </div>
  )
}

function findLast<T>(items: T[], from: number, matches: (item: T) => boolean): number {
  for (let i = Math.min(from, items.length - 1); i >= 0; i--) if (matches(items[i])) return i
  return -1
}
