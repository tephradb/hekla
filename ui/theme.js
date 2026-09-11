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
  tint()
}

/* A phone's address bar takes its colour from this meta tag and from nothing else, so
 * without it the console sits under a white strip that no palette here chose. The
 * value is read back off `--bg` rather than written out as a literal, which is what
 * keeps three modes and two palettes from needing a fourth copy of the same hex. */
function tint() {
  const color = getComputedStyle(document.documentElement).getPropertyValue('--bg').trim()
  if (!color) return
  let meta = document.head.querySelector('meta[name="theme-color"]')
  if (!meta) {
    meta = document.createElement('meta')
    meta.name = 'theme-color'
    document.head.append(meta)
  }
  meta.content = color
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

  /* Following the system means following it while the page is open: the palette
   * swaps under `mode === 'system'` with no render to hang the repaint off. */
  useEffect(() => {
    const query = window.matchMedia('(prefers-color-scheme: dark)')
    query.addEventListener('change', tint)
    return () => query.removeEventListener('change', tint)
  }, [])

  const cycle = () => setMode((current) => MODES[(MODES.indexOf(current) + 1) % MODES.length])
  return [mode, cycle]
}
