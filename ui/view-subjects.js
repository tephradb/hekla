/* Subjects: which subjects still hold key material.
 *
 * Only live keys are listed, and that is not an omission. Erasure deletes the key row,
 * so an erased subject has nothing left to list; "absent" and "never existed" are the
 * same state and the API says so. The lookup is therefore the only way to ask about a
 * specific subject, and answering "absent" is the point of it.
 *
 * Key material itself never leaves the server. This shows which master wrapped a key
 * and when it was created, never the key. */

import { html, useState } from './vendor-preact.js'
import { api } from './api.js'
import { go } from './router.js'
import { Empty, Resource, useResource } from './ui-states.js'
import { DataTable } from './ui-table.js'
import { ago, count, shortHash, stamp } from './format.js'

const PAGE = 100

export function SubjectsView({ params }) {
  return html`
    <${Lookup} field=${params?.field} value=${params?.value} />
    <${Inventory} />
  `
}

function Lookup({ field, value }) {
  const [draftField, setDraftField] = useState(field ?? '')
  const [draftValue, setDraftValue] = useState(value ?? '')
  const answer = useResource(
    (signal) => (field && value ? api.subject(field, value, signal) : Promise.resolve(null)),
    [field, value],
  )

  const submit = (event) => {
    event.preventDefault()
    if (!draftField.trim() || !draftValue.trim()) return
    go(
      `/admin/subjects/${encodeURIComponent(draftField.trim())}/${encodeURIComponent(draftValue.trim())}`,
    )
  }

  return html`
    <section class="card">
      <header>Look up a subject</header>
      <div class="body">
        <form class="row wrap" onSubmit=${submit}>
          <input
            value=${draftField}
            placeholder="customer_id"
            onInput=${(event) => setDraftField(event.target.value)}
            aria-label="Subject field"
            spellcheck="false"
          />
          <input
            value=${draftValue}
            placeholder="c-42"
            onInput=${(event) => setDraftValue(event.target.value)}
            aria-label="Subject value"
            spellcheck="false"
          />
          <button class="btn primary" type="submit">Check</button>
        </form>

        ${field &&
        value &&
        html`
          <${Resource} state=${answer}>
            ${(found) =>
              found &&
              html`
                <p class="row" style=${{ marginBottom: 0 }}>
                  <code>${found.subject_field}=${found.subject_value}</code>
                  ${found.state === 'live'
                    ? html`<span class="pill ok">live</span>`
                    : html`<span class="pill err">absent</span>`}
                  <span class="tiny faint">
                    ${found.state === 'live'
                      ? 'a key exists, so this subject’s fields can still be read'
                      : 'no key: erased, or there never was one. The two are the same state.'}
                  </span>
                </p>
              `}
          <//>
        `}
      </div>
    </section>
  `
}

function Inventory() {
  const [after, setAfter] = useState(null)
  const [trail, setTrail] = useState([])
  const page = useResource(
    (signal) =>
      api.subjects(
        { afterField: after?.after_field, afterValue: after?.after_value, limit: PAGE },
        signal,
      ),
    [after?.after_field, after?.after_value],
  )

  const columns = [
    {
      key: 'field',
      header: 'Subject',
      render: (row) => html`<code>${row.subject_field}</code>`,
    },
    {
      key: 'value',
      header: 'Value',
      render: (row) => html`<span class="mono">${row.subject_value}</span>`,
    },
    {
      key: 'master',
      header: 'Master key',
      render: (row) => html`<span class="mono tiny dim">${shortHash(row.master_key_id)}</span>`,
    },
    {
      key: 'created',
      header: 'Created',
      render: (row) =>
        html`<span class="tiny dim" title=${stamp(row.created_at)}>${ago(row.created_at)}</span>`,
    },
    {
      key: 'events',
      header: '',
      width: '110px',
      render: (row) => html`
        <a
          class="tiny"
          href=${`/admin/events?tag=${encodeURIComponent(`${row.subject_field}:${row.subject_value}`)}`}
          onClick=${(clicked) => {
            clicked.preventDefault()
            go(
              `/admin/events?tag=${encodeURIComponent(`${row.subject_field}:${row.subject_value}`)}`,
            )
          }}
        >
          events →
        </a>
      `,
    },
  ]

  return html`
    <section class="card">
      <header>
        Subject keys
        <div style=${{ flex: 1 }}></div>
        ${page.data?.counts &&
        html`
          <span class="tiny faint row" style=${{ textTransform: 'none', letterSpacing: 0 }}>
            ${page.data.counts.map(
              (entry) => html`<span><code>${entry.subject_field}</code> ${count(entry.live_keys)}</span>`,
            )}
          </span>
        `}
      </header>
      <${Resource}
        state=${page}
        empty=${(data) =>
          data.subjects.length === 0
            ? html`
                <${Empty} title="No subject keys">
                  Either this project declares no subject-scoped fields, or no event has
                  been appended that would create a key.
                <//>
              `
            : null}
      >
        ${(data) => html`
          <${DataTable} label="Subject keys" columns=${columns} rows=${data.subjects} />
          ${(data.next || trail.length > 0) &&
          html`
            <div class="row" style=${{ justifyContent: 'flex-end', padding: '10px 14px' }}>
              <button
                type="button"
                class="btn"
                disabled=${trail.length === 0}
                onClick=${() => {
                  setAfter(trail[trail.length - 1] ?? null)
                  setTrail((current) => current.slice(0, -1))
                }}
              >
                ← back
              </button>
              <button
                type="button"
                class="btn"
                disabled=${!data.next}
                onClick=${() => {
                  setTrail((current) => [...current, after])
                  setAfter(data.next)
                }}
              >
                more →
              </button>
            </div>
          `}
          <p class="body tiny faint" style=${{ marginTop: 0 }}>
            Live keys only. An erased subject has no row here, so use the lookup above to
            confirm one is gone. Key material is never served.
          </p>
        `}
      <//>
    </section>
  `
}
