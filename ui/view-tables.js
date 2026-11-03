/* Folds: the derived read model, which is every storable named fold kept for every key.
 *
 * One page per group of folds that share a key: the key, each fold, and the rows. A group
 * is one key signature, so the folds listed together here are exactly the ones `hekla check
 * --tables` groups together, and a collection fold is listed beside the scalars even
 * though it is stored apart, because the served row joins them. Where they are kept is the
 * store's business: the page is reached by an opaque id, and every read names folds.
 *
 * The rows come from the public rows API, for the reason the projectors page reads the
 * projector read API: what this shows is what an application sees over the same port, sealed
 * folds opened and erased ones absent, not a privileged view of the store. */

import { html, useState } from './vendor-preact.js'
import { api } from './api.js'
import { go, setQuery } from './router.js'
import { useStatus } from './store.js'
import { Empty, ErrorState, Resource, useResource } from './ui-states.js'
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

/* A fold lookup's arguments ride as `a.<param>`, named as the fold declares them. */
const ARG = 'a.'

export function TablesView({ params, search }) {
  if (params?.id) {
    return html`<${TableDetail} key=${params.id} id=${params.id} search=${search} />`
  }
  return html`<${TableList} />`
}

/** The key as the first fold names its parameters, `(shop_id, order_id)`. */
function keyText(table) {
  return table.key.length ? `(${table.key.map((part) => part.name).join(', ')})` : 'one row'
}

/** The folds to name in a read of this group, the first one first so the key is spelled
 * in its parameters. */
function foldNames(table) {
  return table.folds.map((fold) => fold.fold)
}

/* Every fold of the store is at one position, so the store's state is one line and not
 * a line per group: it says whether anything is served at all. */
/* The terms of a `kv` list rather than a list of its own, so the detail page can put
 * them in the same list as the key and they line up. */
function StoreState({ data }) {
  return html`
      <dt>position</dt>
      <dd>${count(data.head)}</dd>
      <dt>lag</dt>
      <dd><${Lag} value=${Math.max(0, data.log_head - data.head)} /></dd>
      <dt>serving</dt>
      <dd>
        ${data.serving
          ? html`<span class="faint">every fold</span>`
          : data.behind > 0
            ? html`<span class="pill warn plain">${plural(data.behind, 'fold')} refolding</span>
                <span class="note">no fold is served until they catch up</span>`
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
    {
      key: 'key',
      header: 'Key',
      render: (row) => html`<code>${keyText(row)}</code>`,
    },
    {
      key: 'folds',
      header: 'Folds',
      render: (row) =>
        html`<span class="tiny">${truncate(foldNames(row).join(' · '), 110)}</span>`,
    },
    {
      key: 'count',
      header: 'Count',
      align: 'right',
      width: '90px',
      render: (row) => html`<span class="mono">${row.folds.length}</span>`,
    },
  ]

  return html`
    <section class="card">
      <header>Stored folds</header>
      <${Resource}
        state=${listing}
        empty=${(data) =>
          data.tables.length === 0
            ? html`<${Empty} title="No stored folds">
                This project declares no storable named fold. A <code>fold</code> with a name
                and parameters is kept here, one row per key.
              <//>`
            : null}
      >
        ${(data) => html`
          <div class="body"><dl class="kv"><${StoreState} data=${data} /></dl></div>
          <${DataTable}
            label="Stored folds, by key"
            columns=${columns}
            rows=${data.tables}
            onOpen=${(row) => go(`/admin/tables/${encodeURIComponent(row.id)}`)}
          />
        `}
      <//>
    </section>
  `
}

/** The key-part filters and the rest of the browse state, read off the query string. */
function browsing(search) {
  const filters = []
  const args = []
  for (const [name, value] of search ?? []) {
    if (name.startsWith(FILTER)) filters.push([name.slice(FILTER.length), value])
    if (name.startsWith(ARG)) args.push([name.slice(ARG.length), value])
  }
  return {
    filters,
    cursor: search?.get('cursor') ?? null,
    row: search?.get('row') ?? null,
    fold: search?.get('fold') ?? null,
    args,
  }
}

/**
 * Replace the browse query wholesale, for the projectors page's reason: a new filter
 * invalidates the cursor, and a new page invalidates the open row.
 *
 * The fold lookup is the exception, since nothing about the rows invalidates it: left
 * `undefined` it carries over from the current URL, so paging or opening a row keeps it
 * open, and `null` is what closes it.
 */
function browse({ filters = [], cursor, row, fold, args } = {}) {
  let keptFold = fold
  let keptArgs = args ?? []
  if (fold === undefined) {
    const current = browsing(new URLSearchParams(window.location.search))
    keptFold = current.fold
    keptArgs = current.args
  }
  setQuery([
    ...filters.map(([name, value]) => [`${FILTER}${name}`, value]),
    ['cursor', cursor],
    ['row', row],
    ['fold', keptFold],
    ...(keptFold ? keptArgs : []).map(([name, value]) => [`${ARG}${name}`, value]),
  ])
}

function TableDetail({ id, search }) {
  const { status } = useStatus()
  const [counts, setCounts] = useState(false)
  const state = useResource((signal) => api.table(id, { counts }, signal), [id, counts])
  const { filters, cursor, row, fold, args } = browsing(search)
  const lookUp = (name, given = []) => browse({ filters, cursor, row, fold: name, args: given })

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
              ← Folds
            </a>
            <span style=${{ textTransform: 'none', letterSpacing: 0 }}>
              ${table.key.length ? html`keyed by <code>${keyText(table)}</code>` : 'one row'}
            </span>
            <div style=${{ flex: 1 }}></div>
            <span
              class="tiny faint mono"
              style=${{ textTransform: 'none', letterSpacing: 0 }}
              title="Where the store keeps these folds. No read names it."
            >
              ${table.id}
            </span>
            ${table.rows !== null &&
            html`<span class="pill mute plain mono">${plural(table.rows, 'row')}</span>`}
            <button
              type="button"
              class="btn"
              onClick=${() => setCounts(!counts)}
              title="a row count is a scan of every key, so it is opt-in"
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
                      (index) => html`
                        <code>(${index.join(', ')})</code>
                        <span class="note">a range arm reaches keys through it</span>
                      `,
                    )}
              </dd>
              ${table.sorts.length > 0 &&
              html`
                <dt>orderings</dt>
                <dd>
                  ${table.sorts.map(
                    (sort) => html`
                      <code>${sort.fold}</code>
                      <span class="note">
                        ${sort.lead.length
                          ? `an @index orders by it, with (${sort.lead.join(', ')}) pinned`
                          : 'an @index orders by it'}
                      </span>
                    `,
                  )}
                </dd>
              `}
              ${status?.tables &&
              html`<${StoreState} data=${{ ...status.tables, log_head: status.log_head }} />`}
            </dl>
          </div>
          <${Folds} table=${table} onFold=${lookUp} />
        </section>

        ${fold !== null &&
        html`
          <${FoldLookup}
            key=${`${fold}:${JSON.stringify(args)}`}
            table=${table}
            fold=${fold}
            args=${args}
            onClose=${() => browse({ filters, cursor, row, fold: null })}
          />
        `}

        <div class=${row === null ? undefined : 'with-detail'}>
          <${Rows} table=${table} filters=${filters} cursor=${cursor} openRow=${row} />
          ${row !== null &&
          html`
            <${RowDetail}
              key=${row}
              table=${table}
              keyText=${row}
              onClose=${() => browse({ filters, cursor })}
              onFold=${lookUp}
            />
          `}
        </div>
      `}
    <//>
  `
}

function Folds({ table, onFold }) {
  return html`
    <table class="data">
      <thead>
        <tr>
          <th scope="col">Fold</th>
          <th scope="col">Parameters</th>
          <th scope="col">Kind</th>
          <th scope="col" style=${{ width: '130px' }}>Sealed under</th>
          <th scope="col" style=${{ width: '100px' }}>Stored</th>
          <th scope="col" style=${{ width: '80px' }}>History</th>
        </tr>
      </thead>
      <tbody>
        ${table.folds.map(
          (fold) => html`
            <tr key=${fold.fold}>
              <td>
                <a
                  href=${foldHref(fold.fold)}
                  onClick=${(clicked) => {
                    clicked.preventDefault()
                    onFold(fold.fold)
                  }}
                  title="Look this fold up for one key"
                >
                  <code>${fold.fold}</code>
                </a>
              </td>
              <td>
                <span class="mono tiny dim">
                  ${fold.args.length ? `(${fold.args.join(', ')})` : '()'}
                </span>
              </td>
              <td><span class="mono tiny dim">${fold.kind}</span></td>
              <td>
                ${fold.subject
                  ? html`<code class="tiny">${fold.subject}</code>`
                  : html`<span class="faint">·</span>`}
              </td>
              <td>
                <span class="tiny dim" title=${fold.collection
                  ? 'a collection, kept apart and joined back into the row'
                  : undefined}>
                  ${fold.collection ? 'apart' : 'row'}
                </span>
              </td>
              <td>
                ${fold.history
                  ? html`<span class="tiny dim">kept</span>`
                  : html`<span class="faint">·</span>`}
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
 * What a fold that is not in the response means. A none is left out, and so is a sealed
 * fold whose subject's key is gone, and the response cannot tell them apart; which of
 * them is possible is a fact about the declaration.
 */
function Absent({ fold }) {
  if (fold.subject) {
    const why = `the key for \`${fold.subject}\` cannot be obtained: erased, or written under a superseded one`
    return html`<span class="pill mute plain" title=${fold.optional ? `none, or ${why}` : why}>
      absent
    </span>`
  }
  return html`<span class="faint" title="the fold answers none for this key">none</span>`
}

function Cell({ fold, row }) {
  if (!Object.hasOwn(row, fold.fold)) return html`<${Absent} fold=${fold} />`
  const value = row[fold.fold]
  if (baseKind(fold.kind) === 'Json') {
    const text = JSON.stringify(value)
    return html`<span class="mono tiny dim" title=${text}>${truncate(text, 48)}</span>`
  }
  if (typeof value === 'boolean' || typeof value === 'number') {
    return html`<span class="mono">${String(value)}</span>`
  }
  const text = String(value)
  return html`<span title=${text.length > 44 ? text : undefined}>${truncate(text, 44)}</span>`
}

function Value({ fold, row }) {
  if (!Object.hasOwn(row, fold.fold)) return html`<${Absent} fold=${fold} />`
  const value = row[fold.fold]
  if (baseKind(fold.kind) === 'Json') return html`<${JsonTree} value=${value} />`
  return html`<span class="mono">${String(value)}</span>`
}

/* An ordering rides with the filters, as the scan's own `order_by`. */
const ORDER = 'order_by'

/* A filter input takes a value, or bounds written `>=a <b`, which become the scan's
 * `part.gte` and `part.lt`. Longest operator first, so `>=` is not read as `>` of `=a`. */
const BOUNDS = [
  ['>=', 'gte'],
  ['<=', 'lte'],
  ['>', 'gt'],
  ['<', 'lt'],
]

function parsed(part, value) {
  const words = value.split(/\s+/)
  const bounds = words.map((word) => {
    const found = BOUNDS.find(([sign]) => word.startsWith(sign))
    return found && word.length > found[0].length
      ? [`${part}.${found[1]}`, word.slice(found[0].length)]
      : null
  })
  return bounds.every(Boolean) ? bounds : [[part, value]]
}

/* The inputs' text back from the query string, so a reload shows what was asked. */
function draftOf(filters) {
  const draft = {}
  for (const [name, value] of filters) {
    if (name === ORDER) continue
    const [part, op] = name.split('.')
    const sign = BOUNDS.find(([, word]) => word === op)?.[0]
    const text = sign ? `${sign}${value}` : value
    draft[part] = draft[part] ? `${draft[part]} ${text}` : text
  }
  return draft
}

function spelled([name, value]) {
  if (name === ORDER) return value.startsWith('-') ? `by ${value.slice(1)} ↓` : `by ${value} ↑`
  const [part, op] = name.split('.')
  const sign = BOUNDS.find(([, word]) => word === op)?.[0]
  return sign ? `${part} ${sign} ${value}` : `${part} = ${value}`
}

function Rows({ table, filters, cursor, openRow }) {
  const folds = foldNames(table)
  /* Not on the shared tick, for the projectors page's reason: rows shifting under
   * someone reading them is worse than rows a few seconds old. ⟳ refreshes. */
  const state = useResource(
    (signal) => api.foldRows(folds, { filters, cursor, limit: PAGE }, signal),
    [table.id, JSON.stringify(filters), cursor],
  )
  const [trail, setTrail] = useState([])
  const [draft, setDraft] = useState(() => draftOf(filters))

  /* Any key part can be filled in. Which sets of them are one index seek is the server's
   * to say, and its refusal names the sets it serves, which is more use than a guess. */
  const apply = () => {
    setTrail([])
    const chosen = []
    for (const part of table.key) {
      const value = (draft[part.name] ?? '').trim()
      if (value) chosen.push(...parsed(part.name, value))
    }
    const order = filters.find(([name]) => name === ORDER)
    browse({ filters: order ? [...chosen, order] : chosen })
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

  const ordering = filters.find(([name]) => name === ORDER)?.[1] ?? ''
  const reorder = (value) => {
    setTrail([])
    const rest = filters.filter(([name]) => name !== ORDER)
    browse({ filters: value ? [...rest, [ORDER, value]] : rest })
  }

  const keyColumns = table.key.map((part) => ({
    key: part.name,
    header: part.name,
    align: numeric(part.kind) ? 'right' : undefined,
    render: (row) => html`<span class="mono">${String(row[part.name])}</span>`,
  }))
  const columns = [
    ...keyColumns,
    ...table.folds.map((fold) => ({
      key: fold.fold,
      header: fold.fold,
      align: numeric(fold.kind) ? 'right' : undefined,
      render: (row) => html`<${Cell} fold=${fold} row=${row} />`,
    })),
  ]

  return html`
    <section class="card">
      <header>
        Rows
        <span class="tiny faint" style=${{ textTransform: 'none', letterSpacing: 0 }}>
          ${ordering ? spelled([ORDER, ordering]) : 'in key order'}
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
            (part) => html`
              <div class="filter-input" key=${part.name}>
                <span class="chip-kind">${part.name}</span>
                <input
                  value=${draft[part.name] ?? ''}
                  placeholder=${`${part.kind}, or >=a <b`}
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
          ${table.sorts.length > 0 &&
          html`
            <select
              class="btn"
              value=${ordering}
              onChange=${(changed) => reorder(changed.target.value)}
              aria-label="Order rows by"
              title="Order by a fold an @index declares; pin the parameters it is ordered within first"
            >
              <option value="">key order</option>
              ${table.sorts.flatMap((sort) => {
                const within = sort.lead.length ? ` per ${sort.lead.join(', ')}` : ''
                return [
                  html`<option value=${sort.fold}>${sort.fold} ↑${within}</option>`,
                  html`<option value=${`-${sort.fold}`}>${sort.fold} ↓${within}</option>`,
                ]
              })}
            </select>
          `}
          ${filters.length > 0 &&
          html`
            <span class="chip">
              <span class="chip-kind">where</span>
              <code>${filters.map(spelled).join(', ')}</code>
              <button type="button" onClick=${clear} aria-label="Remove the filter">✕</button>
            </span>
          `}
        </div>
      `}

      ${state.error?.unavailable
        ? html`<${Empty} title=${state.error.code}>${state.error.message}<//>`
        : state.error?.status === 400
          ? html`<${ErrorState} error=${state.error} />`
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
                            : 'No event has named a key of these folds yet.'}
                      <//>
                    `
                  : null}
            >
              ${(data) => html`
                <${DataTable}
                  label=${`Rows keyed by ${keyText(table)}`}
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

function RowDetail({ table, keyText: text, onClose, onFold }) {
  let values = []
  try {
    values = JSON.parse(text)
  } catch {
    values = []
  }
  const folds = foldNames(table)
  const withHistory = table.folds.filter((fold) => fold.history)
  const kept = withHistory.map((fold) => fold.fold)
  const key = table.key.map((part, index) => [part.name, values[index]])
  // A history read is named in its own first fold's parameters, which need not be the
  // group's first fold's: `(shop: Shop)` and `(shop_id: Shop)` share a key.
  const historyKey = (withHistory[0]?.args ?? []).map((arg, index) => [arg, values[index]])
  const state = useResource(
    (signal) => api.foldRow(folds, key, signal),
    [table.id, text],
  )
  const query = new URLSearchParams([
    ['folds', folds.join(',')],
    ...key.map(([name, value]) => [name, String(value)]),
  ])
  const path = `/row?${query}`

  return html`
    <${DetailPanel}
      title=${key.map(([, value]) => String(value)).join(' · ') || 'the one row'}
      subtitle=${keyText(table)}
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
            ${table.folds.map(
              (fold) => html`
                <dt key=${fold.fold}>
                  <a
                    href=${foldHref(fold.fold)}
                    onClick=${(clicked) => {
                      clicked.preventDefault()
                      onFold(
                        fold.fold,
                        /* Only a whole key starts the lookup filled in: a part the row
                         * text did not carry would be sent as the word `undefined`. */
                        values.length === fold.args.length &&
                        values.every((value) => value !== null && value !== undefined)
                          ? fold.args.map((arg, index) => [arg, String(values[index])])
                          : [],
                      )
                    }}
                    title="Look this fold up for this row's key"
                  >
                    ${fold.fold}
                  </a>
                </dt>
                <dd>
                  <${Value} fold=${fold} row=${found.item} />
                  ${fold.subject && html`<span class="note">sealed under ${fold.subject}</span>`}
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
          ${kept.length > 0 &&
          html`<${History}
            load=${(before, signal) => api.rowHistory(kept, historyKey, { before }, signal)}
            deps=${[table.id, text]}
          />`}
        `}
      <//>
    <//>
  `
}

/**
 * What changed a row's folds or one fold, newest first: each record that moved one, the
 * value before and after, and a link to the event. `load(before, signal)` reads a page.
 */
function History({ load, deps }) {
  const [before, setBefore] = useState(null)
  const [trail, setTrail] = useState([])
  const state = useResource((signal) => load(before ?? undefined, signal), [...deps, before])
  const shown = (value) =>
    value === null || value === undefined
      ? html`<span class="faint">absent</span>`
      : typeof value === 'object'
        ? html`<code class="tiny">${truncate(JSON.stringify(value), 60)}</code>`
        : html`<span class="mono">${String(value)}</span>`
  return html`
    <h3 class="section-title">History <span class="note">newest first</span></h3>
    ${state.error?.code === 'no_history'
      ? html`<p class="tiny faint">${state.error.message}</p>`
      : html`
          <${Resource}
            state=${state}
            empty=${(data) =>
              data.items.length === 0
                ? html`<p class="tiny faint">Nothing has changed it yet.</p>`
                : null}
          >
            ${(data) => html`
              <ol class="history">
                ${data.items.map(
                  (item) => html`
                    <li key=${item.position}>
                      <div class="row" style=${{ gap: '8px' }}>
                        <a
                          class="mono tiny"
                          href=${`/admin/events/${item.position}`}
                          onClick=${(clicked) => {
                            clicked.preventDefault()
                            go(`/admin/events/${item.position}`)
                          }}
                          title="The event that made this change"
                        >
                          #${item.position}
                        </a>
                        <code class="tiny">${item.event?.type}</code>
                      </div>
                      <dl class="kv">
                        ${Object.entries(item.changed).map(
                          ([name, { before: was, after }]) => html`
                            <dt key=${name}>${name}</dt>
                            <dd>${shown(was)} <span class="faint">→</span> ${shown(after)}</dd>
                          `,
                        )}
                      </dl>
                    </li>
                  `,
                )}
              </ol>
              <div class="row" style=${{ gap: '8px' }}>
                <button
                  type="button"
                  class="btn"
                  disabled=${trail.length === 0}
                  onClick=${() => {
                    setBefore(trail[trail.length - 1] ?? null)
                    setTrail(trail.slice(0, -1))
                  }}
                >
                  ← newer
                </button>
                <button
                  type="button"
                  class="btn"
                  disabled=${data.next_before === null}
                  onClick=${() => {
                    setTrail([...trail, before])
                    setBefore(data.next_before)
                  }}
                >
                  older →
                </button>
              </div>
            `}
          </>
        `}
  `
}

/* --- one fold -------------------------------------------------------------- */

/** This page with a fold's lookup open, as a real link so it can open in a new tab. */
function foldHref(fold) {
  return `${window.location.pathname}?fold=${encodeURIComponent(fold)}`
}

/**
 * One named fold for one key, through `GET /folds/{fold}`: what an application calling a
 * fold by name gets, with its arguments named as the fold declares its parameters rather
 * than as the first fold spells the key. Opened from a row, it starts with that row's key.
 */
function FoldLookup({ table, fold, args, onClose }) {
  const declared = table.folds.find((candidate) => candidate.fold === fold)
  const [draft, setDraft] = useState(() => Object.fromEntries(args))
  const complete =
    declared !== undefined && declared.args.every((arg) => (draft[arg] ?? '').trim() !== '')
  /* What was last asked for, so the result belongs to the arguments that produced it
   * rather than to whatever the inputs say now. */
  const [asked, setAsked] = useState(() =>
    complete ? declared.args.map((arg) => [arg, draft[arg].trim()]) : null,
  )
  const state = useResource(
    (signal) => (asked ? api.fold(fold, asked, signal) : Promise.resolve(null)),
    [fold, JSON.stringify(asked)],
  )

  if (declared === undefined) {
    return html`
      <section class="card">
        <header>Fold <code>${fold}</code></header>
        <${Empty} title="Not keyed here">
          <code>${fold}</code> is keyed otherwise than these folds, or not stored at all.
        <//>
      </section>
    `
  }

  const read = () => {
    if (complete) setAsked(declared.args.map((arg) => [arg, draft[arg].trim()]))
  }
  const query = new URLSearchParams(asked ?? [])
  const route = `/folds/${encodeURIComponent(fold)}`
  const path = asked && asked.length ? `${route}?${query}` : route

  return html`
    <section class="card">
      <header>
        Fold
        <code style=${{ textTransform: 'none', letterSpacing: 0 }}>${fold}</code>
        <span class="tiny faint" style=${{ textTransform: 'none', letterSpacing: 0 }}>
          ${declared.kind}
        </span>
        <div style=${{ flex: 1 }}></div>
        <button type="button" class="btn icon" onClick=${onClose} title="Close" aria-label="Close">
          ✕
        </button>
      </header>
      <div class="filters">
        ${declared.args.map(
          (arg, index) => html`
            <div class="filter-input" key=${arg}>
              <span class="chip-kind">${arg}</span>
              <input
                value=${draft[arg] ?? ''}
                placeholder=${table.key[index]?.kind ?? ''}
                onInput=${(typed) => setDraft({ ...draft, [arg]: typed.target.value })}
                onKeyDown=${(pressed) => {
                  if (pressed.key === 'Enter') {
                    pressed.preventDefault()
                    read()
                  }
                }}
                aria-label=${`The ${arg} argument`}
                spellcheck="false"
                autocomplete="off"
              />
            </div>
          `,
        )}
        <button type="button" class="btn" onClick=${read} disabled=${!complete}>read</button>
      </div>
      <div class="body">
        ${asked === null
          ? html`<p class="tiny faint">
              ${declared.args.length
                ? 'Give every argument to read the fold for that key.'
                : 'This fold takes no arguments.'}
            </p>`
          : state.error?.notFound
            ? html`<${Empty} title="No such key">
                No event has named this key, so <code>${fold}</code> holds its seed.
              <//>`
            : state.error?.unavailable
              ? html`<${Empty} title=${state.error.code}>${state.error.message}<//>`
              : html`
                  <${Resource} state=${state}>
                    ${(found) => html`
                      <dl class="kv">
                        <dt>value</dt>
                        <dd>
                          ${found.value === null
                            ? html`<${Absent} fold=${declared} />`
                            : baseKind(declared.kind) === 'Json'
                              ? html`<${JsonTree} value=${found.value} />`
                              : html`<span class="mono">${String(found.value)}</span>`}
                        </dd>
                        <dt>position</dt>
                        <dd>${count(found.position)}</dd>
                      </dl>
                      ${declared.history &&
                      html`<${History}
                        key=${JSON.stringify(asked)}
                        load=${(before, signal) => api.foldHistory(fold, asked, { before }, signal)}
                        deps=${[fold, JSON.stringify(asked)]}
                      />`}
                    `}
                  <//>
                `}
        <div class="row wrap" style=${{ marginTop: '8px' }}>
          <a href=${path} target="_blank" rel="noreferrer"><code>GET ${path}</code></a>
          <${Copy} value=${path} />
        </div>
      </div>
    </section>
  `
}
