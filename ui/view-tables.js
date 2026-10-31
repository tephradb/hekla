/* Tables: the derived read model, which is every storable named fold kept for every key.
 *
 * One page per table: its key, the fold behind each column, and the rows. A table is one
 * key signature, so the folds listed together here are exactly the ones `hekla check
 * --tables` groups together, and a collection column is listed beside the scalars even
 * though it is stored apart, because the served row joins them.
 *
 * The rows come from the public tables API, for the reason the projectors page reads the
 * projector read API: what this shows is what an application sees over the same port, sealed
 * columns opened and erased ones absent, not a privileged view of the store. */

import { html, useState } from './vendor-preact.js'
import { api } from './api.js'
import { go, setQuery } from './router.js'
import { useStatus } from './store.js'
import { Empty, Resource, useResource } from './ui-states.js'
import { DataTable, Pager } from './ui-table.js'
import { Lag } from './ui-badge.js'
import { Copy } from './ui-copy.js'
import { DetailPanel } from './ui-panel.js'
import { JsonTree } from './ui-json.js'
import { baseKind, numeric } from './kinds.js'
import { count, plural, truncate } from './format.js'

/** Rows per page, the read API's own default. */
const PAGE = 50

/* A key part's filter rides in the query string as `k.<part>`. The `.` keeps it apart
 * from `cursor` and `row`, which no key part can be named, since a part is an
 * identifier. */
const FILTER = 'k.'

export function TablesView({ params, search }) {
  if (params?.name) {
    return html`<${TableDetail} key=${params.name} name=${params.name} search=${search} />`
  }
  return html`<${TableList} />`
}

/* Every column of the store is at one position, so the store's state is one line and
 * not a column per table: it says whether anything is served at all. */
/* The terms of a `kv` list rather than a list of its own, so the detail page can put
 * them in the same list as the table's key and they line up. */
function StoreState({ data }) {
  return html`
      <dt>position</dt>
      <dd>${count(data.head)}</dd>
      <dt>lag</dt>
      <dd><${Lag} value=${Math.max(0, data.log_head - data.head)} /></dd>
      <dt>serving</dt>
      <dd>
        ${data.serving
          ? html`<span class="faint">every table</span>`
          : data.behind > 0
            ? html`<span class="pill warn plain">${plural(data.behind, 'column')} refolding</span>
                <span class="note">no table is served until they catch up</span>`
            : html`<span class="pill warn plain">not yet</span>
                <span class="note">folding from the start of the log</span>`}
      </dd>
      ${data.last_error &&
      html`
        <dt>last error</dt>
        <dd style=${{ color: 'var(--err)' }}>${data.last_error}</dd>
      `}
  `
}

function TableList() {
  const { tick } = useStatus()
  const listing = useResource((signal) => api.tables(signal), [tick])

  const columns = [
    { key: 'name', header: 'Table', render: (row) => html`<code>${row.name}</code>` },
    {
      key: 'key',
      header: 'Key',
      render: (row) =>
        html`<span class="mono tiny dim">
          ${row.key.map((part) => part.name).join(', ') || 'one row'}
        </span>`,
    },
    {
      key: 'columns',
      header: 'Columns',
      align: 'right',
      width: '90px',
      render: (row) => html`<span class="mono">${row.columns.length}</span>`,
    },
    {
      key: 'folds',
      header: 'Folds',
      render: (row) =>
        html`<span class="tiny dim">
          ${truncate(row.columns.map((column) => column.fold).join(' · '), 90)}
        </span>`,
    },
  ]

  return html`
    <section class="card">
      <header>Tables</header>
      <${Resource}
        state=${listing}
        empty=${(data) =>
          data.tables.length === 0
            ? html`<${Empty} title="No derived tables">
                This project declares no storable named fold. A <code>fold</code> with a name
                and parameters is kept here, one row per key.
              <//>`
            : null}
      >
        ${(data) => html`
          <div class="body"><dl class="kv"><${StoreState} data=${data} /></dl></div>
          <${DataTable}
            label="Tables"
            columns=${columns}
            rows=${data.tables}
            onOpen=${(row) => go(`/admin/tables/${encodeURIComponent(row.name)}`)}
          />
        `}
      <//>
    </section>
  `
}

/** The key-part filters and the rest of the browse state, read off the query string. */
function browsing(search) {
  const filters = []
  for (const [name, value] of search ?? []) {
    if (name.startsWith(FILTER)) filters.push([name.slice(FILTER.length), value])
  }
  return {
    filters,
    cursor: search?.get('cursor') ?? null,
    row: search?.get('row') ?? null,
  }
}

/**
 * Replace the browse query wholesale, for the projectors page's reason: a new filter
 * invalidates the cursor, and a new page invalidates the open row.
 */
function browse({ filters = [], cursor, row } = {}) {
  setQuery([
    ...filters.map(([name, value]) => [`${FILTER}${name}`, value]),
    ['cursor', cursor],
    ['row', row],
  ])
}

function TableDetail({ name, search }) {
  const { status } = useStatus()
  const [counts, setCounts] = useState(false)
  const state = useResource((signal) => api.table(name, { counts }, signal), [name, counts])
  const { filters, cursor, row } = browsing(search)

  return html`
    <${Resource} state=${state}>
      ${(table) => html`
        <section class="card">
          <header>
            <a
              href="/admin/tables"
              onClick=${(clicked) => {
                clicked.preventDefault()
                go('/admin/tables')
              }}
            >
              ← Tables
            </a>
            <code style=${{ textTransform: 'none', letterSpacing: 0 }}>${table.name}</code>
            <div style=${{ flex: 1 }}></div>
            ${table.rows !== null &&
            html`<span class="pill mute plain mono">${plural(table.rows, 'row')}</span>`}
            <button
              type="button"
              class="btn"
              onClick=${() => setCounts(!counts)}
              title="a row count is a scan of the whole table, so it is opt-in"
            >
              ${counts ? '✓ counts' : '⟳ count rows'}
            </button>
          </header>
          <div class="body">
            <dl class="kv">
              <dt>key</dt>
              <dd>
                ${table.key.length === 0
                  ? html`<span class="faint">none: a fold with no parameters is one row</span>`
                  : table.key.map(
                      (part, index) => html`
                        ${index > 0 && html`<span class="faint"> · </span>`}
                        <code>${part.name}</code>
                        <span class="note">
                          ${part.kind}${part.identifies ? `, identifies ${part.identifies}` : ''}
                        </span>
                      `,
                    )}
              </dd>
              <dt>indexes</dt>
              <dd>
                ${table.indexes.length === 0
                  ? html`<span class="faint">the key only</span>`
                  : table.indexes.map(
                      (index) => html`<code>(${index.columns.join(', ')})</code> `,
                    )}
                <span class="note">derived from arms that leave a key part open</span>
              </dd>
              ${status?.tables &&
              html`<${StoreState} data=${{ ...status.tables, log_head: status.log_head }} />`}
            </dl>
          </div>
          <${Columns} table=${table} />
        </section>

        <div class=${row === null ? undefined : 'with-detail'}>
          <${Rows} table=${table} filters=${filters} cursor=${cursor} openRow=${row} />
          ${row !== null &&
          html`
            <${RowDetail}
              key=${row}
              table=${table}
              keyText=${row}
              onClose=${() => browse({ filters, cursor })}
            />
          `}
        </div>
      `}
    <//>
  `
}

function Columns({ table }) {
  return html`
    <table class="data">
      <thead>
        <tr>
          <th scope="col">Column</th>
          <th scope="col">Fold</th>
          <th scope="col">Kind</th>
          <th scope="col" style=${{ width: '130px' }}>Sealed under</th>
          <th scope="col" style=${{ width: '100px' }}>Stored</th>
        </tr>
      </thead>
      <tbody>
        ${table.columns.map(
          (column) => html`
            <tr key=${column.name}>
              <td><code>${column.name}</code></td>
              <td><code class="tiny">${column.fold}</code></td>
              <td><span class="mono tiny dim">${column.kind}</span></td>
              <td>
                ${column.subject
                  ? html`<code class="tiny">${column.subject}</code>`
                  : html`<span class="faint">·</span>`}
              </td>
              <td>
                <span class="tiny dim" title=${column.collection
                  ? 'a collection, kept in the side table and joined back into the row'
                  : undefined}>
                  ${column.collection ? 'side table' : 'row'}
                </span>
              </td>
            </tr>
          `,
        )}
      </tbody>
    </table>
  `
}

/* --- the rows ------------------------------------------------------------- */

/** A key as one string, so a row can be named in the query string and compared. */
function keyOf(table, row) {
  return JSON.stringify(table.key.map((part) => row[part.name]))
}

/**
 * What a column that is not in the response means. A NULL is left out, and so is a
 * sealed column whose subject's key is gone, and the response cannot tell them apart;
 * which of them is possible is a fact about the declaration.
 */
function Absent({ column }) {
  if (column.subject) {
    const why = `the key for \`${column.subject}\` cannot be obtained: erased, or written under a superseded one`
    return html`<span class="pill mute plain" title=${column.optional ? `none, or ${why}` : why}>
      absent
    </span>`
  }
  return html`<span class="faint" title="the fold answers none for this key">none</span>`
}

function Cell({ column, row }) {
  if (!Object.hasOwn(row, column.name)) return html`<${Absent} column=${column} />`
  const value = row[column.name]
  if (baseKind(column.kind) === 'Json') {
    const text = JSON.stringify(value)
    return html`<span class="mono tiny dim" title=${text}>${truncate(text, 48)}</span>`
  }
  if (typeof value === 'boolean' || typeof value === 'number') {
    return html`<span class="mono">${String(value)}</span>`
  }
  const text = String(value)
  return html`<span title=${text.length > 44 ? text : undefined}>${truncate(text, 44)}</span>`
}

function Value({ column, row }) {
  if (!Object.hasOwn(row, column.name)) return html`<${Absent} column=${column} />`
  const value = row[column.name]
  if (baseKind(column.kind) === 'Json') return html`<${JsonTree} value=${value} />`
  return html`<span class="mono">${String(value)}</span>`
}

function Rows({ table, filters, cursor, openRow }) {
  /* Not on the shared tick, for the projectors page's reason: rows shifting under
   * someone reading them is worse than rows a few seconds old. ⟳ refreshes. */
  const state = useResource(
    (signal) => api.tableRows(table.name, { filters, cursor, limit: PAGE }, signal),
    [table.name, JSON.stringify(filters), cursor],
  )
  const [trail, setTrail] = useState([])
  const [draft, setDraft] = useState(() => Object.fromEntries(filters))

  /* A filter is a leading run of the key, or of an index a range arm derived, so a part
   * is offered once every key part before it has a value, or when it leads an index: the
   * server would refuse anything else, and saying so here is kinder than a 400. */
  const leadsIndex = (name) => table.indexes.some((index) => index.columns[0] === name)
  const offered = (index) =>
    leadsIndex(table.key[index].name) ||
    table.key.slice(0, index).every((part) => (draft[part.name] ?? '').trim() !== '')

  const apply = () => {
    setTrail([])
    const chosen = []
    for (const [index, part] of table.key.entries()) {
      const value = (draft[part.name] ?? '').trim()
      if (value && offered(index)) chosen.push([part.name, value])
    }
    browse({ filters: chosen })
  }

  const clear = () => {
    setTrail([])
    setDraft({})
    browse({})
  }

  const older = () => {
    const next = state.data?.next_cursor
    if (next === null || next === undefined) return
    setTrail((current) => [...current, cursor ?? ''])
    browse({ filters, cursor: next })
  }

  const newer = () => {
    const previous = trail[trail.length - 1]
    setTrail((current) => current.slice(0, -1))
    browse({ filters, cursor: previous || null })
  }

  const keyColumns = table.key.map((part) => ({
    key: part.name,
    header: part.name,
    align: numeric(part.kind) ? 'right' : undefined,
    render: (row) => html`<span class="mono">${String(row[part.name])}</span>`,
  }))
  const columns = [
    ...keyColumns,
    ...table.columns.map((column) => ({
      key: column.name,
      header: column.name,
      align: numeric(column.kind) ? 'right' : undefined,
      render: (row) => html`<${Cell} column=${column} row=${row} />`,
    })),
  ]

  return html`
    <section class="card">
      <header>
        Rows
        <span class="tiny faint" style=${{ textTransform: 'none', letterSpacing: 0 }}>
          in key order
        </span>
        <div style=${{ flex: 1 }}></div>
        ${state.data &&
        html`
          <span class="tiny faint mono" style=${{ textTransform: 'none', letterSpacing: 0 }}>
            as of position ${count(state.data.position)}
          </span>
        `}
        <button
          type="button"
          class="btn icon"
          onClick=${() => state.reload()}
          title="Re-run this scan"
          aria-label="Re-run this scan"
        >
          ⟳
        </button>
      </header>

      ${table.key.length > 0 &&
      html`
        <div class="filters">
          ${table.key.map(
            (part, index) => html`
              <div class="filter-input" key=${part.name}>
                <span class="chip-kind">${part.name}</span>
                <input
                  value=${draft[part.name] ?? ''}
                  placeholder=${offered(index) ? part.kind : `set ${table.key[index - 1].name} first`}
                  disabled=${!offered(index)}
                  onInput=${(typed) => setDraft({ ...draft, [part.name]: typed.target.value })}
                  onKeyDown=${(pressed) => {
                    if (pressed.key === 'Enter') {
                      pressed.preventDefault()
                      apply()
                    }
                  }}
                  aria-label=${`Filter on ${part.name}`}
                  spellcheck="false"
                  autocomplete="off"
                />
              </div>
            `,
          )}
          <button type="button" class="btn" onClick=${apply}>apply</button>
          ${filters.length > 0 &&
          html`
            <span class="chip">
              <span class="chip-kind">where</span>
              <code>${filters.map(([name, value]) => `${name} = ${value}`).join(', ')}</code>
              <button type="button" onClick=${clear} aria-label="Remove the filter">✕</button>
            </span>
          `}
        </div>
      `}

      ${state.error?.unavailable
        ? html`<${Empty} title=${state.error.code}>${state.error.message}<//>`
        : html`
            <${Resource}
              state=${state}
              empty=${(data) =>
                data.items.length === 0
                  ? html`
                      <${Empty} title=${filters.length ? 'No rows match' : 'No rows'}>
                        ${filters.length
                          ? 'No key with these parts has been named by any event.'
                          : cursor
                            ? 'You have paged past the last row.'
                            : 'No event has named a key of this table yet.'}
                      <//>
                    `
                  : null}
            >
              ${(data) => html`
                <${DataTable}
                  label=${`${table.name} rows`}
                  columns=${columns}
                  rows=${data.items}
                  selected=${(item) => keyOf(table, item) === openRow}
                  onOpen=${(item) => browse({ filters, cursor, row: keyOf(table, item) })}
                />
                <${Pager}
                  cursor=${data.next_cursor}
                  canGoBack=${trail.length > 0}
                  onOlder=${older}
                  onNewer=${newer}
                >
                  <span class="tiny faint">${plural(data.items.length, 'row')} on this page</span>
                <//>
              `}
            <//>
          `}
    </section>
  `
}

function RowDetail({ table, keyText, onClose }) {
  let values = []
  try {
    values = JSON.parse(keyText)
  } catch {
    values = []
  }
  const key = table.key.map((part, index) => [part.name, values[index]])
  const state = useResource(
    (signal) => api.tableRow(table.name, key, signal),
    [table.name, keyText],
  )
  const query = new URLSearchParams(key.map(([name, value]) => [name, String(value)]))
  const route = `/tables/${table.name}/row`
  const path = table.key.length ? `${route}?${query}` : route

  return html`
    <${DetailPanel}
      title=${key.map(([, value]) => String(value)).join(' · ') || 'the one row'}
      subtitle=${table.name}
      onClose=${onClose}
      actions=${state.data &&
      html`<${Copy} value=${JSON.stringify(state.data.item, null, 2)} title="Copy as JSON" />`}
    >
      <${Resource} state=${state}>
        ${(found) => html`
          <dl class="kv">
            ${key.map(
              ([name, value]) => html`
                <dt key=${name}>${name}</dt>
                <dd><span class="mono">${String(value)}</span><span class="note">key</span></dd>
              `,
            )}
            ${table.columns.map(
              (column) => html`
                <dt key=${column.name}>${column.name}</dt>
                <dd>
                  <${Value} column=${column} row=${found.item} />
                  <span class="note">${column.fold}</span>
                  ${column.subject && html`<span class="note">sealed under ${column.subject}</span>`}
                </dd>
              `,
            )}
          </dl>

          <h3 class="section-title">Read API</h3>
          <div class="row wrap">
            <a href=${path} target="_blank" rel="noreferrer"><code>GET ${path}</code></a>
            <${Copy} value=${path} />
          </div>
          <p class="tiny faint">
            read at position ${count(found.position)}, in the same snapshot as the row
          </p>
        `}
      <//>
    <//>
  `
}
