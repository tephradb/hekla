/* The shell: sidebar, header, routing, and the error boundary every view sits
 * inside. */

import { html, render, useEffect, useErrorBoundary, useRef, useState } from './vendor-preact.js'
import { NAV, go, useLocation } from './router.js'
import { isLive, setLive, refreshNow, useStatus } from './store.js'
import { useTheme } from './theme.js'
import { Empty } from './ui-states.js'
import { Palette } from './ui-palette.js'
import { count, duration, plural } from './format.js'
import { OverviewView } from './view-overview.js'
import { CommandsView } from './view-commands.js'
import { EventsView } from './view-events.js'
import { TraceView } from './view-trace.js'
import { EffectsView } from './view-effects.js'
import { ProjectorsView } from './view-projectors.js'
import { ProjectionsView } from './view-projections.js'
import { SchemaView } from './view-schema.js'
import { SubjectsView } from './view-subjects.js'
import { SystemView } from './view-system.js'

const VIEWS = {
  overview: OverviewView,
  commands: CommandsView,
  events: EventsView,
  trace: TraceView,
  effects: EffectsView,
  projectors: ProjectorsView,
  projections: ProjectionsView,
  schema: SchemaView,
  subjects: SubjectsView,
  system: SystemView,
}

/* The one breakpoint the shell needs to know about in script rather than in CSS, and
 * it is spelled the same in both places. Below it the rail is a drawer laid over the
 * content, which is a fact about focus as much as about layout: a closed drawer must
 * leave the tab order and an open one must take the content out of it. `inert` is an
 * attribute, so no rule can say either. */
const NARROW = '(max-width: 760px)'

function useNarrow() {
  const [narrow, setNarrow] = useState(() => window.matchMedia(NARROW).matches)

  useEffect(() => {
    const query = window.matchMedia(NARROW)
    const onChange = (event) => setNarrow(event.matches)
    query.addEventListener('change', onChange)
    return () => query.removeEventListener('change', onChange)
  }, [])

  return narrow
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

/* The tephra mark, inline rather than an `<img>` for the same reason the console has
 * no build step: one fewer request and one fewer asset name to keep in the table. Its
 * fills are literal because it is artwork and not an icon: the facets are the drawing,
 * so there is no one colour for it to take from the text around it. */
function Mark() {
  return html`
    <svg viewBox="0 0 2008 2008" fill="none" aria-hidden="true">
      <path d="M1403 1L978 490L940 660L697 426L1403 1Z" fill="#7B7085" />
      <path d="M766 1565L923 2008L598 1482L766 1565ZM1403 1L1561 578L1661 779L1304 788L1085 842L1403 1Z" fill="#1B191E" />
      <path d="M1564 896L1110 1466L891 1498L1129 956L1564 896Z" fill="#25425A" />
      <path d="M1085 841.5L861 1042L486 928L714 582L940 660L1085 841.5Z" fill="#BB9CD3" />
      <path d="M1110 1466L1016.5 1737L923 2008L891 1498L1110 1466Z" fill="#202C3C" />
      <path d="M1110 1466L891 1498L1129 956L1110 1466Z" fill="#9FBFD7" />
      <path d="M1564 896L1129 956L1304 788L1564 896Z" fill="#1A1F2A" />
      <path d="M940 660L486 928L714 582L940 660Z" fill="#C7A6DF" />
      <path d="M978 490L1161 642L1085 842L978 490Z" fill="#3E2F41" />
      <path d="M940 660L1085 842L861 1042L940 660Z" fill="#AF8FC8" />
      <path d="M714 582L486 928L345 1023L419 799L643 667L714 582Z" fill="#261F2D" />
      <path d="M859 1044L890 1498L767 1563L859 1044ZM1661 779L1564 896L1304 788L1661 779Z" fill="#202C3C" />
      <path d="M697 426L940 660L714 582L697 426Z" fill="#1A1A20" />
      <path d="M978 490L1085 842L940 660L978 490Z" fill="#48404E" />
      <path d="M861 1042L891 1498L766 1565L861 1042Z" fill="#1F2531" />
      <path d="M486 928L766 1565L861 1042L486 928Z" fill="#1B1D24" />
      <path d="M861 1042L891 1498L1129 956L861 1042Z" fill="#202C3C" />
      <path d="M1085 842L861 1042L1129 956L1085 842Z" fill="#1F2937" />
      <path d="M1304 788L1085 842L1129 956L1304 788Z" fill="#1F2734" />
      <path d="M1161 642L1403 1L978 490L1161 642Z" fill="#786D80" />
      <path d="M1309 608L1304 788L1661 779L1309 608Z" fill="#2B1729" />
      <path d="M1561 578L1309 608L1661 779L1561 578Z" fill="#261524" />
      <path d="M1403 1L1309 608L1561 578L1403 1Z" fill="#241523" />
      <path d="M1304 788L1309 608L1161 642L1085 842L1304 788Z" fill="#1E151F" />
      <path d="M1403 1L1161 642L1309 608L1403 1Z" fill="#1D151E" />
      <path d="M891 1498L923 2008L766 1565L891 1498Z" fill="#1B1A23" />
      <path d="M486 928L766 1565L598 1482L345 1023L486 928Z" fill="#18171D" />
    </svg>
  `
}

function Rail({ location, status, drawer, open, onClose }) {
  const [theme, cycleTheme] = useTheme()
  const close = useRef(null)
  const active = location.route?.id
  const wedged = (status?.effects ?? []).filter(
    (effect) => effect.state === 'wedged' || effect.state === 'quarantined',
  ).length

  /* Opening the drawer moves focus into it, because the button that opened it is
   * behind the scrim and inert from that moment on: leaving focus there would strand
   * a keyboard on an element nothing can act on. */
  useEffect(() => {
    if (drawer && open) close.current?.focus()
  }, [drawer, open])

  return html`
    <nav
      class="rail"
      id="rail"
      aria-label="Sections"
      inert=${drawer && !open ? true : undefined}
    >
      <div class="brand">
        <${Mark} />
        <span>hekla</span>
        <div style=${{ flex: 1 }}></div>
        <button
          type="button"
          class="btn icon narrow-only"
          ref=${close}
          onClick=${onClose}
          aria-label="Close the menu"
          title="Close the menu"
        >
          ✕
        </button>
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

/* The search button and the ⌘K hint are the same palette from either side of the
 * breakpoint. A phone has no ⌘ to hold, and the palette is how you reach a position
 * or a correlation id you already have in the clipboard, so the hint alone would
 * leave the feature unreachable on the device most likely to be pasting into it. */
function Header({ location, status, error, navOpen, onMenu, onSearch }) {
  /* `isLive` reads storage rather than state, so the toggle keeps a local copy to
   * re-render on. */
  const [live, setLiveLocal] = useState(isLive)
  const title = location.route?.title ?? 'Not found'

  return html`
    <header class="topbar">
      <button
        type="button"
        class="btn icon narrow-only"
        onClick=${onMenu}
        aria-expanded=${navOpen}
        aria-controls="rail"
        aria-label="Open the menu"
        title="Sections"
      >
        ☰
      </button>
      <h1>${title}</h1>
      <div class="spacer"></div>
      ${error && html`<span class="pill err" title=${error.message}>${error.code}</span>`}
      <kbd class="wide-only" title="Jump to a position, correlation id, effect or view">⌘K</kbd>
      <button
        type="button"
        class="btn icon narrow-only"
        onClick=${onSearch}
        aria-label="Jump to a position, correlation id, effect or view"
        title="Jump to…"
      >
        <svg viewBox="0 0 16 16" aria-hidden="true">
          <circle cx="7" cy="7" r="4.25" fill="none" stroke="currentColor" stroke-width="1.6" />
          <path
            d="M10.4 10.4 14 14"
            fill="none"
            stroke="currentColor"
            stroke-width="1.6"
            stroke-linecap="round"
          />
        </svg>
      </button>
      ${status &&
      html`<span class="tiny faint mono wide-only">${plural(status.log_head, 'event')}</span>`}
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
  const narrow = useNarrow()
  const [navOpen, setNavOpen] = useState(false)
  const [palette, setPalette] = useState(false)
  const View = location.route ? VIEWS[location.route.id] : null
  const drawn = narrow && navOpen

  useEffect(() => {
    document.title = location.route ? `${location.route.title} · hekla` : 'hekla'
  }, [location.route?.id])

  /* A tap on a section is a navigation and a dismissal at once. Leaving the drawer up
   * over the view it just opened would make every link in it a two-step. */
  useEffect(() => {
    setNavOpen(false)
  }, [location.pathname])

  useEffect(() => {
    if (!drawn) return
    const onKey = (event) => {
      if (event.key === 'Escape') setNavOpen(false)
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [drawn])

  return html`
    <div class=${drawn ? 'shell nav-open' : 'shell'}>
      <${Rail}
        location=${location}
        status=${status}
        drawer=${narrow}
        open=${navOpen}
        onClose=${() => setNavOpen(false)}
      />
      <div class="main" inert=${drawn ? true : undefined}>
        <${Header}
          location=${location}
          status=${status}
          error=${error}
          navOpen=${navOpen}
          onMenu=${() => setNavOpen(true)}
          onSearch=${() => setPalette(true)}
        />
        <main class="content">
          <${Boundary} key=${location.route?.id ?? 'none'}>
            ${View
              ? html`<${View} params=${location.route.params} search=${location.search} />`
              : html`<${NotFound} pathname=${location.pathname} />`}
          <//>
        </main>
      </div>
      ${drawn && html`<div class="nav-scrim" onClick=${() => setNavOpen(false)}></div>`}
      <${Palette} status=${status} open=${palette} onOpenChange=${setPalette} />
    </div>
  `
}

render(html`<${App} />`, document.getElementById('root'))
