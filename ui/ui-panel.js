/* The detail panel: a column beside the list, not an overlay over it.
 *
 * The events view exists to be walked. You move the row cursor with j/k and read each
 * payload as you go, and an overlay fights that in two ways: it hides the list you are
 * navigating, and it has to be dismissed before the next row can be opened. Sitting
 * beside the table instead means selecting another event simply swaps the contents,
 * which is one interaction rather than three.
 *
 * It is deliberately not modal. There is no scrim and no focus trap, because trapping
 * focus here would trap the user out of the very table the panel is describing. `Esc`
 * still closes it, from anywhere, and so does the button. */

import { html, useEffect, useRef } from './vendor-preact.js'

export function DetailPanel({ title, subtitle, onClose, children, actions }) {
  const panel = useRef(null)

  useEffect(() => {
    const onKey = (event) => {
      const tag = document.activeElement?.tagName
      if (event.key !== 'Escape') return
      if (tag === 'INPUT' || tag === 'TEXTAREA') return
      event.preventDefault()
      onClose()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [onClose])

  return html`
    <section class="card panel" aria-label=${title} ref=${panel}>
      <header class="panel-head">
        <div class="panel-title">${title}</div>
        ${subtitle && html`<code class="note">${subtitle}</code>`}
        <div style=${{ flex: 1 }}></div>
        ${actions}
        <button type="button" class="btn icon" onClick=${onClose} aria-label="Close (Esc)" title="Close (Esc)">
          ✕
        </button>
      </header>
      <div class="panel-body">${children}</div>
    </section>
  `
}
