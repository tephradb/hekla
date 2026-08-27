/* Trace: one correlation id, as the causal tree it actually is.
 *
 * The endpoint returns a flat list in log order plus the invocations that ran over
 * those positions. The tree is built here from `causation_id`, and the effect
 * attribution comes from the join rather than from guessing: an event's envelope
 * records *that* an effect produced it (`triggering_event_id`) but never which one,
 * and the journal is keyed by effect and position, so the server answers it exactly.
 *
 * One thing worth reading carefully, because getting it backwards draws the graph
 * upside down: an invocation's `position` is the position of the event that
 * *triggered* it, not of an event it appended. So `invocations` at position 1 means
 * "an effect ran because of event 1", and the events that effect wrote are further
 * down the chain. */

import { html } from './vendor-preact.js'
import { api } from './api.js'
import { go } from './router.js'
import { Empty, Resource, useResource } from './ui-states.js'
import { Badge } from './ui-badge.js'
import { Copy } from './ui-copy.js'
import { clock, offset, shortId } from './format.js'

const PAGE = 200

/** Build the causation forest. Anything whose parent is not on this page is a root. */
function tree(events) {
  const byEventId = new Map(events.map((event) => [event.event_id, event]))
  const children = new Map()
  const roots = []

  for (const event of events) {
    const parentId = event.triggering_event_id
    const parent = parentId ? byEventId.get(parentId) : undefined
    if (parent) {
      if (!children.has(parent.position)) children.set(parent.position, [])
      children.get(parent.position).push(event)
    } else {
      roots.push(event)
    }
  }
  return { roots, children }
}

function Node({ event, children, invocations, start, depth }) {
  const ran = invocations.filter((invocation) => invocation.position === event.position)
  const kids = children.get(event.position) ?? []
  const elapsed = new Date(event.timestamp).getTime() - start

  return html`
    <li class="trace-node">
      <div class="trace-row">
        <span class="trace-dot" aria-hidden="true"></span>
        <a
          class="mono"
          href=${`/admin/events/${event.position}`}
          onClick=${(clicked) => {
            clicked.preventDefault()
            go(`/admin/events/${event.position}`)
          }}
        >
          #${event.position}
        </a>
        <code>${event.type}</code>
        <div style=${{ flex: 1 }}></div>
        <span class="tiny faint mono">${clock(event.timestamp)}</span>
        <span class="tiny dim mono" style=${{ width: '72px', textAlign: 'right' }}>
          ${offset(elapsed)}
        </span>
      </div>

      ${ran.length > 0 &&
      html`
        <div class="trace-meta">
          ${ran.map(
            (invocation) => html`
              <a
                class="tiny"
                href=${`/admin/effects/${encodeURIComponent(invocation.effect)}/invocations/${invocation.position}`}
                onClick=${(clicked) => {
                  clicked.preventDefault()
                  go(
                    `/admin/effects/${encodeURIComponent(invocation.effect)}/invocations/${invocation.position}`,
                  )
                }}
                title="an effect ran because of this event; open its journal"
              >
                ⚡ ${invocation.effect}
              </a>
              <${Badge} kind="invocation" value=${invocation.status} />
            `,
          )}
        </div>
      `}

      ${kids.length > 0 &&
      html`
        <ul class="trace-children">
          ${kids.map(
            (child) => html`
              <${Node}
                key=${child.position}
                event=${child}
                children=${children}
                invocations=${invocations}
                start=${start}
                depth=${depth + 1}
              />
            `,
          )}
        </ul>
      `}
    </li>
  `
}

export function TraceView({ params }) {
  const id = params.correlation_id
  const trace = useResource((signal) => api.trace(id, { limit: PAGE }, signal), [id])

  return html`
    <${Resource}
      state=${trace}
      empty=${(data) =>
        data.events.length === 0
          ? html`
              <${Empty} title="No events for this correlation">
                Either nothing was appended under <code class="mono">${shortId(id)}</code>, or the
                events predate the reserved correlation tag. Tracing works on events appended by a
                version of hekla that stamps it; older ones carry nothing to find them by.
              <//>
            `
          : null}
    >
      ${(data) => {
        const { roots, children } = tree(data.events)
        const start = new Date(data.events[0].timestamp).getTime()
        const span =
          new Date(data.events[data.events.length - 1].timestamp).getTime() - start
        return html`
          <section class="card">
            <header>
              Trace
              <code class="mono" style=${{ textTransform: 'none', letterSpacing: 0 }}>
                ${data.correlation_id}
              </code>
              <${Copy} value=${data.correlation_id} />
              <div style=${{ flex: 1 }}></div>
              <span class="tiny faint" style=${{ textTransform: 'none', letterSpacing: 0 }}>
                ${data.events.length} events · ${offset(span)} span
                ${!data.complete ? ' · truncated' : ''}
              </span>
            </header>
            <div class="body">
              <ul class="trace">
                ${roots.map(
                  (event) => html`
                    <${Node}
                      key=${event.position}
                      event=${event}
                      children=${children}
                      invocations=${data.invocations}
                      start=${start}
                      depth=${0}
                    />
                  `,
                )}
              </ul>

              ${!data.complete &&
              html`
                <p class="tiny" style=${{ color: 'var(--warn)' }}>
                  This chain is longer than one page, so the tree above is a prefix of it.
                  A partially read causal chain is worse than one read whole, which is why
                  the endpoint says so outright rather than letting the count imply it.
                </p>
              `}
            </div>
          </section>
        `
      }}
    <//>
  `
}
