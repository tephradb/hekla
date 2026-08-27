/* The command palette.
 *
 * The console's addresses are mostly numbers and uuids: a log position, a correlation
 * id. Navigating to one by clicking through a paged list is absurd when you already
 * have it in your clipboard from a log line or a command response, so the palette
 * reads what you paste and offers the destination directly. */

import { html, useEffect, useMemo, useRef, useState } from './vendor-preact.js'
import { NAV, go } from './router.js'

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i

function suggestions(query, status) {
  const trimmed = query.trim()
  const out = []

  if (/^\d+$/.test(trimmed)) {
    out.push({
      group: 'EVENT',
      label: `go to position ${trimmed}`,
      href: `/admin/events/${trimmed}`,
    })
  }
  if (UUID.test(trimmed)) {
    out.push({
      group: 'TRACE',
      label: trimmed,
      hint: 'follow this correlation',
      href: `/admin/traces/${trimmed}`,
    })
  }

  const matches = (name) => !trimmed || name.toLowerCase().includes(trimmed.toLowerCase())

  for (const effect of status?.effects ?? []) {
    if (matches(effect.name)) {
      out.push({
        group: 'EFFECT',
        label: effect.name,
        hint: effect.state,
        href: `/admin/effects/${encodeURIComponent(effect.name)}`,
      })
    }
  }
  for (const projector of status?.projectors ?? []) {
    if (matches(projector.name)) {
      out.push({
        group: 'PROJECTOR',
        label: projector.name,
        hint: projector.readiness,
        href: `/admin/projectors/${encodeURIComponent(projector.name)}`,
      })
    }
  }
  for (const route of NAV) {
    if (matches(route.title)) {
      out.push({ group: 'GO TO', label: route.title, href: route.path })
    }
  }
  return out.slice(0, 12)
}

export function Palette({ status }) {
  const [open, setOpen] = useState(false)
  const [query, setQuery] = useState('')
  const [active, setActive] = useState(0)
  const field = useRef(null)

  useEffect(() => {
    const onKey = (event) => {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'k') {
        event.preventDefault()
        setOpen((was) => !was)
        setQuery('')
        setActive(0)
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [])

  useEffect(() => {
    if (open) field.current?.focus()
  }, [open])

  const items = useMemo(() => (open ? suggestions(query, status) : []), [open, query, status])

  if (!open) return null

  const choose = (item) => {
    setOpen(false)
    go(item.href)
  }

  const onKeyDown = (event) => {
    if (event.key === 'Escape') setOpen(false)
    else if (event.key === 'ArrowDown') {
      event.preventDefault()
      setActive((index) => Math.min(index + 1, items.length - 1))
    } else if (event.key === 'ArrowUp') {
      event.preventDefault()
      setActive((index) => Math.max(index - 1, 0))
    } else if (event.key === 'Enter' && items[active]) {
      event.preventDefault()
      choose(items[active])
    }
  }

  return html`
    <div class="modal-scrim" onClick=${() => setOpen(false)}>
      <div
        class="palette"
        role="dialog"
        aria-modal="true"
        aria-label="Jump to"
        onClick=${(event) => event.stopPropagation()}
      >
        <input
          ref=${field}
          value=${query}
          placeholder="Position, correlation id, effect, projector…"
          onInput=${(event) => {
            setQuery(event.target.value)
            setActive(0)
          }}
          onKeyDown=${onKeyDown}
          aria-label="Jump to"
          spellcheck="false"
          autocomplete="off"
        />
        <div class="palette-list">
          ${items.length === 0 &&
          html`<div class="palette-empty tiny">Nothing matches “${query}”.</div>`}
          ${items.map(
            (item, index) => html`
              <button
                type="button"
                class=${`palette-item${index === active ? ' active' : ''}`}
                onMouseEnter=${() => setActive(index)}
                onClick=${() => choose(item)}
              >
                <span class="palette-group">${item.group}</span>
                <span class="palette-label mono">${item.label}</span>
                ${item.hint && html`<span class="tiny faint">${item.hint}</span>`}
              </button>
            `,
          )}
        </div>
      </div>
    </div>
  `
}
