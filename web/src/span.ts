import type { Span } from './wasm'

/**
 * A span is the currency every view highlights in, so any of them can drive the
 * others: hovering a tree node lights up the source *and* the token rows inside
 * it, because all three are asking the same question of the same numbers.
 */
export function overlaps(span: Span, other: Span | null): boolean {
  if (!other) return false
  // An empty span — a rule that matched nothing, which is how an optional head
  // shows up — marks a position rather than a range, so it highlights where it
  // sits instead of never.
  if (span.start === span.end) return other.start <= span.start && span.start <= other.end
  return span.start < other.end && other.start < span.end
}
