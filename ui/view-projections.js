/* Projections: write a projector that was never deployed, fold it over the log, read
 * its rows, throw it away.
 *
 * The rest of the console answers questions somebody anticipated. A read model exists
 * because an author declared it, and the page that browses it can only show what was
 * declared. This is the page for the question nobody anticipated: it takes heklang,
 * folds it over the real log through the same interpreter the deployment runs, and
 * shows what comes out. Nothing is deployed, nothing is written, and the temporary
 * database it builds the rows in is deleted when the answer is sent.
 *
 * It is gated (`[admin] projections = true`) because it is the one place on the surface
 * that runs code a caller supplied rather than code the project declared. What bounds
 * it is that heklang is total and a projector holds no host, so the worst a projection
 * can spend is CPU and a temporary file, and the server clamps how much of both.
 *
 * Saved projections live in `localStorage` rather than on the server, which is the
 * honest place for them: a question you asked is yours, not the deployment's, and
 * putting them server-side would mean a write surface on a page whose whole claim is
 * that it does not write. */

import { html, useEffect, useRef, useState } from './vendor-preact.js'
import { api } from './api.js'
import { go } from './router.js'
import { Empty, ErrorState, Resource, useResource } from './ui-states.js'
import { DataTable } from './ui-table.js'
import { DetailPanel } from './ui-panel.js'
import { SourceEditor } from './ui-editor.js'
import { JsonTree } from './ui-json.js'
import { Copy } from './ui-copy.js'
import { baseKind, maxLength } from './kinds.js'
import { count, duration, plural, shortHash, truncate } from './format.js'

/* Namespaced like every other key the console keeps (`hekla.theme`, `hekla.live`), and
 * the first of them to hold JSON rather than one word, so the read below validates
 * what it finds instead of trusting it. */
const SAVED = 'hekla.projections'
const DRAFT = 'hekla.projections.draft'
const SHAPE = 1

/** How long after the last keystroke the draft is written. */
const SETTLE = 400

/** What the window controls send when they are left blank: the server's own defaults. */
const BLANK = {
  projector: '',
  entity: '',
  from: '',
  upto: '',
  max_events: '',
  rows: '',
  decrypt: true,
}

/* Read when the schema call has not answered or the project declares no events. Valid
 * heklang, so the first Run succeeds rather than teaching the page with an error. */
const STARTER = `// A projector that is never deployed. Fold it, read it, throw it away.
projector Scratch {
  entity Tally {
    kind: String @key @max(120),
    events: Int,
  }
}
`

// --- storage ---------------------------------------------------------------

/* Every one of these swallows its own failure. A private window, cleared site data and
 * a browser set to block storage all throw here rather than returning null, and none of
 * them is a reason to take the page down: the editor works, the run works, and only
 * remembering it across a reload does not. */

function saved() {
  try {
    const held = JSON.parse(localStorage.getItem(SAVED) ?? 'null')
    if (held?.v !== SHAPE || !Array.isArray(held.items)) return []
    return held.items.filter((item) => typeof item?.name === 'string' && typeof item.source === 'string')
  } catch {
    return []
  }
}

/** False when the browser refused, which is worth telling somebody who just typed. */
function keep(items) {
  try {
    localStorage.setItem(SAVED, JSON.stringify({ v: SHAPE, items }))
    return true
  } catch {
    return false
  }
}

function draft() {
  try {
    const held = JSON.parse(localStorage.getItem(DRAFT) ?? 'null')
    if (typeof held?.source !== 'string') return null
    return { source: held.source, params: { ...BLANK, ...held.params } }
  } catch {
    return null
  }
}

function remember(source, params) {
  try {
    localStorage.setItem(DRAFT, JSON.stringify({ source, params }))
  } catch {
    /* The buffer is still on the screen; it just will not survive a reload. */
  }
}

// --- writing the first one -------------------------------------------------

/** A column declaration for `field`, with the `@max` heklang requires on a `String`. */
function column(field, role) {
  const base = baseKind(field.kind)
  const cap = base === 'String' ? ` @max(${maxLength(field.kind) ?? 120})` : ''
  return `${field.name}: ${base} ${role}${cap}`
}

/* Something that runs, over an event this project actually declares. A blank editor
 * with a "write heklang here" placeholder is a worse first minute than a projector that
 * answers a real question about the log and can be edited into the one you wanted. */
function starter(schema) {
  /* Grouping by an id counts one per id, which teaches nothing. Anything that is not an
   * identifier or a time usually has repeats in it, and repeats are what a `patch` is
   * for, so the event to open with is the first one that declares such a field. */
  const groupable = (field) => !['Uuid', 'Timestamp'].includes(baseKind(field.kind))
  const events = schema?.events ?? []
  const event = events.find((one) => one.fields.some(groupable)) ?? events[0]
  if (!event) return STARTER
  const key = event.fields.find(groupable) ?? event.fields[0]
  return `// Counts @${event.type} by ${key.name}. Edit, then Run.
projector Scratch {
  entity Tally {
    ${column(key, '@key')},
    events: Int,
  }

  on @${event.type} { ${key.name} } {
    patch Tally[${key.name}] { events: .events + 1 }
  }
}
`
}

/* Spliced in before the projector's closing brace, which is where a handler goes in
 * anything well formed. A guess, deliberately a cheap one: it is an editing aid, the
 * source is right there, and heklang says so immediately if it landed wrong. */
function withHandler(source, event) {
  const fields = event.fields.map((field) => field.name).join(', ')
  const block = `\n  on @${event.type} { ${fields} } {\n    \n  }\n`
  const close = source.lastIndexOf('}')
  if (close < 0) return source + block
  return source.slice(0, close) + block + source.slice(close)
}

// --- the page --------------------------------------------------------------

/** The seven the endpoint takes, and it refuses an eighth rather than ignoring it. */
function knobs(params) {
  return [
    ['projector', params.projector],
    ['entity', params.entity],
    ['from', params.from],
    ['upto', params.upto],
    ['max_events', params.max_events],
    ['rows', params.rows],
    ['decrypt', String(params.decrypt)],
  ]
}

export function ProjectionsView() {
  const limits = useResource((signal) => api.projections(signal), [])
  /* The schema is an aid, not a dependency: it writes the first projector and fills the
   * insert menu. A failure here leaves both, and nothing else, less helpful. */
  const schema = useResource((signal) => api.schema(signal), [])

  /* One read, not one per initializer: the draft holds the whole editor buffer, and
   * parsing a few kilobytes of it twice on the main thread buys nothing. */
  const [held] = useState(draft)
  const [source, setSource] = useState(() => held?.source ?? '')
  const [params, setParams] = useState(() => held?.params ?? BLANK)
  const [items, setItems] = useState(saved)
  const seeded = useRef(false)
  const [loaded, setLoaded] = useState(null)
  const [undone, setUndone] = useState(null)
  const [refused, setRefused] = useState(false)

  const [running, setRunning] = useState(false)
  const [progress, setProgress] = useState(null)
  const [elapsed, setElapsed] = useState(0)
  const [outcome, setOutcome] = useState(null)
  const [reveal, setReveal] = useState(null)
  const [open, setOpen] = useState(null)
  const flight = useRef(null)

  /* Once, and only into an editor nobody has put anything in. Someone who cleared the
   * box meant to clear it, and a page that writes over that is a page you cannot empty.
   *
   * Functional, and for the reason `view-commands.js` spells out: this effect runs when
   * the schema lands, so reading `source` from the closure reads whatever it was when
   * the effect was created. Someone who starts typing in the few milliseconds before
   * `/admin/schema` answers would have had their first keystrokes replaced by the
   * starter. The updater sees the live value instead. */
  useEffect(() => {
    if (seeded.current || !schema.data) return
    seeded.current = true
    setSource((current) => current || starter(schema.data))
  }, [schema.data])

  /* Written once the typing stops rather than per keystroke: a projector is a few
   * kilobytes and `setItem` is synchronous on the main thread. */
  useEffect(() => {
    if (!source && params === BLANK) return
    const timer = setTimeout(() => remember(source, params), SETTLE)
    return () => clearTimeout(timer)
  }, [source, params])

  /* The page's own clock, not the server's. Ticks arrive once per matching event, so a
   * selective projector can be silent for a long time while working perfectly, and a
   * number that stops moving is how a person decides something is stuck. */
  useEffect(() => {
    if (!running) return
    const began = Date.now()
    setElapsed(0)
    const timer = setInterval(() => setElapsed(Date.now() - began), 100)
    return () => clearInterval(timer)
  }, [running])

  /* Abandoning the read does not stop the fold, which runs to its budget on a blocking
   * thread. It does stop this page holding a reader open after it is gone. */
  useEffect(() => () => flight.current?.abort(), [])

  const enabled = limits.data?.enabled ?? false

  const run = async () => {
    if (running || !enabled || !source.trim()) return
    const controller = new AbortController()
    flight.current = controller
    setRunning(true)
    setProgress(null)
    setOutcome(null)
    setOpen(null)
    const began = Date.now()
    try {
      const projection = await api.project(source, knobs(params), {
        onProgress: setProgress,
        signal: controller.signal,
      })
      setOutcome({ projection, took: Date.now() - began })
    } catch (err) {
      if (err.name === 'AbortError') return
      setOutcome({ error: err, took: Date.now() - began })
    } finally {
      setRunning(false)
    }
  }

  const change = (key, value) => setParams((current) => ({ ...current, [key]: value }))

  const save = (name) => {
    const entry = { name, source, params, saved_at: new Date().toISOString() }
    const next = [...items.filter((item) => item.name !== name), entry].sort((left, right) =>
      left.name.localeCompare(right.name),
    )
    setRefused(!keep(next))
    setItems(next)
    setLoaded(name)
  }

  const load = (item) => {
    setSource(item.source)
    setParams({ ...BLANK, ...item.params })
    setLoaded(item.name)
    setOutcome(null)
  }

  const drop = (item) => {
    const next = items.filter((one) => one.name !== item.name)
    setRefused(!keep(next))
    setItems(next)
    setUndone(item)
    if (loaded === item.name) setLoaded(null)
  }

  const undo = () => {
    const next = [...items, undone].sort((left, right) => left.name.localeCompare(right.name))
    setRefused(!keep(next))
    setItems(next)
    setUndone(null)
  }

  const findings = outcome?.error?.body?.findings ?? []
  const markers = new Set(findings.map((finding) => finding.line).filter(Boolean))

  return html`
    <${Resource} state=${limits}>
      ${(caps) => html`
        <section class="card">
          <header>
            Source
            <span style=${{ flex: 1 }}></span>
            ${schema.data &&
            html`
              <select
                class="hk-insert"
                value=""
                aria-label="Insert a handler"
                onChange=${(picked) => {
                  const event = schema.data.events.find((one) => one.type === picked.target.value)
                  if (event) setSource(withHandler(source, event))
                  picked.target.value = ''
                }}
              >
                <option value="">insert on @…</option>
                ${schema.data.events.map(
                  (event) => html`<option value=${event.type}>@${event.type}</option>`,
                )}
              </select>
            `}
            <button
              type="button"
              class="btn primary"
              onClick=${run}
              disabled=${running || !enabled || !source.trim()}
              title=${enabled ? 'Run this projection (⌘↵)' : 'projections are turned off'}
            >
              ${running ? 'Folding…' : 'Run'}
            </button>
          </header>

          <${Saved}
            items=${items}
            loaded=${loaded}
            undone=${undone}
            refused=${refused}
            onLoad=${load}
            onSave=${save}
            onDrop=${drop}
            onUndo=${undo}
          />

          <${SourceEditor}
            value=${source}
            onInput=${setSource}
            onRun=${run}
            markers=${markers}
            reveal=${reveal}
            label="Projector source"
            placeholder=${'projector Scratch {\n  …\n}'}
          />

          <${Window} params=${params} caps=${caps} onChange=${change} />

          ${!enabled && html`<${Disabled} />`}
        </section>

        ${running && html`<${Folding} progress=${progress} elapsed=${elapsed} params=${params} caps=${caps} />`}

        ${!running &&
        outcome?.error &&
        html`<${Refused} error=${outcome.error} findings=${findings} onReveal=${setReveal} onRetry=${run} />`}

        ${!running &&
        outcome?.projection &&
        html`
          <${Answer}
            projection=${outcome.projection}
            took=${outcome.took}
            caps=${caps}
            open=${open}
            onOpen=${setOpen}
            onRaise=${(key, value) => {
              change(key, String(value))
              setOutcome(null)
            }}
          />
        `}
      `}
    <//>
  `
}

// --- saved -----------------------------------------------------------------

function Saved({ items, loaded, undone, refused, onLoad, onSave, onDrop, onUndo }) {
  const [naming, setNaming] = useState(false)
  const [name, setName] = useState('')

  const commit = () => {
    const trimmed = name.trim()
    if (!trimmed) return
    onSave(trimmed)
    setName('')
    setNaming(false)
  }

  return html`
    <div class="filters wrap">
      <span class="tiny faint">saved</span>
      ${items.length === 0 &&
      !naming &&
      html`<span class="tiny dim">nothing yet, and they stay in this browser</span>`}
      ${items.map(
        (item) => html`
          <span class=${`chip${item.name === loaded ? ' on' : ''}`}>
            <button
              type="button"
              class="chip-open"
              onClick=${() => onLoad(item)}
              title=${`saved ${item.saved_at?.slice(0, 16).replace('T', ' ') ?? ''}`}
            >
              ${truncate(item.name, 32)}
            </button>
            <button type="button" aria-label=${`Delete ${item.name}`} onClick=${() => onDrop(item)}>
              ✕
            </button>
          </span>
        `,
      )}
      ${naming
        ? html`
            <input
              class="filter-input"
              value=${name}
              autofocus
              placeholder="name it"
              aria-label="Name for this projection"
              onInput=${(typed) => setName(typed.target.value)}
              onKeyDown=${(pressed) => {
                if (pressed.key === 'Enter') {
                  pressed.preventDefault()
                  commit()
                }
                if (pressed.key === 'Escape') setNaming(false)
              }}
            />
            <button type="button" class="btn" onClick=${commit} disabled=${!name.trim()}>save</button>
          `
        : html`
            <button type="button" class="btn" onClick=${() => setNaming(true)}>+ save as…</button>
          `}
      ${undone &&
      html`
        <button type="button" class="btn" onClick=${onUndo}>
          undo delete of ${truncate(undone.name, 24)}
        </button>
      `}
      ${refused &&
      html`<span class="tiny" style=${{ color: 'var(--err)' }}>
        this browser refused to store it
      </span>`}
    </div>
  `
}

// --- the window ------------------------------------------------------------

function Window({ params, caps, onChange }) {
  const [more, setMore] = useState(Boolean(params.projector || params.entity))
  const field = (key, label, placeholder, width) => html`
    <label class="hk-knob">
      <span class="tiny faint">${label}</span>
      <input
        value=${params[key]}
        inputmode="numeric"
        placeholder=${placeholder}
        style=${{ width }}
        aria-label=${label}
        onInput=${(typed) => onChange(key, typed.target.value)}
      />
    </label>
  `

  return html`
    <div class="filters wrap">
      ${field('from', 'from', '1', '7rem')}
      ${field('upto', 'upto', String(caps.log_head), '7rem')}
      ${field('max_events', 'max events', String(caps.max_events.default), '8rem')}
      ${field('rows', 'rows', String(caps.rows.default), '5rem')}
      <label class="hk-knob row">
        <input
          type="checkbox"
          checked=${params.decrypt}
          onChange=${(changed) => onChange('decrypt', changed.target.checked)}
        />
        <span class="tiny">decrypt</span>
      </label>
      <span style=${{ flex: 1 }}></span>
      <button type="button" class="btn" onClick=${() => setMore(!more)}>
        ${more ? '▾ less' : '▸ more'}
      </button>
    </div>
    ${more &&
    html`
      <div class="filters wrap">
        <label class="hk-knob">
          <span class="tiny faint">projector</span>
          <input
            value=${params.projector}
            placeholder="only if the source declares more than one"
            style=${{ width: '18rem' }}
            aria-label="Projector"
            onInput=${(typed) => onChange('projector', typed.target.value)}
          />
        </label>
        <label class="hk-knob">
          <span class="tiny faint">entity</span>
          <input
            value=${params.entity}
            placeholder="all of them"
            style=${{ width: '12rem' }}
            aria-label="Entity"
            onInput=${(typed) => onChange('entity', typed.target.value)}
          />
        </label>
      </div>
    `}
    <div class="body tiny faint">
      ${`The log is at position ${count(caps.log_head)}. A budget above ` +
      `${count(caps.max_events.limit)} events is clamped to it, and so is a row cap ` +
      `above ${count(caps.rows.limit)}. Nothing here is deployed and nothing is ` +
      `written: the fold builds its rows in a temporary database and deletes it on ` +
      `the way out.`}
    </div>
  `
}

function Disabled() {
  return html`
    <div class="body">
      <${Empty} title="Projections are turned off">
        ${'This deployment does not fold ad-hoc projections. Add it to '}
        <code>hekla.toml</code>${' and restart:'}
        <pre class="json" style=${{ marginTop: 'var(--s3)' }}>[admin]
projections = true</pre>
        <div class="row" style=${{ marginTop: 'var(--s2)' }}>
          <${Copy} value=${'[admin]\nprojections = true\n'} title="Copy the setting" />
          <span class="tiny faint">
            It is off by default because this is the one page that runs code the project
            did not declare.
          </span>
        </div>
      <//>
    </div>
  `
}

// --- while it folds --------------------------------------------------------

function Folding({ progress, elapsed, params, caps }) {
  const from = Number(params.from || 0)
  const width = progress ? Math.max(1, progress.upto - from) : 0
  const done = progress ? Math.min(1, Math.max(0, (progress.position - from) / width)) : 0
  const budget = Number(params.max_events || caps.max_events.default)

  return html`
    <section class="card">
      <header>Folding</header>
      <div class="body">
        <div class=${`hk-bar${progress ? '' : ' waiting'}`} role="progressbar" aria-label="Fold progress">
          <div class="hk-bar-fill" style=${{ width: `${(done * 100).toFixed(1)}%` }}></div>
        </div>
        <div class="row wrap tiny dim" style=${{ marginTop: 'var(--s2)' }}>
          ${progress
            ? html`
                <span class="mono">
                  position ${count(progress.position)} of ${count(progress.upto)}
                </span>
                <span>·</span>
                <span>${plural(progress.events, 'event')} folded</span>
              `
            : html`<span>starting</span>`}
          <span>·</span>
          <span class="mono">${duration(elapsed)}</span>
          <span>·</span>
          <span class="faint">stops at ${count(budget)} events</span>
        </div>
        <p class="tiny faint" style=${{ marginTop: 'var(--s3)' }}>
          Progress arrives once per matching event, so a projector that selects rarely
          reports rarely. The fold runs to its budget once started; leaving this page
          stops the watching, not the work.
        </p>
      </div>
    </section>
  `
}

// --- when it will not run --------------------------------------------------

function Refused({ error, findings, onReveal, onRetry }) {
  if (findings.length > 0) {
    return html`
      <section class="card">
        <header>
          Does not compile
          <span style=${{ flex: 1 }}></span>
          <span class="tiny faint">${plural(findings.length, 'finding')}</span>
        </header>
        <div class="body">
          ${findings.map(
            (finding, index) => html`
              <div class="hk-finding" key=${index}>
                <span class=${`pill ${finding.severity === 'error' ? 'err' : 'warn'} plain`}>
                  ${finding.severity}
                </span>
                ${finding.line
                  ? html`
                      <button
                        type="button"
                        class="btn icon mono"
                        title="Go to this line"
                        onClick=${() =>
                          onReveal({
                            line: finding.line,
                            column: finding.column,
                            nonce: `${index}:${Date.now()}`,
                          })}
                      >
                        ${finding.line}:${finding.column}
                      </button>
                    `
                  : html`<span class="tiny faint mono">${finding.location}</span>`}
                <div>
                  <div>${finding.message}</div>
                  ${finding.hint && html`<div class="tiny faint">${finding.hint}</div>`}
                </div>
              </div>
            `,
          )}
        </div>
      </section>
    `
  }

  /* A deployment already folding two is not a failure of this request, and painting it
   * red would say something broke. */
  if (error.status === 429) {
    return html`
      <section class="card">
        <header>Busy</header>
        <div class="body">
          <${Empty} title="Every slot is folding">
            ${error.message}
            <div class="row" style=${{ marginTop: 'var(--s3)' }}>
              <button type="button" class="btn" onClick=${onRetry}>Try again</button>
            </div>
          <//>
        </div>
      </section>
    `
  }

  return html`
    <section class="card">
      <div class="body"><${ErrorState} error=${error} onRetry=${onRetry} /></div>
    </section>
  `
}

// --- the answer ------------------------------------------------------------

function Answer({ projection, took, caps, open, onOpen, onRaise }) {
  const window = projection.window
  const scanned = projection.scanned
  const partial = scanned.stopped === 'max-events'

  return html`
    <section class="card">
      <header>
        Result
        <span style=${{ flex: 1 }}></span>
        <span class="tiny faint mono" title="the projector's heklang digest">
          ${shortHash(projection.digest)}
        </span>
        <${Copy} value=${JSON.stringify(projection, null, 2)} title="Copy as JSON" />
      </header>
      <div class="body">
        <div class="row wrap">
          <span class="pill ok plain">${plural(scanned.events, 'event')}</span>
          <span class="tiny dim mono">
            positions ${count(window.from || 1)}–${count(window.upto)} of ${count(window.head)}
          </span>
          <span class="tiny faint">·</span>
          <span class="tiny dim mono">${duration(took)}</span>
          ${partial &&
          html`
            <span class="pill warn" title="these rows are part of the answer and not all of it">
              partial
            </span>
            <button
              type="button"
              class="btn"
              onClick=${() =>
                onRaise(
                  'max_events',
                  Math.min(caps.max_events.limit, Math.max(scanned.events * 4, 1000)),
                )}
            >
              raise the budget
            </button>
          `}
          ${projection.shredded.writes > 0 &&
          html`
            <span
              class="pill mute"
              title="the subject's key is gone, erased or superseded, so the fold dropped the write rather than storing plaintext"
            >
              ${plural(projection.shredded.writes, 'write')} dropped across
              ${plural(projection.shredded.subjects, 'subject')}
            </span>
          `}
        </div>
        <div class="row wrap tiny" style=${{ marginTop: 'var(--s2)' }}>
          <span class="faint">from</span>
          ${projection.sources.map(
            (type) => html`
              <a
                href=${`/admin/events?type=${encodeURIComponent(type)}`}
                class="mono"
                onClick=${(clicked) => {
                  clicked.preventDefault()
                  go(`/admin/events?type=${encodeURIComponent(type)}`)
                }}
              >
                @${type}
              </a>
            `,
          )}
        </div>
      </div>
    </section>

    ${projection.entities.map(
      (entity) => html`
        <${EntityRows}
          key=${entity.name}
          entity=${entity}
          caps=${caps}
          open=${open}
          onOpen=${onOpen}
          onRaise=${onRaise}
        />
      `,
    )}
  `
}

function EntityRows({ entity, caps, open, onOpen, onRaise }) {
  const sealed = new Map(entity.sealed.map((one) => [one.column, one.subject]))
  const ordered = [
    ...entity.columns.filter((name) => name === entity.key),
    ...entity.columns.filter((name) => name !== entity.key),
  ]
  /* No declared kinds on the wire, so alignment comes off the values on this page. A
   * column with one string in it is not a number column. */
  const numeric = (name) =>
    entity.rows.some((row) => typeof row[name] === 'number') &&
    entity.rows.every((row) => !Object.hasOwn(row, name) || typeof row[name] === 'number')

  const columns = ordered.map((name) => ({
    key: name,
    header: name,
    align: numeric(name) ? 'right' : undefined,
    render: (row) => html`<${Cell} name=${name} row=${row} subject=${sealed.get(name)} />`,
  }))

  const shown = open?.entity === entity.name ? entity.rows[open.index] : null
  return html`
    <div class=${shown ? 'with-detail' : undefined}>
      <section class="card">
        <header>
          <code>${entity.name}</code>
          <span class="tiny faint">key ${entity.key}</span>
          <span style=${{ flex: 1 }}></span>
          ${entity.truncated
            ? html`
                <span class="tiny dim">
                  showing ${count(entity.rows.length)} of ${count(entity.row_count)}
                </span>
                <button
                  type="button"
                  class="btn"
                  onClick=${() => onRaise('rows', caps.rows.limit)}
                >
                  show more
                </button>
              `
            : html`<span class="pill mute plain mono">${plural(entity.row_count, 'row')}</span>`}
          ${entity.stale.cells > 0 &&
          html`
            <span
              class="pill warn"
              title=${`counted over the ${entity.stale.of_rows_shown} row(s) shown, not the whole entity`}
            >
              ${plural(entity.stale.cells, 'cell')} would not open
            </span>
          `}
        </header>
        <${DataTable}
          label=${`${entity.name} rows`}
          columns=${columns}
          rows=${entity.rows}
          onOpen=${(row, index) => onOpen({ entity: entity.name, index })}
          selected=${(row) => row === shown}
          empty=${html`<${Empty} title="No rows">
            ${`The projector folded nothing into ${entity.name}.`}
          <//>`}
        />
      </section>
      ${shown &&
      html`
        <${DetailPanel}
          title=${entity.name}
          subtitle=${String(shown[entity.key] ?? '')}
          onClose=${() => onOpen(null)}
          actions=${html`<${Copy} value=${JSON.stringify(shown, null, 2)} title="Copy the row" />`}
        >
          <${JsonTree} value=${shown} />
          ${entity.sealed.length > 0 &&
          html`
            <p class="tiny faint" style=${{ marginTop: 'var(--s3)' }}>
              ${entity.sealed
                .map((one) => `${one.column} is scoped to ${one.subject}`)
                .join('; ')}${'. A column missing above is either unset or has no key ' +
              'left to open it, and this response cannot tell those apart.'}
            </p>
          `}
        <//>
      `}
    </div>
  `
}

/* A row omits what it does not carry rather than nulling it, and three different things
 * look identical from the row alone: an unset optional, a value whose subject key was
 * erased, and one written under a key since superseded. What the response does say is
 * which columns are sealed, and that is exactly the line worth drawing: a blank cell in
 * a sealed column may be an erasure, and a blank cell anywhere else cannot be. */
function Cell({ name, row, subject }) {
  if (!Object.hasOwn(row, name)) {
    if (subject) {
      return html`
        <span
          class="pill mute plain"
          title=${`the key for \`${subject}\` cannot be obtained: erased, or this value was written under a superseded one`}
        >
          absent
        </span>
      `
    }
    return html`<span class="faint" title="the row carries no value for this column">·</span>`
  }
  const value = row[name]
  if (value !== null && typeof value === 'object') {
    const text = JSON.stringify(value)
    return html`<span class="mono tiny dim" title=${text}>${truncate(text, 48)}</span>`
  }
  if (value === null || typeof value === 'number' || typeof value === 'boolean') {
    return html`<span class="mono">${String(value)}</span>`
  }
  const text = String(value)
  return html`<span title=${text.length > 44 ? text : undefined}>${truncate(text, 44)}</span>`
}
