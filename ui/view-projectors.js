/* Projectors: readiness, lag, and the shape of what they materialised.
 *
 * The entity shapes come from the read model itself rather than from the project
 * source, so this is what the rows were actually built under, not what the current
 * `.star` file declares. On a stale or rebuilding projector those two differ, which is
 * exactly when you want to know. */

import { html, useState } from './vendor-preact.js'
import { api } from './api.js'
import { go } from './router.js'
import { refreshNow, useStatus } from './store.js'
import { Empty, Resource, useResource } from './ui-states.js'
import { DataTable } from './ui-table.js'
import { Badge, Lag } from './ui-badge.js'
import { Confirm } from './ui-confirm.js'
import { count, shortHash, sources } from './format.js'

export function ProjectorsView({ params }) {
  if (params?.name) return html`<${ProjectorDetail} key=${params.name} name=${params.name} />`
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

function ProjectorDetail({ name }) {
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

        ${projector.entities.map((entity) => html`<${Entity} key=${entity.name} projector=${projector.name} entity=${entity} />`)}

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

function Entity({ projector, entity }) {
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
        html`<span class="pill mute plain mono">${count(entity.rows)} rows</span>`}
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
