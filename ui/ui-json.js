/* A collapsible JSON tree, with subject-state annotations.
 *
 * The annotation is the reason this is not a `<pre>` of `JSON.stringify`. hekla's
 * event payloads carry subject-scoped fields whose value in `data` may be plaintext,
 * ciphertext, or ciphertext that can never be read again, and the server reports which
 * in a sibling `subjects` object precisely so the payload keeps the shape the author
 * declared. Rendering the two side by side is what turns that into something an
 * operator can act on: an `erased` badge next to a base64 blob says "this is gone",
 * where the blob alone says nothing. */

import { html, useState } from './vendor-preact.js'
import { Badge } from './ui-badge.js'

/* Deep structures are collapsed by default below this depth. An event payload is
 * usually flat, so two levels open covers the common case without a click. */
const AUTO_OPEN_DEPTH = 2

function Scalar({ value }) {
  if (value === null) return html`<span class="null">null</span>`
  switch (typeof value) {
    case 'string':
      return html`<span class="str">"${value}"</span>`
    case 'number':
      return html`<span class="num-v">${value}</span>`
    case 'boolean':
      return html`<span class="bool">${String(value)}</span>`
    default:
      return html`<span class="null">${String(value)}</span>`
  }
}

function Node({ name, value, depth, subjects }) {
  const container = value !== null && typeof value === 'object'
  const [open, setOpen] = useState(depth < AUTO_OPEN_DEPTH)
  const subject = name && subjects?.[name]

  if (!container) {
    return html`
      <div class="line">
        <span class="toggle"></span>
        ${name !== undefined && html`<span class="key">${name}:</span>`}
        <${Scalar} value=${value} />
        ${subject &&
        html`
          <${Badge}
            kind="subject"
            value=${subject.state}
            title=${`scoped to ${subject.subject}=${subject.subject_value ?? '?'}`}
          />
        `}
      </div>
    `
  }

  const entries = Array.isArray(value)
    ? value.map((item, index) => [String(index), item])
    : Object.entries(value)
  const open_ = Array.isArray(value) ? '[' : '{'
  const close = Array.isArray(value) ? ']' : '}'

  return html`
    <div>
      <div class="line">
        <button
          type="button"
          class="toggle"
          onClick=${() => setOpen(!open)}
          aria-expanded=${open}
          aria-label=${open ? 'Collapse' : 'Expand'}
        >
          ${open ? '▾' : '▸'}
        </button>
        ${name !== undefined && html`<span class="key">${name}:</span>`}
        <span class="faint">
          ${open_}${open ? '' : ` ${entries.length} `}${open ? '' : close}
        </span>
      </div>
      ${open &&
      html`
        <div class="indent">
          ${entries.map(
            ([key, item]) => html`
              <${Node}
                key=${key}
                name=${key}
                value=${item}
                depth=${depth + 1}
                subjects=${depth === 0 ? subjects : undefined}
              />
            `,
          )}
        </div>
        <div class="line"><span class="toggle"></span><span class="faint">${close}</span></div>
      `}
    </div>
  `
}

/**
 * `subjects` is the event's sidecar, keyed by field name. Only applied at the top
 * level of `data`, which is where a subject field lives.
 */
export function JsonTree({ value, subjects }) {
  return html`<div class="json"><${Node} value=${value} depth=${0} subjects=${subjects} /></div>`
}

/** Flat pre-formatted JSON, for a recorded result whose shape is not known. */
export function JsonBlock({ value }) {
  const text =
    typeof value === 'string' ? value : JSON.stringify(value, null, 2)
  return html`<pre class="json" style=${{ margin: 0, whiteSpace: 'pre-wrap' }}>${text}</pre>`
}
