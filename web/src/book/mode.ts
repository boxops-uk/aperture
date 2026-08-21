import { useCallback, useState } from 'react'

/**
 * Light or dark, and which one the reader asked for.
 *
 * Three states, not two: the choice is *no choice* until somebody makes one,
 * and then it sticks. `system` is the default and is what `Theme` reads to
 * follow the operating system, so an untouched site changes with the machine
 * around it.
 */
export type Mode = 'system' | 'light' | 'dark'

const KEY = 'fjord-theme'

function stored(): Mode {
  try {
    const value = localStorage.getItem(KEY)
    return value === 'dark' || value === 'light' ? value : 'system'
  } catch {
    // A browser with storage denied still gets a working toggle for the session.
    return 'system'
  }
}

export function useMode(): { mode: Mode; toggle: () => void } {
  const [mode, setMode] = useState<Mode>(stored)

  const toggle = useCallback(() => {
    setMode((current) => {
      const dark =
        current === 'dark' ||
        (current === 'system' && window.matchMedia('(prefers-color-scheme: dark)').matches)
      const next: Mode = dark ? 'light' : 'dark'
      try {
        localStorage.setItem(KEY, next)
      } catch {
        // Nothing to do: the toggle worked, it just will not be remembered.
      }
      return next
    })
  }, [])

  return { mode, toggle }
}
