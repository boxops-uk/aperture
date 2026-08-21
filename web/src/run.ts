import type { RegisterView, Scanning, Trace } from './wasm'

/**
 * The machine's state at step `at`, folded from the changes each step carries.
 *
 * One fold, read by both panels: the transport and registers on one side, the
 * database table on the other, showing the same moment. Two folds would be two
 * chances to disagree about which row the machine is standing on.
 */
export type Moment = {
  registers: (RegisterView & { written: boolean })[]
  rows: unknown[]
  /** The range the *current* level is walking, if it is scanning one. */
  scanning: Scanning | null
  /** The keys the registers hold — the rows the machine is standing on. */
  held: Set<string>
  /** The row this step read and dropped, if it did. */
  dropped: string | null
  /** Every row dropped so far, so the table can grey them. */
  droppedSoFar: Set<string>
}

export function fold(trace: Trace, at: number): Moment {
  const registers = new Map<number, RegisterView & { written: boolean }>()
  const rows: unknown[] = []
  const droppedSoFar = new Set<string>()
  const scanning = new Map<number, Scanning>()
  let dropped: string | null = null
  let depth = 0

  for (let i = 0; i <= at && i < trace.steps.length; i++) {
    const step = trace.steps[i]
    for (const [, held] of registers) held.written = false

    for (const register of step.registers) {
      if (register.kind === 'empty') registers.delete(register.address)
      else registers.set(register.address, { ...register, written: i === at })
    }

    if (step.event === 'yield') rows.push(step.row)
    if (step.scanning) scanning.set(step.scanning.step, step.scanning)
    if (step.rejected?.row.key) {
      droppedSoFar.add(step.rejected.row.key)
      dropped = i === at ? step.rejected.row.key : null
    } else if (i === at) {
      dropped = null
    }
    depth = step.depth
  }

  const held = new Set(
    [...registers.values()].map((register) => register.key).filter((key): key is string => !!key),
  )

  return {
    registers: [...registers.values()].sort((a, b) => a.address - b.address),
    rows,
    // The range belonging to the level the machine is in — an inner level's
    // scan is the one being walked, and an outer one's has already produced the
    // row that opened it.
    scanning: scanning.get(depth) ?? scanning.get(depth - 1) ?? null,
    held,
    dropped,
    droppedSoFar,
  }
}

/** Whether a stored key falls in `[lo, hi)`, compared as the store compares it. */
export function inRange(key: string, scanning: Scanning | null): boolean {
  if (!scanning || scanning.fetch) return false
  // Hex, unseparated and fixed-width per byte, so lexicographic order over the
  // strings *is* lexicographic order over the bytes — which is the order the
  // store keeps and the order a range means.
  return key >= scanning.lo && (scanning.hi === null || key < scanning.hi)
}
