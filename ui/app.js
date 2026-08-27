/* The shell: sidebar, header, routing, and the error boundary every view sits
 * inside. */

import { html, render, useEffect, useErrorBoundary, useState } from './vendor-preact.js'
import { NAV, go, useLocation } from './router.js'
import { isLive, setLive, refreshNow, useStatus } from './store.js'
import { useTheme } from './theme.js'
import { Empty } from './ui-states.js'
import { Palette } from './ui-palette.js'
import { count, duration } from './format.js'
import { OverviewView } from './view-overview.js'
import { EventsView } from './view-events.js'
import { TraceView } from './view-trace.js'
import { EffectsView } from './view-effects.js'
import { ProjectorsView } from './view-projectors.js'
import { SchemaView } from './view-schema.js'
import { SubjectsView } from './view-subjects.js'
import { SystemView } from './view-system.js'

const VIEWS = {
  overview: OverviewView,
  events: EventsView,
  trace: TraceView,
  effects: EffectsView,
  projectors: ProjectorsView,
  schema: SchemaView,
  subjects: SubjectsView,
  system: SystemView,
}

/* Left-click without modifiers is a navigation; everything else is the browser's
 * (open in a new tab, download, middle-click). Intercepting those would break the
 * one thing a real URL is for. */
function Link({ href, class: className, children, ...rest }) {
  const onClick = (event) => {
    if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) {
      return
    }
    event.preventDefault()
    go(href)
  }
  return html`<a href=${href} class=${className} onClick=${onClick} ...${rest}>${children}</a>`
}

function Rail({ location, status }) {
  const [theme, cycleTheme] = useTheme()
  const active = location.route?.id
  const wedged = (status?.effects ?? []).filter(
    (effect) => effect.state === 'wedged' || effect.state === 'quarantined',
  ).length

  return html`
    <nav class="rail" aria-label="Sections">
      <div class="brand">
        <svg viewBox="0 0 32 32" aria-hidden="true">
          <path d="M8 23V9h3.2v5.4h6.1V9h3.2v14h-3.2v-5.7h-6.1V23z" fill="currentColor" />
        </svg>
        <span>hekla</span>
      </div>

      <div class="nav">
        ${NAV.map(
          (route) => html`
            <${Link}
              href=${route.path}
              aria-current=${active === route.id ? 'page' : undefined}
            >
              <span>${route.title}</span>
              ${route.id === 'effects' &&
              wedged > 0 &&
              html`<span class="pill err">${wedged}</span>`}
            <//>
          `,
        )}
      </div>

      <div class="rail-foot">
        <div class="kv"><span>head</span><span>${count(status?.log_head)}</span></div>
        <div class="kv">
          <span>uptime</span>
          <span>${status ? duration(status.uptime_seconds * 1000) : '-'}</span>
        </div>
        <div class="rail-actions">
          <button
            type="button"
            class="btn icon"
            onClick=${cycleTheme}
            title=${`Theme: ${theme}`}
            aria-label=${`Theme: ${theme}. Click to change.`}
          >
            ${theme === 'dark' ? '◐' : theme === 'light' ? '◑' : '◒'}
          </button>
          <a
            class="btn icon"
            href="/docs"
            target="_blank"
            rel="noreferrer"
            title="API reference (opens Scalar, needs network)"
          >
            API ↗
          </a>
        </div>
      </div>
    </nav>
  `
}

function Header({ location, status, error }) {
  /* `isLive` reads storage rather than state, so the toggle keeps a local copy to
   * re-render on. */
  const [live, setLiveLocal] = useState(isLive)
  const title = location.route?.title ?? 'Not found'

  return html`
    <header class="topbar">
      <h1>${title}</h1>
      <div class="spacer"></div>
      ${error && html`<span class="pill err" title=${error.message}>${error.code}</span>`}
      <kbd title="Jump to a position, correlation id, effect or view">⌘K</kbd>
      ${status && html`<span class="tiny faint mono">${count(status.log_head)} events</span>`}
      <button
        type="button"
        class="btn"
        onClick=${() => {
          setLive(!live)
          setLiveLocal(!live)
        }}
        title=${live ? 'Polling every 3s. Click to pause.' : 'Paused. Click to resume.'}
      >
        ${live ? '● live' : '○ paused'}
      </button>
      <button type="button" class="btn" onClick=${refreshNow} title="Refresh now">⟳</button>
    </header>
  `
}

function Boundary({ children }) {
  const [error, reset] = useErrorBoundary()
  if (error) {
    /* One broken panel should cost that panel, not the whole console. The stack goes
     * to the console for whoever is debugging it; the page stays usable. */
    return html`
      <div class="error-state" role="alert">
        <h3>This view failed to render</h3>
        <p class="tiny">${String(error?.message ?? error)}</p>
        <button type="button" class="btn" onClick=${reset}>Reload the view</button>
      </div>
    `
  }
  return children
}

function NotFound({ pathname }) {
  return html`
    <${Empty} title="No such page">
      <code>${pathname}</code> is under <code>/admin</code> but is not one of the
      console's views. Requested with <code>Accept: application/json</code> it may still
      be a real endpoint.
    <//>
  `
}

function App() {
  const location = useLocation()
  const { status, error } = useStatus()
  const View = location.route ? VIEWS[location.route.id] : null

  useEffect(() => {
    document.title = location.route ? `${location.route.title} · hekla` : 'hekla'
  }, [location.route?.id])

  return html`
    <div class="shell">
      <${Rail} location=${location} status=${status} />
      <div class="main">
        <${Header} location=${location} status=${status} error=${error} />
        <main class="content">
          <${Boundary} key=${location.route?.id ?? 'none'}>
            ${View
              ? html`<${View} params=${location.route.params} search=${location.search} />`
              : html`<${NotFound} pathname=${location.pathname} />`}
          <//>
        </main>
      </div>
      <${Palette} status=${status} />
    </div>
  `
}

render(html`<${App} />`, document.getElementById('root'))
