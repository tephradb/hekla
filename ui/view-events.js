/* Events: page the log, filter it, and read one event without leaving the list.
 *
 * The list fetches with `decrypt=false` and the detail panel fetches one event with
 * `decrypt=true`. That is not a performance choice. A decrypting request emits an
 * audit line on the server, so decrypting whole pages would bury the signal in noise
 * and unwrap keys for hundreds of fields nobody looked at. Fetching per event means
 * one audit line corresponds to one operator reading one event, which is what an audit
 * line should mean. The list needs no plaintext anyway: `subjects[].state` still comes
 * back, so the row can still say a field is there and encrypted. */

import { html, useState } from './vendor-preact.js'
import { api } from './api.js'
import { go, setQuery } from './router.js'
import { Resource, useResource, Empty } from './ui-states.js'
import { DataTable, Pager } from './ui-table.js'
import { Chips } from './ui-chips.js'
import { DetailPanel } from './ui-panel.js'
import { JsonTree } from './ui-json.js'
import { Badge } from './ui-badge.js'
import { Copy } from './ui-copy.js'
import { ago, clock, count, shortId, stamp } from './format.js'

const PAGE = 50

export function EventsView({ params, search }) {
  const types = search.getAll('type')
  const tags = search.getAll('tag')
  const cursor = search.get('cursor')
  /* Where we came from, so "newer" can walk back. The API's cursor is one-directional
   * per request, so the console keeps its own trail rather than pretending otherwise. */
  const [trail, setTrail] = useState([])

  const page = useResource(
    (signal) => api.events({ types, tags, cursor, limit: PAGE }, signal),
    [types.join('|'), tags.join('|'), cursor],
  )

  const setFilters = ({ types: nextTypes, tags: nextTags }) => {
    setTrail([])
    setQuery([
      ...nextTypes.map((type) => ['type', type]),
      ...nextTags.map((tag) => ['tag', tag]),
    ])
  }

  const older = () => {
    const next = page.data?.next_cursor
    if (next === null || next === undefined) return
    setTrail((current) => [...current, cursor ?? ''])
    setQuery([
      ...types.map((type) => ['type', type]),
      ...tags.map((tag) => ['tag', tag]),
      ['cursor', next],
    ])
  }

  const newer = () => {
    const previous = trail[trail.length - 1]
    setTrail((current) => current.slice(0, -1))
    setQuery([
      ...types.map((type) => ['type', type]),
      ...tags.map((tag) => ['tag', tag]),
      ['cursor', previous || ''],
    ])
  }

  /* Opening an event keeps the query for the same reason closing it does (see `close`):
   * the cursor lives in the URL, so navigating without it walks the list silently back
   * to the newest page while `trail` still says we are deep in the log, leaving "newer"
   * enabled over page one. */
  const openEvent = (event) =>
    go(`/admin/events/${event.position}` + window.location.search)

  const columns = [
    {
      key: 'position',
      header: 'Pos',
      width: '90px',
      align: 'right',
      render: (event) => html`<span class="mono">${event.position}</span>`,
    },
    {
      key: 'type',
      header: 'Type',
      render: (event) => html`
        <code>${event.type}</code>
        ${!event.declared &&
        html`<span
          class="pill warn"
          title="the log holds this type but the loaded project no longer declares it"
        >
          undeclared
        </span>`}
      `,
    },
    {
      key: 'when',
      header: 'When',
      width: '110px',
      render: (event) => html`<span class="mono dim">${clock(event.timestamp)}</span>`,
    },
    {
      key: 'tags',
      header: 'Tags',
      render: (event) => html`
        <span class="row wrap" style=${{ gap: '6px' }}>
          ${event.tags.slice(0, 3).map((tag) => html`<code class="tiny dim">${tag}</code>`)}
          ${event.tags.length > 3 &&
          html`<span class="tiny faint">+${event.tags.length - 3}</span>`}
          ${Object.keys(event.subjects ?? {}).length > 0 &&
          html`<span class="pill mute" title="this event carries subject-scoped fields">
            enc
          </span>`}
        </span>
      `,
    },
    {
      key: 'correlation',
      header: 'Correlation',
      width: '120px',
      render: (event) => html`
        <a
          class="mono tiny"
          href=${`/admin/traces/${event.correlation_id}`}
          onClick=${(clicked) => {
            clicked.preventDefault()
            clicked.stopPropagation()
            go(`/admin/traces/${event.correlation_id}`)
          }}
          title="follow this whole causal chain"
        >
          ${shortId(event.correlation_id)} →
        </a>
      `,
    },
  ]

  const open = params?.position

  return html`
    <div class=${open ? 'with-detail' : ''}>
      <section class="card">
        <header>
          Events
          <div style=${{ flex: 1 }}></div>
          <span class="tiny faint" style=${{ textTransform: 'none', letterSpacing: 0 }}>
            ${page.data ? `${count(page.data.log_head)} in log` : ''}
          </span>
        </header>
        <${Chips} types=${types} tags=${tags} onChange=${setFilters} />
        <${Resource}
          state=${page}
          empty=${(data) =>
            data.events.length === 0
              ? html`
                  <${Empty} title=${types.length || tags.length ? 'No events match' : 'The log is empty'}>
                    ${types.length || tags.length
                      ? html`
                          The log holds nothing that is
                          ${types.length
                            ? html`
                                <span>
                                  any of
                                  ${types.map((type) => html`<code>${type}</code> `)}
                                </span>
                              `
                            : ''}
                          ${types.length && tags.length ? ' and ' : ''}
                          ${tags.length
                            ? html`
                                <span>
                                  tagged with all of
                                  ${tags.map((tag) => html`<code>${tag}</code> `)}
                                </span>
                              `
                            : ''}
                        `
                      : 'Run a command and it will appear here.'}
                  <//>
                `
              : null}
        >
          ${(data) => html`
            <${DataTable}
              label="Event log"
              columns=${columns}
              rows=${data.events}
              selected=${(event) => String(event.position) === open}
              onOpen=${openEvent}
            />
            <${Pager}
              cursor=${data.next_cursor}
              canGoBack=${trail.length > 0}
              onOlder=${older}
              onNewer=${newer}
            />
          `}
        <//>
      </section>

      ${open && html`<${EventDetail} key=${open} position=${open} />`}
    </div>
  `
}

function EventDetail({ position }) {
  /* Defaults to decrypting, which is the decision recorded for this surface: the read
   * API already serves a projector's subject columns as plaintext over this same port,
   * so the boundary is not new. It is wider though, which is why it is per event and
   * why the server audits it. */
  const [decrypt, setDecrypt] = useState(true)
  const event = useResource((signal) => api.event(position, { decrypt }, signal), [position, decrypt])
  /* Back to the list with its filters intact: closing a detail view is not a reason to
   * lose the query someone built to find it. Concatenated rather than interpolated so
   * the path stays a plain literal, which is what `tests/ui.rs` checks against the
   * router's own table. */
  const close = () => go('/admin/events' + window.location.search)

  return html`
    <${DetailPanel}
      title=${`#${position}`}
      subtitle=${event.data?.type}
      onClose=${close}
      actions=${html`
        <label class="tiny row" style=${{ gap: '6px' }}>
          <input
            type="checkbox"
            checked=${decrypt}
            onChange=${(changed) => setDecrypt(changed.target.checked)}
          />
          decrypt
        </label>
        ${event.data && html`<${Copy} value=${JSON.stringify(event.data, null, 2)} title="Copy as JSON" />`}
      `}
    >
      <${Resource} state=${event}>
        ${(found) => html`
          <dl class="kv">
            <dt>event_id</dt>
            <dd class="row">
              <span>${found.event_id}</span><${Copy} value=${found.event_id} />
            </dd>

            <dt>correlation</dt>
            <dd class="row">
              <a href=${`/admin/traces/${found.correlation_id}`} onClick=${(clicked) => {
                clicked.preventDefault()
                go(`/admin/traces/${found.correlation_id}`)
              }}>
                ${found.correlation_id}
              </a>
              <span class="note">→ trace</span>
            </dd>

            <dt>causation</dt>
            <dd>${found.causation_id}</dd>

            ${found.triggering_event_id !== undefined &&
            html`
              <dt>triggered by</dt>
              <dd title="an effect produced this while processing another event">
                ${found.triggering_event_id}
              </dd>
            `}

            <dt>timestamp</dt>
            <dd>${stamp(found.timestamp)} <span class="note">${ago(found.timestamp)}</span></dd>

            <dt>declared</dt>
            <dd>
              ${found.declared
                ? html`<span class="pill ok">yes</span>`
                : html`<span class="pill warn" title="the project no longer declares this type">no</span>`}
            </dd>
          </dl>

          <h3 class="section-title">Data</h3>
          <${JsonTree} value=${found.data} subjects=${found.subjects} />

          ${Object.keys(found.subjects ?? {}).length > 0 &&
          html`
            <h3 class="section-title">Subject fields</h3>
            <dl class="kv">
              ${Object.entries(found.subjects).map(
                ([field, info]) => html`
                  <dt>${field}</dt>
                  <dd class="row">
                    <${Badge} kind="subject" value=${info.state} />
                    <span class="tiny faint">
                      scoped to ${info.subject}=${info.subject_value ?? '?'}
                    </span>
                  </dd>
                `,
              )}
            </dl>
          `}

          <h3 class="section-title">Tags</h3>
          <div class="row wrap">
            ${found.tags.length === 0 && html`<span class="tiny faint">none</span>`}
            ${found.tags.map((tag) => html`<code class="tiny">${tag}</code>`)}
          </div>

          ${found.hekla_tags.length > 0 &&
          html`
            <h3 class="section-title">
              Reserved
              <span class="note">
                stamped by the runtime, never by an author
              </span>
            </h3>
            <div class="row wrap">
              ${found.hekla_tags.map((tag) => html`<code class="tiny dim">${tag}</code>`)}
            </div>
          `}
        `}
      <//>
    <//>
  `
}
