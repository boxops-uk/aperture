/** The same value as plain text, for the `title` a truncated cell needs. */
export function plain(value: unknown): string {
  if (value === null || value === undefined) return ''
  if (typeof value === 'string') return value
  if (typeof value !== 'object') return String(value)
  if (Array.isArray(value)) return `[${value.map(plain).join(', ')}]`
  return `{${Object.entries(value as Record<string, unknown>)
    .map(([key, item]) => `${key}: ${plain(item)}`)
    .join(', ')}}`
}
