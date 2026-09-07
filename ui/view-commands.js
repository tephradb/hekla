/* Commands: the one thing in this console that makes something happen.
 *
 * Everything else here reads. This posts to `/commands/{name}`, which is the
 * application's own front door rather than operator surgery, so it is a button and not
 * a typed confirmation: the console is exactly as powerful here as `curl` against the
 * same port, and a dialog per run would make the loop this exists for unusable.
 *
 * The form is generated from the declaration, and that is worth more than a `curl`
 * snippet for one reason: it encodes the wire rules structurally. A `Money(n)` leaves
 * as a decimal string and never as a JSON number, a `Timestamp` carries its offset, and
 * an `Int` past 2^53 is not rounded on the way out. Those are the three that are easy
 * to get wrong by hand, and a generated body cannot get them wrong at all. */

import { html, useState } from './vendor-preact.js'
import { api } from './api.js'
import { go } from './router.js'
import { Empty, Resource, useResource } from './ui-states.js'
import { DataTable } from './ui-table.js'
import { DetailPanel } from './ui-panel.js'
import { Copy } from './ui-copy.js'
import { baseKind, enumVariants, maxLength } from './kinds.js'
import { count, plural } from './format.js'

export function CommandsView({ params }) {
  if (params?.name) return html`<${CommandDetail} key=${params.name} name=${params.name} />`
  return html`<${CommandList} />`
}

/** `user_id: Uuid, email: String @max(200)`, the same summary the schema view renders. */
function signature(command) {
  return command.input.map((field) => `${field.name}: ${field.kind}`).join(', ') || '-'
}

function CommandList() {
  const state = useResource((signal) => api.commands(signal), [])

  const columns = [
    {
      key: 'routed',
      header: '',
      width: '110px',
      render: (row) =>
        row.internal
          ? html`<span class="pill mute" title="not routed over HTTP; only an effect can invoke it">
              internal
            </span>`
          : html`<span class="pill ok">public</span>`,
    },
    { key: 'name', header: 'Name', render: (row) => html`<code>${row.name}</code>` },
    {
      key: 'input',
      header: 'Parameters',
      render: (row) => html`<span class="tiny dim mono">${signature(row)}</span>`,
    },
    {
      key: 'path',
      header: 'Declared in',
      width: '200px',
      render: (row) => html`<span class="tiny dim mono">${row.path}</span>`,
    },
  ]

  return html`
    <section class="card">
      <header>
        Commands
        <div style=${{ flex: 1 }}></div>
        <span class="tiny faint" style=${{ textTransform: 'none', letterSpacing: 0 }}>
          posting one appends events
        </span>
      </header>
      <${Resource}
        state=${state}
        empty=${(data) =>
          data.commands.length === 0
            ? html`<${Empty} title="No commands">This project declares none.<//>`
            : null}
      >
        ${(data) => html`
          <${DataTable}
            label="Commands"
            columns=${columns}
            rows=${data.commands}
            onOpen=${(row) => go(`/admin/commands/${encodeURIComponent(row.name)}`)}
          />
        `}
      <//>
    </section>
  `
}

function CommandDetail({ name }) {
  const state = useResource((signal) => api.command(name, signal), [name])
  return html`
    <${Resource} state=${state}>
      ${(command) => html`<${Runner} key=${command.name} command=${command} />`}
    <//>
  `
}

/* --- the body ------------------------------------------------------------- */

/** The form's starting state: a checkbox is off, and everything else is empty. */
function blankValues(input) {
  const values = {}
  for (const field of input) {
    const variants = enumVariants(field.kind)
    if (baseKind(field.kind) === 'Bool') {
      values[field.name] = false
    } else if (variants && !field.optional) {
      /* A required enum has no `(omit)` option, so an empty default selects nothing:
       * the control renders blank and posts `""`. Its first variant is the only
       * startable value. Not necessarily the enum's `@default` (the kind carries the
       * variants and not which one that is), but it is always a legal one. */
      values[field.name] = variants[0]
    } else {
      values[field.name] = ''
    }
  }
  return values
}

/**
 * The request body, as JSON **text** rather than an object.
 *
 * Text because a field's wire form is decided per kind and two of them cannot survive
 * a round trip through a JS value: an `Int` past 2^53 loses digits, and a `Money`
 * turned into a number is a float, which is the one thing money must never be. Emitting
 * each field as an already-correct JSON fragment settles both at the point the rule is
 * known, and it makes the JSON tab literally the bytes that get posted.
 *
 * An empty input omits the field, which is one of the two forms the API documents for
 * `T?` (the other being an explicit null). A *required* string-shaped field is sent as
 * `""` instead, so the command's own guard gets to answer rather than the parser.
 */
function buildBody(input, values) {
  const errors = {}
  const parts = []
  for (const field of input) {
    const kind = baseKind(field.kind)
    const raw = values[field.name]

    if (kind === 'Bool') {
      parts.push([field.name, raw === true ? 'true' : 'false'])
      continue
    }
    const text = String(raw ?? '')
    if (kind === 'Int') {
      if (text.trim() === '') continue
      if (!/^-?\d+$/.test(text.trim())) {
        errors[field.name] = 'an Int is digits, optionally signed'
        continue
      }
      // Spliced verbatim, never through `Number`: that is the whole point of building
      // text here.
      parts.push([field.name, text.trim()])
      continue
    }
    if (kind === 'Json') {
      if (text.trim() === '') continue
      try {
        JSON.parse(text)
      } catch (err) {
        errors[field.name] = err.message
        continue
      }
      parts.push([field.name, text.trim()])
      continue
    }
    /* String, Uuid, Timestamp, Money and every enum are strings on the wire. */
    if (text === '' && field.optional) continue
    parts.push([field.name, JSON.stringify(text)])
  }
  const body =
    parts.length === 0
      ? '{}'
      : `{\n${parts.map(([name, value]) => `  ${JSON.stringify(name)}: ${value}`).join(',\n')}\n}`
  return { body, errors }
}

/* --- the runner ----------------------------------------------------------- */

function Runner({ command }) {
  const [values, setValues] = useState(() => blankValues(command.input))
  /* `form` builds the body from the controls; `json` posts whatever is in the box. The
   * box is re-seeded from the form on every switch into it, so it always opens showing
   * what the form built, and an edit made there is what goes out. */
  const [mode, setMode] = useState('form')
  const [draft, setDraft] = useState('{}')
  const [idempotencyKey, setIdempotencyKey] = useState('')
  const [correlationId, setCorrelationId] = useState('')
  const [running, setRunning] = useState(false)
  const [outcome, setOutcome] = useState(null)

  const built = buildBody(command.input, values)
  const invalid = Object.keys(built.errors).length > 0
  const body = mode === 'json' ? draft : built.body

  const toJson = () => {
    setDraft(built.body)
    setMode('json')
  }

  const run = async () => {
    if (mode === 'form' && invalid) return
    if (mode === 'json') {
      try {
        const parsed = JSON.parse(draft)
        if (parsed === null || typeof parsed !== 'object' || Array.isArray(parsed)) {
          throw new Error('the body must be a JSON object')
        }
      } catch (err) {
        setOutcome({ ok: false, local: err.message })
        return
      }
    }
    setRunning(true)
    try {
      const data = await api.run(command.name, body, { idempotencyKey, correlationId })
      setOutcome({ ok: true, data })
    } catch (err) {
      setOutcome({ ok: false, error: err })
    }
    setRunning(false)
  }

  if (command.internal) {
    return html`<${Head} command=${command}>
      <${Empty} title="Not routed">
        <code>${command.name}</code> is declared under <code>commands/internal/</code>,
        so <code>POST /commands/${command.name}</code> answers 404. An effect reaches it
        through <code>invoke_command</code>; nothing outside the process can. Its
        parameters are below because an operator reading an effect's journal still needs
        to know its shape.
      <//>
      <${Parameters} command=${command} />
    <//>`
  }

  return html`
    <div class=${outcome ? 'with-detail' : undefined}>
      <${Head} command=${command}>
        <div class="filters">
          <button
            type="button"
            class=${`btn${mode === 'form' ? ' primary' : ''}`}
            onClick=${() => setMode('form')}
          >
            Form
          </button>
          <button
            type="button"
            class=${`btn${mode === 'json' ? ' primary' : ''}`}
            onClick=${toJson}
          >
            JSON
          </button>
          <div style=${{ flex: 1 }}></div>
          <span class="tiny faint">
            ${mode === 'form'
              ? `${plural(command.input.length, 'parameter')}, as declared`
              : 'seeded from the form on every switch; what is here is what gets posted'}
          </span>
        </div>

        ${mode === 'form'
          ? html`<${Form} command=${command} values=${values} errors=${built.errors} onChange=${setValues} />`
          : html`
              <div class="body">
                <textarea
                  class="cmd-json"
                  value=${draft}
                  rows=${Math.max(6, draft.split('\n').length + 1)}
                  onInput=${(typed) => setDraft(typed.target.value)}
                  aria-label="Request body"
                  spellcheck="false"
                ></textarea>
              </div>
            `}

        <details class="body cmd-headers">
          <summary class="tiny dim">headers</summary>
          <div class="cmd-form" style=${{ padding: '12px 0 0' }}>
            <label for="cmd-idem">
              <code>Idempotency-Key</code>
              <span class="tiny faint">a repeat replays the first commit verbatim</span>
            </label>
            <div class="cmd-field">
              <input
                id="cmd-idem"
                type="text"
                value=${idempotencyKey}
                onInput=${(typed) => setIdempotencyKey(typed.target.value)}
                spellcheck="false"
                autocomplete="off"
              />
            </div>
            <label for="cmd-corr">
              <code>X-Correlation-Id</code>
              <span class="tiny faint">a uuid, or a fresh one is minted</span>
            </label>
            <div class="cmd-field">
              <input
                id="cmd-corr"
                type="text"
                value=${correlationId}
                onInput=${(typed) => setCorrelationId(typed.target.value)}
                placeholder="auto"
                spellcheck="false"
                autocomplete="off"
              />
            </div>
          </div>
        </details>

        <div class="row" style=${{ justifyContent: 'flex-end', padding: '10px 14px', gap: '12px' }}>
          <span class="tiny faint">
            <code>POST /commands/${command.name}</code> · appends events
          </span>
          <${Copy} value=${body} title="Copy the request body" />
          <button
            type="button"
            class="btn primary"
            onClick=${run}
            disabled=${running || (mode === 'form' && invalid)}
          >
            ${running ? 'Running…' : 'Run'}
          </button>
        </div>
      <//>

      ${outcome &&
      html`
        <${Outcome}
          outcome=${outcome}
          command=${command.name}
          onClose=${() => setOutcome(null)}
          onRetry=${run}
        />
      `}
    </div>
  `
}

function Head({ command, children }) {
  return html`
    <section class="card">
      <header>
        <a
          href="/admin/commands"
          onClick=${(clicked) => {
            clicked.preventDefault()
            go('/admin/commands')
          }}
        >
          ← Commands
        </a>
        <code style=${{ textTransform: 'none', letterSpacing: 0 }}>${command.name}</code>
        ${command.internal
          ? html`<span class="pill mute">internal</span>`
          : html`<span class="pill ok">public</span>`}
        <div style=${{ flex: 1 }}></div>
        <span class="tiny faint mono" style=${{ textTransform: 'none', letterSpacing: 0 }}>
          ${command.path}
        </span>
      </header>
      ${children}
    </section>
  `
}

/** The declared shape, for a command with no form to fill. */
function Parameters({ command }) {
  if (command.input.length === 0) {
    return html`<div class="body tiny faint">It takes no parameters.</div>`
  }
  return html`
    <table class="data">
      <thead>
        <tr>
          <th scope="col">Parameter</th>
          <th scope="col">Kind</th>
          <th scope="col" style=${{ width: '90px' }}>Optional</th>
        </tr>
      </thead>
      <tbody>
        ${command.input.map(
          (field) => html`
            <tr key=${field.name}>
              <td><code>${field.name}</code></td>
              <td><span class="mono tiny dim">${field.kind}</span></td>
              <td>${field.optional ? '✓' : html`<span class="faint">·</span>`}</td>
            </tr>
          `,
        )}
      </tbody>
    </table>
  `
}

function Form({ command, values, errors, onChange }) {
  if (command.input.length === 0) {
    return html`<div class="body tiny faint">This command takes no parameters. Run it.</div>`
  }
  /* Functional, not `{ ...values, ... }`: preact batches state updates, so two changes
   * landing in one task both close over the same `values` and the second silently
   * discards the first. A paste into one field followed straight away by another, or
   * `new` followed by a keystroke, is enough to hit it. */
  const set = (name, value) => onChange((current) => ({ ...current, [name]: value }))
  return html`
    <div class="cmd-form">
      ${command.input.map(
        (field) => html`
          <label key=${field.name} for=${`cmd-${field.name}`}>
            <code>${field.name}</code>
            <span class="tiny faint mono">
              ${field.kind}${field.optional ? '' : ' · required'}
            </span>
          </label>
          <div>
            <${Control}
              field=${field}
              value=${values[field.name]}
              onChange=${(value) => set(field.name, value)}
            />
            ${errors[field.name] &&
            html`<div class="tiny" style=${{ color: 'var(--err)', marginTop: '4px' }}>
              ${errors[field.name]}
            </div>`}
          </div>
        `,
      )}
    </div>
  `
}

/** One control, chosen by the declared kind. The comments are the wire contract. */
function Control({ field, value, onChange }) {
  const id = `cmd-${field.name}`
  const kind = baseKind(field.kind)
  const variants = enumVariants(field.kind)

  if (variants) {
    return html`
      <div class="cmd-field">
        <select id=${id} value=${value} onChange=${(picked) => onChange(picked.target.value)}>
          ${field.optional && html`<option value="">(omit)</option>`}
          ${variants.map((variant) => html`<option key=${variant} value=${variant}>${variant}</option>`)}
        </select>
      </div>
    `
  }

  if (kind === 'Bool') {
    return html`
      <div class="cmd-field">
        <input
          id=${id}
          type="checkbox"
          checked=${value === true}
          onChange=${(checked) => onChange(checked.target.checked)}
        />
        <span class="tiny faint">${value === true ? 'true' : 'false'}</span>
      </div>
    `
  }

  if (kind === 'Json') {
    return html`
      <div class="cmd-field">
        <textarea
          id=${id}
          class="cmd-json"
          rows="3"
          value=${value}
          placeholder=${'{ }'}
          onInput=${(typed) => onChange(typed.target.value)}
          spellcheck="false"
        ></textarea>
      </div>
    `
  }

  if (kind === 'Uuid') {
    return html`
      <div class="cmd-field">
        <input
          id=${id}
          type="text"
          value=${value}
          placeholder="00000000-0000-4000-8000-000000000000"
          onInput=${(typed) => onChange(typed.target.value)}
          spellcheck="false"
          autocomplete="off"
        />
        ${/* `randomUUID` needs a secure context, which loopback is and a bare-http host
            is not. Hide the button rather than offer one that throws. */
        typeof crypto !== 'undefined' &&
        crypto.randomUUID &&
        html`<button type="button" class="btn" onClick=${() => onChange(crypto.randomUUID())}>
          new
        </button>`}
      </div>
    `
  }

  /* `toISOString` is always UTC with a `Z`. RFC 3339 without an offset is a 400 naming
   * RFC 3339, so the button cannot produce a value the server refuses. */
  if (kind === 'Timestamp') {
    return html`
      <div class="cmd-field">
        <input
          id=${id}
          type="text"
          value=${value}
          placeholder="2026-06-01T00:00:00Z"
          onInput=${(typed) => onChange(typed.target.value)}
          spellcheck="false"
          autocomplete="off"
        />
        <button type="button" class="btn" onClick=${() => onChange(new Date().toISOString())}>
          now
        </button>
      </div>
    `
  }

  if (kind === 'Int') {
    return html`
      <div class="cmd-field">
        <input
          id=${id}
          type="text"
          inputmode="numeric"
          value=${value}
          onInput=${(typed) => onChange(typed.target.value)}
          spellcheck="false"
          autocomplete="off"
        />
      </div>
    `
  }

  /* Money is text and leaves as a string, in and out. A number input would invite the
   * browser to hand back a float, which is the one representation money may not take. */
  const money = kind.startsWith('Money(')
  const max = maxLength(field.kind)
  return html`
    <div class="cmd-field">
      <input
        id=${id}
        type="text"
        inputmode=${money ? 'decimal' : undefined}
        maxlength=${max ?? undefined}
        value=${value}
        placeholder=${money ? '120.00' : undefined}
        onInput=${(typed) => onChange(typed.target.value)}
        spellcheck="false"
        autocomplete="off"
      />
      ${max && html`<span class="tiny faint">${value.length}/${max}</span>`}
    </div>
  `
}

/* --- the outcome ---------------------------------------------------------- */

/**
 * What came back.
 *
 * The one distinction worth getting right is 422. The command ran, folded its boundary
 * and declined on state grounds, which is a `refusal` doing its job; rendering it in
 * the same red as a 500 would teach the opposite of what refusals are for.
 */
function Outcome({ outcome, command, onClose, onRetry }) {
  if (outcome.local) {
    return html`
      <${DetailPanel} title="Result" subtitle="not sent" onClose=${onClose}>
        <div class="error-state" role="alert">
          <h3>The body is not JSON</h3>
          <p class="tiny">${outcome.local}</p>
        </div>
      <//>
    `
  }

  if (!outcome.ok) {
    const error = outcome.error
    const refused = error.status === 422
    const conflict = error.status === 409
    return html`
      <${DetailPanel}
        title="Result"
        subtitle=${`${command}`}
        onClose=${onClose}
        actions=${conflict &&
        html`<button type="button" class="btn" onClick=${onRetry}>Retry</button>`}
      >
        <div class="row wrap">
          <span class=${`pill ${refused || conflict ? 'warn' : 'err'} mono`}>
            ${/* A dead server has no status, and its code already says `unreachable`. */
            error.status ? `${error.status} ${error.code}` : error.code}
          </span>
        </div>
        <p class="tiny" style=${{ marginTop: 'var(--s3)' }}>${error.message}</p>
        ${refused &&
        html`
          <p class="tiny faint">
            The command ran and declined: <code>${error.code}</code> is a
            declared <code>refusal</code>, and nothing was appended. This is the command
            working.
          </p>
        `}
        ${conflict &&
        html`
          <p class="tiny faint">
            Its consistency boundary kept changing while it retried. Nothing was appended,
            and running it again is the documented answer.
          </p>
        `}
      <//>
    `
  }

  const { data } = outcome
  const positions = data.positions
  return html`
    <${DetailPanel}
      title="Result"
      subtitle=${command}
      onClose=${onClose}
      actions=${html`<${Copy} value=${JSON.stringify(data, null, 2)} title="Copy as JSON" />`}
    >
      <div class="row wrap">
        <span class="pill ok mono">200 committed</span>
      </div>

      <h3 class="section-title">Appended</h3>
      ${positions
        ? html`
            <div class="row wrap">
              ${range(positions).map(
                (position) => html`
                  <a
                    key=${position}
                    class="mono"
                    href=${`/admin/events/${position}`}
                    onClick=${(clicked) => {
                      clicked.preventDefault()
                      go(`/admin/events/${position}`)
                    }}
                  >
                    #${count(position)}
                  </a>
                `,
              )}
            </div>
          `
        : html`
            <p class="tiny faint">
              Nothing. The command decided to append no events, which is a success and what
              an idempotent replay looks like.
            </p>
          `}

      ${data.events.length > 0 &&
      html`
        <dl class="kv">
          ${data.events.map(
            (event, index) => html`
              <dt key=${index}><code>${event.type}</code></dt>
              <dd class="row wrap">
                ${event.tags.length === 0 && html`<span class="tiny faint">no tags</span>`}
                ${event.tags.map((tag) => html`<code class="tiny dim">${tag}</code>`)}
              </dd>
            `,
          )}
        </dl>
      `}

      <h3 class="section-title">Flow</h3>
      <dl class="kv">
        <dt>correlation</dt>
        <dd class="row">
          <a
            href=${`/admin/traces/${data.correlation_id}`}
            onClick=${(clicked) => {
              clicked.preventDefault()
              go(`/admin/traces/${data.correlation_id}`)
            }}
          >
            ${data.correlation_id}
          </a>
          <span class="note">→ trace</span>
        </dd>
        <dt>causation</dt>
        <dd class="mono tiny">${data.causation_id}</dd>
      </dl>
      ${positions &&
      html`
        <p class="tiny faint">
          A projector is asynchronous, so a read straight after this can miss it.
          Pass <code>?after=${positions.last}</code> to a read to wait for one.
        </p>
      `}
    <//>
  `
}

/** Every position a command appended. Ranges here are small: one command, one boundary. */
function range({ first, last }) {
  const out = []
  for (let position = first; position <= last; position++) out.push(position)
  return out
}
