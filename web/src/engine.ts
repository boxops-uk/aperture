/**
 * **The engine, shared by every page that wants one.**
 *
 * The module is 360-odd KiB and the book is mostly prose, so nothing downloads
 * it to colour a code block: a demo *demands* the engine, and everything else
 * merely *observes* whether one has turned up. A page of prose costs nothing; a
 * page with a demo pays once and every block on it is painted by the real lexer
 * from then on.
 */
import { useEffect, useSyncExternalStore } from 'react'
import { load, type Engine } from './wasm'

export type EngineState = { engine: Engine | null; failure: string | null }

let state: EngineState = { engine: null, failure: null }
let started = false
const listeners = new Set<() => void>()

function announce(next: EngineState): void {
  state = next
  for (const listener of listeners) listener()
}

function start(): void {
  if (started) return
  started = true
  load().then(
    (engine) => announce({ engine, failure: null }),
    (error: unknown) => announce({ engine: null, failure: String(error) }),
  )
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener)
  return () => listeners.delete(listener)
}

/**
 * The engine if it is here. Pass `demand` to be the reason it arrives — a
 * component that only paints itself better should not be that reason.
 */
export function useEngine(demand = false): EngineState {
  const current = useSyncExternalStore(
    subscribe,
    () => state,
    () => state,
  )
  useEffect(() => {
    if (demand) start()
  }, [demand])
  return current
}
