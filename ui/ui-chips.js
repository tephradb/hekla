/* The filter builder for the events view.
 *
 * Each chip lowers to exactly one query parameter, and the parameters lower to exactly
 * one tephra query item: types OR together, tags AND together. Nothing is reinterpreted
 * on the way through, so what the chips say is what the store is asked, and a filter
 * that returns nothing means the log holds nothing matching rather than that the
 * console built the wrong query. */

import { html, useEffect, useRef, useState } from './vendor-preact.js'

export function Chips({ types, tags, onChange }) {
  const [draft, setDraft] = useState('')
  const [kind, setKind] = useState('type')
  const input = useRef(null)

  useEffect(() => {
    const onKey = (event) => {
      const tag = document.activeElement?.tagName
      if (event.key !== '/' || event.metaKey || event.ctrlKey) return
      if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') return
      // Focus rather than type: without this the `/` lands in the field as the first
      // character of the filter, which is never what was meant.
      event.preventDefault()
      input.current?.focus()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [])

  const add = () => {
    const value = draft.trim()
    if (!value) return
    if (kind === 'type') {
      if (!types.includes(value)) onChange({ types: [...types, value], tags })
    } else if (!tags.includes(value)) {
      onChange({ types, tags: [...tags, value] })
    }
    setDraft('')
  }

  const remove = (which, value) => {
    if (which === 'type') onChange({ types: types.filter((t) => t !== value), tags })
    else onChange({ types, tags: tags.filter((t) => t !== value) })
  }

  /* The row is a div, not a label. A label forwards its activation to the first
   * labelable descendant, which here is the select, so clicking "add" would fire the
   * handler and pop the type/tag dropdown open at the same time. Each control carries
   * its own aria-label instead. */
  return html`
    <div class="filters">
      <div class="filter-input">
        <select
          value=${kind}
          onChange=${(event) => setKind(event.target.value)}
          aria-label="Filter kind"
        >
          <option value="type">type</option>
          <option value="tag">tag</option>
        </select>
        <input
          ref=${input}
          value=${draft}
          placeholder=${kind === 'type' ? 'user.registered' : 'user_id:c-42'}
          onInput=${(event) => setDraft(event.target.value)}
          onKeyDown=${(event) => {
            if (event.key === 'Enter') {
              event.preventDefault()
              add()
            }
          }}
          aria-label=${`Add a ${kind} filter`}
        />
        <button type="button" class="btn" onClick=${add} disabled=${!draft.trim()}>add</button>
      </div>

      ${types.map(
        (value) => html`
          <span class="chip">
            <span class="chip-kind">type</span>
            <code>${value}</code>
            <button
              type="button"
              onClick=${() => remove('type', value)}
              aria-label=${`Remove ${value}`}
            >
              ✕
            </button>
          </span>
        `,
      )}
      ${tags.map(
        (value) => html`
          <span class="chip">
            <span class="chip-kind">tag</span>
            <code>${value}</code>
            <button
              type="button"
              onClick=${() => remove('tag', value)}
              aria-label=${`Remove ${value}`}
            >
              ✕
            </button>
          </span>
        `,
      )}
      ${(types.length > 0 || tags.length > 0) &&
      html`
        <button type="button" class="btn" onClick=${() => onChange({ types: [], tags: [] })}>
          clear
        </button>
        <span class="tiny faint">
          ${types.length > 1 ? 'types match any' : ''}
          ${types.length > 1 && tags.length > 1 ? ', ' : ''}
          ${tags.length > 1 ? 'tags must all match' : ''}
        </span>
      `}
    </div>
  `
}
