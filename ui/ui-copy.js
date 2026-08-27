/* Copy to clipboard, with the one bit of feedback that makes it trustworthy: a
 * confirmation that it actually happened. */

import { html, useState } from './vendor-preact.js'

export function Copy({ value, label = '⧉', title }) {
  const [copied, setCopied] = useState(false)

  const onClick = async (event) => {
    event.stopPropagation()
    try {
      await navigator.clipboard.writeText(value)
      setCopied(true)
      setTimeout(() => setCopied(false), 1200)
    } catch {
      /* Clipboard access is denied outside a secure context, which includes plain
       * http on anything but localhost. Saying so beats a button that does nothing. */
      setCopied('denied')
      setTimeout(() => setCopied(false), 1800)
    }
  }

  return html`
    <button
      type="button"
      class="btn icon"
      onClick=${onClick}
      title=${title ?? 'Copy'}
      aria-label=${title ?? 'Copy'}
    >
      ${copied === true ? '✓' : copied === 'denied' ? '✕' : label}
    </button>
  `
}
