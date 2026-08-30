/* Overview: what is wrong, and what just happened.
 *
 * Ordered by what an operator opens the console to find out. Anything broken is at the
 * top; the log tail is at the bottom. A page that led with a healthy inventory would
 * bury the one line that matters. */

import { html } from './vendor-preact.js'
import { api } from './api.js'
import { go } from './router.js'
import { useStatus } from './store.js'
import { Empty, Resource, useResource, Skeleton } from './ui-states.js'
import { Badge, Lag } from './ui-badge.js'
import { Sparkline, bucket } from './ui-sparkline.js'
import { clock, count, duration, shortId } from './format.js'

const WINDOW_MS = 5 * 60 * 1000
const RECENT = 50

export function OverviewView() {
  const { status, tick } = useStatus()
  const recent = useResource((signal) => api.events({ limit: RECENT }, signal), [tick])

  if (!status) return html`<${Skeleton} rows=${6} />`

  const broken = [
    ...status.effects
      .filter((effect) => effect.state === 'wedged' || effect.state === 'quarantined')
      .map((effect) => ({
        kind: 'effect',
        name: effect.name,
        state: effect.state,
        detail: effect.last_error,
        href: `/admin/effects/${encodeURIComponent(effect.name)}`,
      })),
    ...status.projectors
      .filter((projector) => projector.readiness !== 'ready')
      .map((projector) => ({
        kind: 'projector',
        name: projector.name,
        state: projector.readiness,
        detail: projector.last_error,
        href: `/admin/projectors/${encodeURIComponent(projector.name)}`,
      })),
  ]

  /* Bucketed from the timestamps on one page of events, not from a metrics endpoint,
   * because hekla has none. It describes the recent tail of the log and says so. */
  const timestamps = (recent.data?.events ?? []).map((event) => event.timestamp)
  const series = bucket(timestamps, WINDOW_MS)
  const inWindow = series.reduce((total, value) => total + value, 0)

  return html`
    ${broken.length > 0 &&
    html`
      <section class="card" style=${{ borderColor: 'var(--err)' }}>
        <header style=${{ color: 'var(--err)' }}>Needs attention</header>
        <div class="body">
          ${broken.map(
            (item) => html`
              <div class="row wrap attention" key=${`${item.kind}-${item.name}`}>
                <${Badge} kind=${item.kind === 'effect' ? 'effect' : 'readiness'} value=${item.state} />
                <a
                  href=${item.href}
                  onClick=${(clicked) => {
                    clicked.preventDefault()
                    go(item.href)
                  }}
                >
                  <code>${item.name}</code>
                </a>
                <span class="tiny faint">${item.kind}</span>
                ${item.detail && html`<span class="tiny" style=${{ color: 'var(--err)' }}>${item.detail}</span>`}
              </div>
            `,
          )}
        </div>
      </section>
    `}

    <div class="grid">
      <section class="card">
        <header>Log</header>
        <div class="body">
          <div class="stat">${count(status.log_head)}</div>
          <div class="tiny faint">events appended</div>
          <div class="spark-wrap">
            <${Sparkline} values=${series} label=${`${inWindow} events in the last 5 minutes`} />
            <span class="tiny faint">
              ${inWindow} in the last 5m
              <span
                class="note"
                title="bucketed in the browser from the last ${RECENT} events; hekla has no metrics endpoint"
              >
                · from the last ${RECENT}
              </span>
            </span>
          </div>
        </div>
      </section>

      <section class="card">
        <header>Projectors</header>
        <div class="body module-list">
          ${status.projectors.length === 0 && html`<span class="tiny faint">none</span>`}
          ${status.projectors.map(
            (projector) => html`
              <div class="row" key=${projector.name}>
                <${Badge} kind="readiness" value=${projector.readiness} />
                <a
                  href=${`/admin/projectors/${encodeURIComponent(projector.name)}`}
                  onClick=${(clicked) => {
                    clicked.preventDefault()
                    go(`/admin/projectors/${encodeURIComponent(projector.name)}`)
                  }}
                >
                  <code>${projector.name}</code>
                </a>
                <div style=${{ flex: 1 }}></div>
                <${Lag} value=${projector.lag} />
              </div>
            `,
          )}
        </div>
      </section>

      <section class="card">
        <header>Effects</header>
        <div class="body module-list">
          ${status.effects.length === 0 && html`<span class="tiny faint">none</span>`}
          ${status.effects.map(
            (effect) => html`
              <div class="row" key=${effect.name}>
                <${Badge} kind="effect" value=${effect.state} />
                <a
                  href=${`/admin/effects/${encodeURIComponent(effect.name)}`}
                  onClick=${(clicked) => {
                    clicked.preventDefault()
                    go(`/admin/effects/${encodeURIComponent(effect.name)}`)
                  }}
                >
                  <code>${effect.name}</code>
                </a>
                <div style=${{ flex: 1 }}></div>
                <${Lag} value=${effect.lag} />
              </div>
            `,
          )}
        </div>
      </section>

      <section class="card">
        <header>Process</header>
        <div class="body">
          <dl class="kv">
            <dt>uptime</dt>
            <dd>${duration(status.uptime_seconds * 1000)}</dd>
            <dt>commands</dt>
            <dd>
              ${status.commands.public.length} public
              <span class="note">· ${status.commands.internal.length} internal</span>
            </dd>
            <dt>event types</dt>
            <dd>${status.events}</dd>
            <dt>verify</dt>
            <dd>${status.verify ? 'on' : 'off'}</dd>
          </dl>
        </div>
      </section>
    </div>

    <section class="card">
      <header>
        Recent
        <div style=${{ flex: 1 }}></div>
        <a
          href="/admin/events"
          style=${{ textTransform: 'none', letterSpacing: 0 }}
          onClick=${(clicked) => {
            clicked.preventDefault()
            go('/admin/events')
          }}
        >
          all events →
        </a>
      </header>
      <${Resource}
        state=${recent}
        empty=${(data) =>
          data.events.length === 0
            ? html`
                <${Empty} title="The log is empty">
                  Run a command and it will appear here.
                <//>
              `
            : null}
      >
        ${(data) => html`
          <table class="data">
            <tbody>
              ${data.events.slice(0, 10).map(
                (event) => html`
                  <tr
                    key=${event.position}
                    class="clickable"
                    onClick=${() => go(`/admin/events/${event.position}`)}
                  >
                    <td class="num mono" style=${{ width: '90px' }}>${event.position}</td>
                    <td><code>${event.type}</code></td>
                    <td class="mono dim" style=${{ width: '110px' }}>${clock(event.timestamp)}</td>
                    <td class="tiny dim">${event.tags.slice(0, 2).join(' · ')}</td>
                    <td class="mono tiny faint" style=${{ width: '110px' }}>
                      ${shortId(event.correlation_id)}
                    </td>
                  </tr>
                `,
              )}
            </tbody>
          </table>
        `}
      <//>
    </section>
  `
}
