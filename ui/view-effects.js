/* Effects: the diagnosis screen.
 *
 * "My effect is stuck" is one request here. The journal lists every call an invocation
 * made and what came back, in order, and the first call *missing* from that list is
 * where it is wedged: calls already journaled replay from the journal rather than
 * re-firing, so the sequence is exactly what has succeeded so far.
 *
 * One thing the list cannot tell you and does not pretend to: a journaled call records
 * its result, never its arguments. Arguments are hashed only, so plaintext that came
 * out of `reveal()` cannot outlive the erasure of the subject it belonged to. */

import { html, useEffect, useState } from './vendor-preact.js'
import { api } from './api.js'
import { go } from './router.js'
import { refreshNow, useStatus } from './store.js'
import { Empty, Resource, useResource } from './ui-states.js'
import { DataTable } from './ui-table.js'
import { Badge, Lag, StatusCode } from './ui-badge.js'
import { JsonBlock } from './ui-json.js'
import { Confirm } from './ui-confirm.js'
import { Copy } from './ui-copy.js'
import { ago, clock, duration, shortHash, sources, stamp } from './format.js'

/** Counts down against `retry_in_ms`, which is a remaining duration precisely so the
 *  browser's clock never has to agree with the server's. */
function Countdown({ ms }) {
  const [remaining, setRemaining] = useState(ms)

  useEffect(() => {
    setRemaining(ms)
    if (ms === null || ms === undefined) return
    const timer = setInterval(() => {
      setRemaining((current) => (current === null ? null : Math.max(0, current - 1000)))
    }, 1000)
    return () => clearInterval(timer)
  }, [ms])

  if (remaining === null || remaining === undefined) return null
  return html`
    <span class="tiny dim mono" title="until the next attempt">
      retry in ${duration(remaining)}
    </span>
  `
}

export function EffectsView({ params }) {
  // Keyed by name so switching effects resets this view's own state (an open confirm
  // dialog, most importantly) while staying mounted across list-to-journal moves.
  if (params?.name)
    return html`<${EffectDetail} key=${params.name} name=${params.name} position=${params.position} />`
  return html`<${EffectList} />`
}

function EffectList() {
  const { tick } = useStatus()
  const listing = useResource((signal) => api.effects(signal), [tick])

  const columns = [
    {
      key: 'state',
      header: '',
      width: '110px',
      render: (effect) => html`<${Badge} kind="effect" value=${effect.state} />`,
    },
    { key: 'name', header: 'Name', render: (effect) => html`<code>${effect.name}</code>` },
    {
      key: 'position',
      header: 'Position',
      align: 'right',
      width: '110px',
      render: (effect) => html`<span class="mono">${effect.position}</span>`,
    },
    {
      key: 'lag',
      header: 'Lag',
      align: 'right',
      width: '80px',
      render: (effect) => html`<${Lag} value=${effect.lag} />`,
    },
    {
      key: 'failures',
      header: 'Fails',
      align: 'right',
      width: '70px',
      render: (effect) =>
        html`<span class=${effect.consecutive_failures ? 'mono' : 'mono faint'}>
          ${effect.consecutive_failures}
        </span>`,
    },
    {
      key: 'lanes',
      header: 'Lanes',
      align: 'right',
      width: '70px',
      render: (effect) =>
        html`<span class=${effect.wedged_lanes ? 'mono' : 'mono faint'}>
          ${effect.wedged_lanes}
        </span>`,
    },
    {
      key: 'error',
      header: 'Last error',
      clip: true,
      render: (effect) =>
        effect.last_error
          ? html`<span class="tiny" style=${{ color: 'var(--err)' }} title=${effect.last_error}>
              ${effect.last_error}
            </span>`
          : html`<span class="faint">-</span>`,
    },
    {
      key: 'retry',
      header: '',
      width: '120px',
      render: (effect) => html`<${Countdown} ms=${effect.retry_in_ms} />`,
    },
  ]

  return html`
    <section class="card">
      <header>Effects</header>
      <${Resource}
        state=${listing}
        empty=${(data) =>
          data.effects.length === 0
            ? html`<${Empty} title="No effects">This project declares none.<//>`
            : null}
      >
        ${(data) => {
          /* Broken first. An operator opening this page is looking for the problem,
           * not for an alphabetical inventory. */
          const order = { quarantined: 0, wedged: 1, lagging: 2, healthy: 3 }
          const rows = [...data.effects].sort(
            (a, b) => (order[a.state] ?? 9) - (order[b.state] ?? 9) || a.name.localeCompare(b.name),
          )
          return html`
            <${DataTable}
              label="Effects"
              columns=${columns}
              rows=${rows}
              onOpen=${(effect) => go(`/admin/effects/${encodeURIComponent(effect.name)}`)}
            />
          `
        }}
      <//>
    </section>
  `
}

function EffectDetail({ name, position }) {
  const { tick } = useStatus()
  const effect = useResource((signal) => api.effect(name, signal), [name, tick])
  const invocations = useResource((signal) => api.invocations(name, { limit: 30 }, signal), [name, tick])
  const [confirming, setConfirming] = useState(null)

  /* The position to skip is the one the runtime names as pinning the watermark, which is
   * the failing lane it also reports `last_error` for. Not "the newest running
   * invocation": lanes mean several run at once, and the newest is exactly the wrong one,
   * because skipping it would not release the prefix. */
  const stuck = effect.data?.pinning_position ?? null

  return html`
    <${Resource} state=${effect}>
      ${(detail) => html`
        <section class="card">
          <header>
            <a
              href="/admin/effects"
              onClick=${(clicked) => {
                clicked.preventDefault()
                go('/admin/effects')
              }}
            >
              ← Effects
            </a>
            <code style=${{ textTransform: 'none', letterSpacing: 0 }}>${detail.name}</code>
            <${Badge} kind="effect" value=${detail.state} />
            <${Countdown} ms=${detail.retry_in_ms} />
            <div style=${{ flex: 1 }}></div>
            ${stuck !== null &&
            detail.consecutive_failures > 0 &&
            html`
              <button type="button" class="btn danger" onClick=${() => setConfirming(stuck)}>
                Skip #${stuck}
              </button>
            `}
          </header>
          <div class="body">
            <dl class="kv">
              <dt>sources</dt>
              <dd>${sources(detail.sources)}</dd>

              <dt>position</dt>
              <dd>${detail.position} <span class="note">in memory</span></dd>

              <dt>watermark</dt>
              <dd>
                ${detail.watermark === null
                  ? html`<span class="faint">never ran</span>`
                  : detail.watermark}
                <span class="note">durable</span>
              </dd>

              <dt>lag</dt>
              <dd><${Lag} value=${detail.lag} /></dd>

              <dt>failures</dt>
              <dd>${detail.consecutive_failures}</dd>

              <dt>pinned by</dt>
              <dd>
                ${detail.pinning_key === null
                  ? html`<span class="faint">nothing in flight</span>`
                  : html`<code>${detail.pinning_key}</code>
                      <span class="note">at #${detail.pinning_position}</span>`}
              </dd>

              <dt>wedged lanes</dt>
              <dd>
                ${detail.wedged_lanes}
                <span class="note">
                  other lanes keep running; this one holds the watermark
                </span>
              </dd>

              <dt>terminal skips</dt>
              <dd>
                ${detail.terminal_skips}
                <span class="note">
                  since this process started; a restart resets it
                </span>
              </dd>

              ${detail.last_error &&
              html`
                <dt>last error</dt>
                <dd style=${{ color: 'var(--err)' }}>${detail.last_error}</dd>
              `}
              ${detail.last_terminal_error &&
              html`
                <dt>last terminal</dt>
                <dd style=${{ color: 'var(--warn)' }}>${detail.last_terminal_error}</dd>
              `}
              ${detail.quarantine &&
              html`
                <dt>quarantine</dt>
                <dd>
                  position ${detail.quarantine.position} · ${detail.quarantine.reason}
                  <span class="note">${stamp(detail.quarantine.at)}</span>
                </dd>
              `}
            </dl>
          </div>
        </section>

        <${StuckLanes}
          lanes=${detail.stuck_lanes}
          total=${detail.wedged_lanes}
          onSkip=${setConfirming}
        />

        <div class="split">
          <section class="card">
            <header>Invocations</header>
            <${Resource}
              state=${invocations}
              empty=${(data) =>
                data.invocations.length === 0
                  ? html`
                      <${Empty} title="No invocations recorded">
                        Either this effect has not run, or the retention sweeper has
                        reclaimed its completed invocations.
                      <//>
                    `
                  : null}
            >
              ${(data) => html`
                <${DataTable}
                  label="Invocations"
                  keyboard=${false}
                  columns=${[
                    {
                      key: 'position',
                      header: 'Pos',
                      align: 'right',
                      width: '80px',
                      render: (row) => html`<span class="mono">${row.position}</span>`,
                    },
                    {
                      key: 'status',
                      header: 'Status',
                      width: '100px',
                      render: (row) => html`<${Badge} kind="invocation" value=${row.status} />`,
                    },
                    {
                      key: 'created',
                      header: 'Started',
                      render: (row) => html`<span class="mono dim">${clock(row.created_at)}</span>`,
                    },
                    {
                      key: 'took',
                      header: 'Took',
                      align: 'right',
                      render: (row) =>
                        row.completed_at
                          ? html`<span class="mono dim">
                              ${duration(
                                new Date(row.completed_at).getTime() -
                                  new Date(row.created_at).getTime(),
                              )}
                            </span>`
                          : html`<span class="faint">-</span>`,
                    },
                  ]}
                  rows=${data.invocations}
                  selected=${(row) => String(row.position) === position}
                  onOpen=${(row) =>
                    go(
                      `/admin/effects/${encodeURIComponent(name)}/invocations/${row.position}`,
                    )}
                />
              `}
            <//>
          </section>

          ${position
            ? html`<${Journal} name=${name} position=${position} />`
            : html`
                <section class="card">
                  <${Empty} title="Pick an invocation">
                    Its journal lists every call it made and what came back. The first
                    call <em>missing</em> from that list is where it is wedged.
                  <//>
                </section>
              `}
        </div>

        ${confirming !== null &&
        confirming !== undefined &&
        html`
          <${Confirm}
            title=${`Skip #${confirming}`}
            confirmWord=${name}
            danger=${true}
            onCancel=${() => setConfirming(null)}
            onConfirm=${async () => {
              await api.skip(name, confirming)
              setConfirming(null)
              refreshNow()
              effect.reload()
              invocations.reload()
            }}
          >
            <p>
              <code>${name}</code> will advance past position ${confirming} without processing
              it. The event stays in the log; this effect simply never acts on it.
            </p>
            <p>
              This is not undoable and it is never automatic. Do it when the event is
              genuinely unprocessable, not to clear a transient failure the retry would
              have handled.
            </p>
          <//>
        `}
      `}
    <//>
  `
}

/* Every lane that is stuck, which is the half of "my effect is wedged" the log cannot
 * answer. A `fail` appends an event, so anything reading the log can list one; a wedge
 * appends nothing, so without this panel a person sees lag in the thousands and one
 * pinning key, and no way to learn that forty other shops are stuck behind their own
 * failures rather than behind that one.
 *
 * Skipping is per row, because the header's button can only ever name the pinning
 * position: clearing the second-worst lane used to mean clearing the worst first and
 * waiting for the list to re-derive. */
function StuckLanes({ lanes, total, onSkip }) {
  if (!lanes || lanes.length === 0) return null
  /* `total` is counted before the server caps the array, so a gap is lanes left out and
   * not lanes that cleared: the two are read under one lock. Saying "5 retrying" over a
   * page of 100 out of 500 is the one way this panel could mislead, since a list that
   * looks complete is read as complete. */
  const hidden = Math.max(0, (total ?? lanes.length) - lanes.length)

  return html`
    <section class="card">
      <header>
        Stuck lanes
        <span class="note">
          ${total ?? lanes.length} retrying; other lanes keep running
        </span>
        ${hidden > 0 &&
        html`<span class="note" style=${{ color: 'var(--warn)' }}>
          showing the ${lanes.length} worst; ${hidden} more not listed
        </span>`}
      </header>
      <${DataTable}
        label="Stuck lanes"
        keyboard=${false}
        columns=${[
          {
            key: 'lane',
            header: 'Lane',
            render: (row) => html`<code>${row.lane}</code>`,
          },
          {
            key: 'position',
            header: 'Pos',
            align: 'right',
            width: '80px',
            render: (row) => html`<span class="mono">${row.position}</span>`,
          },
          {
            key: 'attempt',
            header: 'Tries',
            align: 'right',
            width: '70px',
            render: (row) => html`<span class="mono">${row.attempt}</span>`,
          },
          {
            /* The same component the effect list uses for the same field, rather than a
             * `duration()` frozen until the next poll. Two tables in one view counting
             * the same value down at different rates is the kind of thing a person
             * notices and mistrusts. */
            key: 'retry',
            header: '',
            width: '120px',
            render: (row) => html`<${Countdown} ms=${row.retry_in_ms} />`,
          },
          {
            key: 'error',
            header: 'Error',
            clip: true,
            render: (row) => html`<span title=${row.error}>${row.error}</span>`,
          },
          {
            key: 'skip',
            header: '',
            width: '70px',
            render: (row) => html`
              <button
                type="button"
                class="btn danger tiny"
                onClick=${() => onSkip(row.position)}
              >
                Skip
              </button>
            `,
          },
        ]}
        rows=${lanes}
      />
    </section>
  `
}

function Journal({ name, position }) {
  const state = useResource(
    (signal) => api.invocation(name, position, { limit: 100 }, signal),
    [name, position],
  )
  const [open, setOpen] = useState(null)

  return html`
    <section class="card">
      <header>
        Invocation #${position}
        <div style=${{ flex: 1 }}></div>
        ${state.data && html`<${Badge} kind="invocation" value=${state.data.status} />`}
      </header>
      <${Resource}
        state=${state}
        empty=${(data) =>
          data.calls.length === 0
            ? html`
                <${Empty} title="Nothing journaled yet">
                  A retryable failure bails <em>before</em> the journal write, which is what
                  lets a retry re-send rather than replaying a call that never landed. An
                  empty list on a running invocation means it has not completed a single
                  call.
                <//>
              `
            : null}
      >
        ${(data) => html`
          <div class="body" style=${{ paddingBottom: 0 }}>
            <dl class="kv tiny">
              <dt>script</dt>
              <dd class="row">
                ${shortHash(data.script_hash)}<${Copy} value=${data.script_hash} />
              </dd>
              <dt>started</dt>
              <dd>${stamp(data.created_at)} <span class="note">${ago(data.created_at)}</span></dd>
              ${data.completed_at &&
              html`
                <dt>completed</dt>
                <dd>${stamp(data.completed_at)}</dd>
              `}
            </dl>
          </div>
          <table class="data">
            <thead>
              <tr>
                <th scope="col" class="num" style=${{ width: '50px' }}>Seq</th>
                <th scope="col" style=${{ width: '150px' }}>Kind</th>
                <th scope="col">Result</th>
                <th scope="col" style=${{ width: '40px' }}></th>
              </tr>
            </thead>
            <tbody>
              ${data.calls.map(
                (call) => html`
                  <tr key=${call.seq}>
                    <td class="num mono">${call.seq}</td>
                    <td><${Badge} kind="call" value=${call.kind} /></td>
                    <td>
                      ${typeof call.result?.status === 'number'
                        ? html`<${StatusCode} status=${call.result.status} />`
                        : html`<span class="tiny mono dim">${summarise(call.result)}</span>`}
                      ${call.disambiguator > 0 &&
                      html`<span class="note" title="a byte-identical repeat of an earlier call">
                        repeat ${call.disambiguator}
                      </span>`}
                    </td>
                    <td>
                      <button
                        type="button"
                        class="btn icon"
                        onClick=${() => setOpen(open === call.seq ? null : call.seq)}
                        aria-expanded=${open === call.seq}
                        aria-label="Show the recorded result"
                      >
                        ${open === call.seq ? '⌃' : '⌄'}
                      </button>
                    </td>
                  </tr>
                  ${open === call.seq &&
                  html`
                    <tr key=${`${call.seq}-detail`}>
                      <td colspan="4" style=${{ height: 'auto', padding: '10px 14px' }}>
                        <${JsonBlock} value=${call.result} />
                        <p class="tiny faint" style=${{ marginBottom: 0 }}>
                          The call's arguments are hashed, never stored: keeping them would let
                          plaintext from <code>reveal()</code> outlive the erasure of the subject
                          it belonged to.
                        </p>
                      </td>
                    </tr>
                  `}
                `,
              )}
            </tbody>
          </table>
          ${data.next_cursor !== null &&
          data.next_cursor !== undefined &&
          html`
            <p class="body tiny faint" style=${{ marginBottom: 0 }}>
              This invocation made more calls than one page holds, so the list above stops
              at ${data.calls.length}. A truncated call list must never read as the whole
              sequence, which is why the endpoint hands back a cursor rather than silently
              capping.
            </p>
          `}
        `}
      <//>
    </section>
  `
}

function summarise(result) {
  if (result === null || result === undefined) return '-'
  if (typeof result === 'string') return result.slice(0, 60)
  const text = JSON.stringify(result)
  return text.length > 60 ? `${text.slice(0, 60)}…` : text
}
