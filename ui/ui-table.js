/* The keyboard-navigable data table.
 *
 * Every list in this console is the same shape: a header, rows, a cursor you can walk
 * with j/k, Enter to open, and a pair of page buttons driven by an opaque-free
 * cursor. Writing that once is what makes it affordable to have it everywhere, which
 * is the whole argument for a component model here.
 *
 * The cursor is a row index rather than a row identity: rows are replaced wholesale on
 * every page and every poll, so an identity would have to be threaded through by each
 * caller for no benefit. */

import { html, useEffect, useRef, useState } from './vendor-preact.js'

/**
 * `columns` is `{ key, header, width, align, render(row) }`.
 * `onOpen(row, index)` fires on Enter and on click.
 */
export function DataTable({
  columns,
  rows,
  onOpen,
  selected,
  empty,
  keyboard = true,
  label,
}) {
  const [cursor, setCursor] = useState(-1)
  const body = useRef(null)

  useEffect(() => {
    if (!keyboard) return
    const onKey = (event) => {
      /* Never steal a key from something the user is typing into, and never from a
       * modified chord the browser or the palette owns. `SELECT` counts: j and k are
       * how you walk a native dropdown, and Chips' own handler already treats it that
       * way, so leaving it out here had the two disagreeing inside one view. */
      const active = document.activeElement
      const tag = active?.tagName
      if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || active?.isContentEditable) return
      if (event.metaKey || event.ctrlKey) return
      if (event.key === 'j' || event.key === 'ArrowDown') {
        event.preventDefault()
        setCursor((current) => Math.min(current + 1, rows.length - 1))
      } else if (event.key === 'k' || event.key === 'ArrowUp') {
        event.preventDefault()
        setCursor((current) => Math.max(current - 1, 0))
      } else if (event.key === 'Enter' && cursor >= 0 && rows[cursor]) {
        /* Enter belongs to whatever is focused when that thing acts on it. Taking it
         * here cancelled the click on every button in the view (pager, copy, theme,
         * refresh) from the moment a row cursor existed, and opened a row instead. */
        if (tag === 'BUTTON' || tag === 'SUMMARY' || (tag === 'A' && active.hasAttribute('href'))) {
          return
        }
        event.preventDefault()
        onOpen?.(rows[cursor], cursor)
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [rows, cursor, onOpen, keyboard])

  /* Follow the cursor when the keyboard moves it off screen, but only then: calling
   * this on every render would fight the user's own scrolling. */
  useEffect(() => {
    if (cursor < 0) return
    const row = body.current?.children[cursor]
    row?.scrollIntoView({ block: 'nearest' })
  }, [cursor])

  if (rows.length === 0 && empty) return empty

  return html`
    <div class="table-wrap">
      <table class="data" aria-label=${label}>
        <thead>
          <tr>
            ${columns.map(
              (column) => html`
                <th
                  scope="col"
                  class=${column.align === 'right' ? 'num' : undefined}
                  style=${column.width ? { width: column.width } : undefined}
                >
                  ${column.header}
                </th>
              `,
            )}
          </tr>
        </thead>
        <tbody ref=${body}>
          ${rows.map(
            (row, index) => html`
              <tr
                key=${index}
                class=${[
                  onOpen ? 'clickable' : '',
                  index === cursor ? 'cursor' : '',
                  selected?.(row) ? 'selected' : '',
                ]
                  .filter(Boolean)
                  .join(' ')}
                onClick=${() => {
                  setCursor(index)
                  onOpen?.(row, index)
                }}
              >
                ${columns.map(
                  (column) => html`
                    <td class=${column.align === 'right' ? 'num' : undefined}>
                      ${column.render(row)}
                    </td>
                  `,
                )}
              </tr>
            `,
          )}
        </tbody>
      </table>
    </div>
  `
}

/**
 * Cursor paging.
 *
 * `onOlder` is enabled only when the server handed back a cursor. That is the whole
 * point of the API's over-fetch: a full page is not evidence that there is more, so
 * the button reflects what the server said rather than what the row count implies.
 */
export function Pager({ onNewer, onOlder, cursor, canGoBack, children }) {
  return html`
    <div class="row" style=${{ justifyContent: 'flex-end', padding: '10px 14px', gap: '12px' }}>
      ${children}
      <button type="button" class="btn" onClick=${onNewer} disabled=${!canGoBack}>← newer</button>
      <button type="button" class="btn" onClick=${onOlder} disabled=${cursor === null || cursor === undefined}>
        older →
      </button>
      ${cursor !== null &&
      cursor !== undefined &&
      html`<span class="tiny faint mono">at ${cursor}</span>`}
    </div>
  `
}
