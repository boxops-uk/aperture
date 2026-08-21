/**
 * Light or dark, and which one the reader asked for.
 *
 * Three states, not two: the choice is *no choice* until somebody makes one,
 * and then it sticks. `data-theme` on the root element is what the stylesheets
 * read, and its absence is what lets `prefers-color-scheme` decide.
 */
import { useCallback, useSyncExternalStore } from 'react'

const KEY = 'fjord-theme'
const listeners = new Set<() => void>()

function stored(): 'light' | 'dark' | null {
  try {
    const value = localStorage.getItem(KEY)
    return value === 'dark' || value === 'light' ? value : null
  } catch {
    // A browser with storage denied still gets a working toggle for the session.
    return null
  }
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener)
  return () => listeners.delete(listener)
}

function snapshot(): string {
  return document.documentElement.getAttribute('data-theme') ?? 'system'
}

/** Apply the stored choice before the first paint, if there is one. */
export function restoreTheme(): void {
  const choice = stored()
  if (choice) document.documentElement.setAttribute('data-theme', choice)
}

export function useTheme(): { theme: string; toggle: () => void } {
  const theme = useSyncExternalStore(subscribe, snapshot, () => 'system')

  const toggle = useCallback(() => {
    const dark =
      document.documentElement.getAttribute('data-theme') === 'dark' ||
      (!document.documentElement.getAttribute('data-theme') &&
        window.matchMedia('(prefers-color-scheme: dark)').matches)
    const next = dark ? 'light' : 'dark'
    document.documentElement.setAttribute('data-theme', next)
    try {
      localStorage.setItem(KEY, next)
    } catch {
      // Nothing to do: the toggle still worked, it just will not be remembered.
    }
    for (const listener of listeners) listener()
  }, [])

  return { theme, toggle }
}
