/* Projectors: readiness, lag, the shape of what they materialised, and the rows
 * themselves.
 *
 * The entity shapes come from the read model itself rather than from the project
 * source, so this is what the rows were actually built under, not what the current
 * `.hk` file declares. On a stale or rebuilding projector those two differ, which is
 * exactly when you want to know.
 *
 * The rows are fetched through the public read API rather than through `/admin`, and
 * that is the point rather than a shortcut: this page shows what an application sees
 * over the same port. The same key order, the same refusal to scan an unindexed
 * column, the same columns missing where a subject key is gone. A privileged view of
 * the same table would show more and mean less. */

import { html, useState } from './vendor-preact.js'
import { api } from './api.js'
import { go, setQuery } from './router.js'
import { refreshNow, useStatus } from './store.js'
import { Empty, Resource, useResource } from './ui-states.js'
import { DataTable, Pager } from './ui-table.js'
import { Badge, Lag } from './ui-badge.js'
import { Confirm } from './ui-confirm.js'
import { Copy } from './ui-copy.js'
import { DetailPanel } from './ui-panel.js'
import { JsonTree } from './ui-json.js'
import { baseKind, numeric } from './kinds.js'
import { count, plural, shortHash, sources, truncate } from './format.js'

/** Rows per page. The read API's own default, so the console asks for what it tuned. */
const PAGE = 50

export function ProjectorsView({ params, search }) {
  if (params?.name) {
    return html`<${ProjectorDetail} key=${params.name} name=${params.name} search=${search} />`
  }
  return html`<${ProjectorList} />`
}

function ProjectorList() {
  const { tick } = useStatus()
  const listing = useResource((signal) => api.projectors(signal), [tick])

  const columns = [
    {
      key: 'readiness',
      header: '',
      width: '130px',
      render: (row) => html`<${Badge} kind="readiness" value=${row.readiness} />`,
    },
    { key: 'name', header: 'Name', render: (row) => html`<code>${row.name}</code>` },
    {
      key: 'position',
      header: 'Position',
      align: 'right',
      width: '110px',
      render: (row) => html`<span class="mono">${row.position}</span>`,
    },
    {
      key: 'lag',
      header: 'Lag',
      align: 'right',
      width: '80px',
      render: (row) => html`<${Lag} value=${row.lag} />`,
    },
    {
      key: 'entities',
      header: 'Entities',
      render: (row) =>
        html`<span class="tiny dim">
          ${row.entities.map((entity) => entity.name).join(' · ') || '-'}
        </span>`,
    },
    {
      key: 'sources',
      header: 'Sources',
      render: (row) => html`<span class="tiny dim">${sources(row.sources)}</span>`,
    },
  ]

  return html`
    <section class="card">
      <header>Projectors</header>
      <${Resource}
        state=${listing}
        empty=${(data) =>
          data.projectors.length === 0
            ? html`<${Empty} title="No projectors">This project declares none.<//>`
            : null}
      >
        ${(data) => html`
          <${DataTable}
            label="Projectors"
            columns=${columns}
            rows=${data.projectors}
            onOpen=${(row) => go(`/admin/projectors/${encodeURIComponent(row.name)}`)}
          />
        `}
      <//>
    </section>
  `
}

/**
 * Replace the browse query wholesale.
 *
 * Wholesale rather than merged, because each part is invalidated by a change to the
 * one before it: a new filter invalidates the cursor, and a new page invalidates the
 * open row, whose key is not on it. A merging helper would happily leave `row=`
 * pointing at a row the table no longer holds.
 */
function browse({ entity, field, value, cursor, row } = {}) {
  setQuery([
    ['entity', entity],
    ['field', field],
    ['value', value],
    ['cursor', cursor],
    ['row', row],
  ])
}

function ProjectorDetail({ name, search }) {
  const { status, tick } = useStatus()
  /* Counts are a full table scan, so they are a button rather than a default, and the
   * endpoint refuses them unless the projector is `ready`: a model at a previous
   * definition's shape has no table worth counting. The readiness gate is repeated
   * here because a refused request 503s, and an error replaces this whole view with
   * the retry state, toggle included, leaving no way back but the browser. Readiness
   * comes from the shared poll, so if it changes under a projector whose counts are
   * already on, the next tick drops them and the view recovers on its own. */
  const [counts, setCounts] = useState(false)
  const ready = status?.projectors?.find((row) => row.name === name)?.readiness === 'ready'
  const showCounts = counts && ready
  const state = useResource(
    (signal) => api.projector(name, { counts: showCounts }, signal),
    [name, showCounts, tick],
  )
  const [confirming, setConfirming] = useState(false)

  /* Which entity is open, and where in it. All of it lives in the query string, so a
   * row someone found is a link they can send rather than a place they can only
   * describe. */
  const browsing = search?.get('entity') ?? null
  const field = search?.get('field') ?? ''
  const value = search?.get('value') ?? ''
  const cursor = search?.get('cursor') ?? null
  const openRow = search?.get('row') ?? null

  /* Only the provenance link needs this, and only one fact from it: which event types
   * carry a row's key as a tag. Fetched once rather than on the tick, because a
   * running process cannot change the declarations it was loaded with. */
  const schema = useResource((signal) => api.schema(signal), [])

  return html`
    <${Resource} state=${state}>
      ${(projector) => html`
        <section class="card">
          <header>
            <a
              href="/admin/projectors"
              onClick=${(clicked) => {
                clicked.preventDefault()
                go('/admin/projectors')
              }}
            >
              ← Projectors
            </a>
            <code style=${{ textTransform: 'none', letterSpacing: 0 }}>${projector.name}</code>
            <${Badge} kind="readiness" value=${projector.readiness} />
            <div style=${{ flex: 1 }}></div>
            <button
              type="button"
              class="btn"
              disabled=${!ready}
              onClick=${() => setCounts(!counts)}
              title=${ready
                ? 'a row count is a full table scan, so it is opt-in'
                : 'counts need a ready projector'}
            >
              ${showCounts ? '✓ counts' : '⟳ count rows'}
            </button>
            <button type="button" class="btn danger" onClick=${() => setConfirming(true)}>Replay</button>
          </header>
          <div class="body">
            <dl class="kv">
              <dt>sources</dt>
              <dd>${sources(projector.sources)}</dd>
              <dt>position</dt>
              <dd>${projector.position}</dd>
              <dt>lag</dt>
              <dd><${Lag} value=${projector.lag} /></dd>
              <dt>definition</dt>
              <dd>
                ${projector.definition_hash
                  ? shortHash(projector.definition_hash)
                  : html`<span class="faint">-</span>`}
                <span class="note">what the rows were built under</span>
              </dd>
              ${projector.last_error &&
              html`
                <dt>last error</dt>
                <dd style=${{ color: 'var(--err)' }}>${projector.last_error}</dd>
              `}
            </dl>
          </div>
        </section>

        ${projector.entities.map(
          (entity) => html`
            <${Entity}
              key=${entity.name}
              projector=${projector.name}
              entity=${entity}
              browsing=${browsing === entity.name}
              onBrowse=${() =>
                browse(browsing === entity.name ? {} : { entity: entity.name })}
            />
            ${browsing === entity.name &&
            html`
              <div class=${openRow === null ? undefined : 'with-detail'}>
                <${Rows}
                  key=${entity.name}
                  projector=${projector.name}
                  entity=${entity}
                  field=${field}
                  value=${value}
                  cursor=${cursor}
                  openRow=${openRow}
                />
                ${openRow !== null &&
                html`
                  <${RowDetail}
                    key=${openRow}
                    projector=${projector.name}
                    entity=${entity}
                    keyValue=${openRow}
                    projectorSources=${projector.sources}
                    schema=${schema.data}
                    onClose=${() => browse({ entity: entity.name, field, value, cursor })}
                  />
                `}
              </div>
            `}
          `,
        )}

        ${confirming &&
        html`
          <${Confirm}
            title="Replay"
            confirmWord=${name}
            danger=${true}
            onCancel=${() => setConfirming(false)}
            onConfirm=${async () => {
              await api.replay(name)
              setConfirming(false)
              refreshNow()
              state.reload()
            }}
          >
            <p>
              <code>${name}</code> will rebuild its read model from the start of the log and
              swap it in when it finishes.
            </p>
            <p>
              Reads keep being served from the current model while it rebuilds, so this is
              safe, but it re-folds the whole log and can take a long time on a large one.
            </p>
          <//>
        `}
      `}
    <//>
  `
}

function Entity({ projector, entity, browsing, onBrowse }) {
  const path = `/read/${projector}/${entity.name}`
  return html`
    <section class="card">
      <header>
        Entity
        <code style=${{ textTransform: 'none', letterSpacing: 0 }}>${entity.name}</code>
        <span class="tiny faint" style=${{ textTransform: 'none', letterSpacing: 0 }}>
          key ${entity.key} : ${entity.key_kind}
        </span>
        <div style=${{ flex: 1 }}></div>
        ${entity.rows !== null &&
        entity.rows !== undefined &&
        html`<span class="pill mute plain mono">${plural(entity.rows, 'row')}</span>`}
        <button type="button" class="btn" onClick=${onBrowse}>
          ${browsing ? '✓ rows' : 'Rows →'}
        </button>
      </header>
      <table class="data">
        <thead>
          <tr>
            <th scope="col">Field</th>
            <th scope="col">Kind</th>
            <th scope="col" style=${{ width: '70px' }}>Indexed</th>
            <th scope="col" style=${{ width: '70px' }}>Unique</th>
            <th scope="col" style=${{ width: '130px' }}>Subject</th>
          </tr>
        </thead>
        <tbody>
          ${entity.fields.map(
            (field) => html`
              <tr key=${field.name}>
                <td>
                  <code>${field.name}</code>
                  ${field.name === entity.key &&
                  html`<span class="note">key</span>`}
                </td>
                <td><span class="mono tiny dim">${field.kind}</span></td>
                <td>${field.indexed ? '✓' : html`<span class="faint">·</span>`}</td>
                <td>${field.unique ? '✓' : html`<span class="faint">·</span>`}</td>
                <td>
                  ${field.subject
                    ? html`<code class="tiny">${field.subject}</code>`
                    : html`<span class="faint">·</span>`}
                </td>
              </tr>
            `,
          )}
        </tbody>
      </table>
      <div class="body tiny dim">
        <div class="row wrap">
          <strong>Indexes</strong>
          ${entity.indexes.length === 0 && html`<span class="faint">none</span>`}
          ${entity.indexes.map(
            (index) => html`<code>${index.name} (${index.columns.join(', ')})</code>`,
          )}
        </div>
        <div class="row wrap" style=${{ marginTop: '8px' }}>
          <strong>Read API</strong>
          <a href=${path} target="_blank" rel="noreferrer"><code>GET ${path}</code></a>
          <span class="faint">
            filter on ${entity.filterable.join(', ') || 'nothing'}
          </span>
        </div>
      </div>
    </section>
  `
}

/* --- the rows ------------------------------------------------------------- */

/**
 * What a column that is not in the response means.
 *
 * Two different things drop a column, and the response cannot tell them apart:
 * `row_to_json` omits a SQL NULL, and the read API's `decrypt_row` omits a subject
 * column whose key it could not obtain. Which of them are *possible* is a fact about
 * the declaration, so that is what this reads. Calling every absence "absent" would
 * invent a mystery where only `null` can occur; drawing every absence as `null` would
 * hide an erasure, which is the one thing here worth noticing.
 */
function absence(field) {
  if (!field.subject) {
    return field.optional
      ? { label: 'null', tone: null, why: 'no value; the column is declared optional' }
      : {
          label: 'absent',
          tone: 'warn',
          why: 'the read model carries no value for this required column',
        }
  }
  const gone = `the key for \`${field.subject}\` cannot be obtained: erased, or this value was written under a superseded one`
  return {
    label: 'absent',
    tone: 'mute',
    why: field.optional ? `no value, or ${gone}` : gone,
  }
}

function Absent({ field }) {
  const { label, tone, why } = absence(field)
  if (tone === null) return html`<span class="faint" title=${why}>${label}</span>`
  return html`<span class=${`pill ${tone} plain`} title=${why}>${label}</span>`
}

/** One value in the table, abbreviated to fit a row. */
function Cell({ field, row }) {
  if (!Object.hasOwn(row, field.name)) return html`<${Absent} field=${field} />`
  const value = row[field.name]
  if (baseKind(field.kind) === 'Json') {
    const text = JSON.stringify(value)
    return html`<span class="mono tiny dim" title=${text}>${truncate(text, 48)}</span>`
  }
  if (typeof value === 'boolean' || typeof value === 'number') {
    return html`<span class="mono">${String(value)}</span>`
  }
  const text = String(value)
  return html`<span title=${text.length > 44 ? text : undefined}>${truncate(text, 44)}</span>`
}

/** The same value in the detail panel, whole. */
function Value({ field, row }) {
  if (!Object.hasOwn(row, field.name)) return html`<${Absent} field=${field} />`
  const value = row[field.name]
  if (baseKind(field.kind) === 'Json') return html`<${JsonTree} value=${value} />`
  return html`<span class="mono">${String(value)}</span>`
}

function Rows({ projector, entity, field, value, cursor, openRow }) {
  /* Deliberately not a dependant of the shared tick. A scan costs the process more
   * than a `/status` read does, and rows shifting under someone who is reading them is
   * worse than rows that are three seconds old. The ⟳ in this header is the refresh. */
  const state = useResource(
    (signal) => api.rows(projector, entity.name, { field, value, cursor, limit: PAGE }, signal),
    [projector, entity.name, field, value, cursor],
  )
  /* Where we came from, so "newer" can walk back: the read API's cursor is forward
   * only, so the console keeps its own trail rather than pretending otherwise. */
  const [trail, setTrail] = useState([])
  const filterable = new Set(entity.filterable)
  const [draftField, setDraftField] = useState(field || entity.filterable[0] || entity.key)
  const [draftValue, setDraftValue] = useState(value)

  const apply = () => {
    setTrail([])
    browse({ entity: entity.name, field: draftField, value: draftValue.trim() })
  }

  const clear = () => {
    setTrail([])
    setDraftValue('')
    browse({ entity: entity.name })
  }

  const older = () => {
    const next = state.data?.next_cursor
    if (next === null || next === undefined) return
    setTrail((current) => [...current, cursor ?? ''])
    browse({ entity: entity.name, field, value, cursor: next })
  }

  const newer = () => {
    const previous = trail[trail.length - 1]
    setTrail((current) => current.slice(0, -1))
    browse({ entity: entity.name, field, value, cursor: previous || null })
  }

  /* Key first, then the rest in declaration order: the key is what every other view
   * addresses a row by, so it is what the eye should land on. */
  const ordered = [
    ...entity.fields.filter((column) => column.name === entity.key),
    ...entity.fields.filter((column) => column.name !== entity.key),
  ]
  const columns = ordered.map((column) => ({
    key: column.name,
    header: column.name,
    align: numeric(column.kind) ? 'right' : undefined,
    render: (row) => html`<${Cell} field=${column} row=${row} />`,
  }))

  return html`
    <section class="card">
      <header>
        Rows
        <code style=${{ textTransform: 'none', letterSpacing: 0 }}>${entity.name}</code>
        <span class="tiny faint" style=${{ textTransform: 'none', letterSpacing: 0 }}>
          in ${entity.key} order
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
        <button
          type="button"
          class="btn icon"
          onClick=${() => browse({})}
          title="Close"
          aria-label="Close"
        >
          ✕
        </button>
      </header>

      <div class="filters">
        <div class="filter-input">
          <select
            value=${draftField}
            onChange=${(changed) => setDraftField(changed.target.value)}
            aria-label="Filter column"
          >
            ${entity.fields.map(
              (column) => html`
                <option
                  key=${column.name}
                  value=${column.name}
                  disabled=${!filterable.has(column.name)}
                >
                  ${column.name}${filterable.has(column.name) ? '' : ' — not indexed'}
                </option>
              `,
            )}
          </select>
          <input
            value=${draftValue}
            placeholder="value"
            onInput=${(typed) => setDraftValue(typed.target.value)}
            onKeyDown=${(pressed) => {
              if (pressed.key === 'Enter') {
                pressed.preventDefault()
                apply()
              }
            }}
            aria-label="Filter value"
            spellcheck="false"
            autocomplete="off"
          />
          <button type="button" class="btn" onClick=${apply} disabled=${!draftValue.trim()}>
            apply
          </button>
        </div>
        ${field &&
        html`
          <span class="chip">
            <span class="chip-kind">where</span>
            <code>${field} = ${value}</code>
            <button type="button" onClick=${clear} aria-label="Remove this filter">✕</button>
          </span>
        `}
        <span class="tiny faint">
          only the key and each index's leftmost column can be filtered; the rest would
          be a table scan, which the read API refuses
        </span>
      </div>

      ${state.error?.unavailable
        ? /* Not an error state. A projector that is rebuilding, stale or quarantined
           * cannot serve rows at the definition this page is describing, and the
           * server's own message already names the fix (Replay, for the two that need
           * an operator). Painting that red would say something broke. */
          html`<${Empty} title=${state.error.code}>${state.error.message}<//>`
        : html`
            <${Resource}
              state=${state}
              empty=${(data) =>
                data.items.length === 0
                  ? html`
                      <${Empty} title=${field ? 'No rows match' : 'No rows'}>
                        ${field
                          ? html`This entity holds nothing with <code>${field} = ${value}</code>.`
                          : cursor
                            ? 'You have paged past the last row.'
                            : 'The projector has folded nothing into this entity yet.'}
                      <//>
                    `
                  : null}
            >
              ${(data) => html`
                <${DataTable}
                  label=${`${entity.name} rows`}
                  columns=${columns}
                  rows=${data.items}
                  selected=${(row) => String(row[entity.key]) === openRow}
                  onOpen=${(row) =>
                    browse({
                      entity: entity.name,
                      field,
                      value,
                      cursor,
                      row: String(row[entity.key]),
                    })}
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

/**
 * Which of a projector's source event types can carry this row's key as a tag.
 *
 * Both conditions are load-bearing. Only an `indexed` field becomes a store tag at
 * all, and a subject-scoped field is tagged with its *ciphertext*, so its tag can
 * never be matched from a plaintext key. An event type failing either would produce a
 * confident link to an empty result, which is worse than no link.
 */
function provenance(entity, projectorSources, schema) {
  if (!schema) return []
  return projectorSources.filter((type) => {
    const def = schema.events.find((event) => event.type === type)
    return Boolean(
      def?.fields.some(
        (field) => field.name === entity.key && field.indexed && !field.subject,
      ),
    )
  })
}

/** The events view, filtered to one tag and the types that can carry it. */
function eventsHref(types, tag) {
  const search = new URLSearchParams()
  for (const type of types) search.append('type', type)
  search.append('tag', tag)
  return `/admin/events?${search.toString()}`
}

function RowDetail({ projector, entity, keyValue, projectorSources, schema, onClose }) {
  const state = useResource(
    (signal) => api.row(projector, entity.name, keyValue, signal),
    [projector, entity.name, keyValue],
  )
  const path = `/read/${projector}/${entity.name}/${keyValue}`
  const ordered = [
    ...entity.fields.filter((column) => column.name === entity.key),
    ...entity.fields.filter((column) => column.name !== entity.key),
  ]
  const types = provenance(entity, projectorSources, schema)
  /* `String(keyValue)` is the same rendering the runtime tagged the event with: a tag
   * value is `scalar_to_string` of the field, which for the string, number and boolean
   * forms a key takes is exactly this. */
  const events = eventsHref(types, `${entity.key}:${keyValue}`)

  return html`
    <${DetailPanel}
      title=${keyValue}
      subtitle=${entity.name}
      onClose=${onClose}
      actions=${state.data &&
      html`<${Copy} value=${JSON.stringify(state.data.item, null, 2)} title="Copy as JSON" />`}
    >
      <${Resource} state=${state}>
        ${(found) => html`
          <dl class="kv">
            ${ordered.map(
              (column) => html`
                <dt key=${column.name}>${column.name}</dt>
                <dd>
                  <${Value} field=${column} row=${found.item} />
                  ${column.name === entity.key && html`<span class="note">key</span>`}
                  ${column.subject &&
                  html`<span class="note">scoped to ${column.subject}</span>`}
                </dd>
              `,
            )}
          </dl>

          <h3 class="section-title">Provenance</h3>
          ${/* Withheld until the schema is in. "No event carries this key" is a claim,
              and making it in the moment before the answer arrives is one the page
              would retract on every open. */
          !schema
            ? html`<p class="tiny faint">…</p>`
            : types.length > 0
              ? html`
                  <div class="row wrap">
                    <a
                      href=${events}
                      onClick=${(clicked) => {
                        clicked.preventDefault()
                        go(events)
                      }}
                    >
                      events that built this row →
                    </a>
                    <span class="note">
                      ${sources(types)}, tagged <code>${entity.key}:${keyValue}</code>
                    </span>
                  </div>
                `
              : html`
                  <p class="tiny faint">
                    No source event of this projector declares <code>${entity.key}</code>
                    as an unscoped indexed field, so the log carries no tag to follow
                    from this row back to the events behind it.
                  </p>
                `}

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
