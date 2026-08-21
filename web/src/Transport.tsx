import { Toolbar } from '@astryxdesign/core/Toolbar'
import { Button } from '@astryxdesign/core/Button'
import { ButtonGroup } from '@astryxdesign/core/ButtonGroup'
import { Slider } from '@astryxdesign/core/Slider'
import { Text } from '@astryxdesign/core/Text'
import { HStack } from '@astryxdesign/core/Stack'
import type { Trace } from './wasm'

/**
 * **The controls that move a run** — start, a row back, a transition either
 * way, on to the next row, play, the end, and a scrub bar over the whole trace.
 *
 * Two traps, both about a hand and a timer wanting the play head at once. Any
 * navigation stops the run, or a reader who steps back watches the machine step
 * forward over them. And the buttons keep their size as they change state: the
 * one being clicked is the one under the pointer.
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
    <Toolbar
      className="transport"
      label="Run"
      size="sm"
      variant="muted"
      startContent={
        // Labels rather than transport glyphs: ⏮ and ⏩ are not in every
        // monospace font, and a control that renders as a box is worse than a
        // word.
        <ButtonGroup label="Move the run">
          <Button variant="secondary" label="|◀ start" tooltip="to the start" onClick={() => seek(0)} isDisabled={here === 0} />
          <Button
            variant="secondary"
            label="◀ row"
            tooltip="back to the previous row"
            onClick={() => seek(previousYield)}
            isDisabled={previousYield < 0}
          />
          <Button variant="secondary" label="◀" tooltip="back one transition" onClick={() => seek(here - 1)} isDisabled={here === 0} />
          <Button variant="secondary" label="▶" tooltip="one transition" onClick={() => seek(here + 1)} isDisabled={here >= end} />
          <Button
            variant="secondary"
            label="row ▶"
            tooltip="on to the next row — step over"
            onClick={() => seek(nextYield)}
            isDisabled={nextYield < 0}
          />
          <Button
            variant={playback.playing ? 'primary' : 'secondary'}
            label={playback.playing ? 'pause' : 'play'}
            tooltip={
              playback.playing ? 'pause' : here >= end ? 'play again from the start' : 'play'
            }
            onClick={() => {
              if (playback.playing) return playback.setPlaying(false)
              // Play from the end is play from the start: there is nowhere else
              // for it to mean.
              if (here >= end) onSeek(0)
              playback.setPlaying(true)
            }}
          />
          <Button variant="secondary" label="end ▶|" tooltip="to the end" onClick={() => seek(end)} isDisabled={here >= end} />
        </ButtonGroup>
      }
      endContent={
        <HStack gap={3} align="center">
          <Slider
            label="step"
            isLabelHidden
            width={200}
            value={here}
            min={0}
            max={Math.max(end, 1)}
            onChange={((value: number) => seek(value)) as (value: number) => void}
            valueDisplay="none"
          />
          <Text className="count" type="supporting" hasTabularNumbers>
            {here + 1}/{trace.steps.length}
          </Text>
        </HStack>
      }
    />
  )
}

function findLast<T>(items: T[], from: number, matches: (item: T) => boolean): number {
  for (let i = Math.min(from, items.length - 1); i >= 0; i--) if (matches(items[i])) return i
  return -1
}
