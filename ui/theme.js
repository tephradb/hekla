/* Theme. Three states, not two: an explicit dark, an explicit light, and following
 * the system, which is the default. The explicit choices set `data-theme` on the root
 * so they win over the media query in both directions. */

import { useEffect, useState } from './vendor-preact.js'

/* Also spelled out inline in index.html, which applies the stored choice before the
 * first paint. `the_shell_applies_the_stored_theme_before_the_first_paint` keeps the
 * two spellings in step. */
const KEY = 'hekla.theme'
export const MODES = ['system', 'dark', 'light']

function stored() {
  try {
    const value = localStorage.getItem(KEY)
    return MODES.includes(value) ? value : 'system'
  } catch {
    /* A private window, cleared site data, or a browser set to block storage all
     * throw here rather than returning null. Following the system is the right
     * fallback, so this is not an error worth surfacing. */
    return 'system'
  }
}

function apply(mode) {
  if (mode === 'system') delete document.documentElement.dataset.theme
  else document.documentElement.dataset.theme = mode
}

/** The theme mode and a cycler through the three states. */
export function useTheme() {
  const [mode, setMode] = useState(stored)

  useEffect(() => {
    apply(mode)
    try {
      localStorage.setItem(KEY, mode)
    } catch {
      /* The choice still applies to this page; it just will not survive a reload. */
    }
  }, [mode])

  const cycle = () => setMode((current) => MODES[(MODES.indexOf(current) + 1) % MODES.length])
  return [mode, cycle]
}
