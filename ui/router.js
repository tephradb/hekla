/* Client-side routing over the History API.
 *
 * The route table below mirrors the server's own, path for path, because the server
 * serves this shell from every /admin URL. That is the whole trick: a deep link is a
 * real URL that also answers as JSON, the back button works, and nothing needs a `#`.
 *
 * A route's `view` is resolved by app.js; this module only decides which one and with
 * what parameters. */

import { useEffect, useState } from './vendor-preact.js'

/** Every view, in sidebar order. `path` is a pattern; `{name}` captures one segment. */
export const ROUTES = [
  { id: 'overview', path: '/admin', title: 'Overview' },
  { id: 'events', path: '/admin/events', title: 'Events' },
  { id: 'events', path: '/admin/events/{position}', title: 'Events' },
  { id: 'trace', path: '/admin/traces/{correlation_id}', title: 'Trace' },
  { id: 'effects', path: '/admin/effects', title: 'Effects' },
  { id: 'effects', path: '/admin/effects/{name}', title: 'Effects' },
  { id: 'effects', path: '/admin/effects/{name}/invocations', title: 'Effects' },
  {
    id: 'effects',
    path: '/admin/effects/{name}/invocations/{position}',
    title: 'Effects',
  },
  { id: 'projectors', path: '/admin/projectors', title: 'Projectors' },
  { id: 'projectors', path: '/admin/projectors/{name}', title: 'Projectors' },
  { id: 'schema', path: '/admin/schema', title: 'Schema' },
  { id: 'subjects', path: '/admin/subjects', title: 'Subjects' },
  { id: 'subjects', path: '/admin/subjects/{field}/{value}', title: 'Subjects' },
  { id: 'system', path: '/admin/system', title: 'System' },
]

/**
 * What the sidebar lists: the first route per view that is a real address.
 *
 * A pattern is not somewhere you can go. Trace exists only as
 * `/admin/traces/{correlation_id}`, so it is reached from an event's correlation link
 * or the palette and has no sidebar entry; linking the template itself would navigate
 * to a literal `{correlation_id}` and 400.
 */
export const NAV = ROUTES.filter(
  (route, index) =>
    !route.path.includes('{') &&
    ROUTES.findIndex((other) => other.id === route.id && !other.path.includes('{')) === index,
)

function match(pattern, path) {
  const expected = pattern.split('/')
  const actual = path.split('/')
  if (expected.length !== actual.length) return null
  const params = {}
  for (let index = 0; index < expected.length; index++) {
    const part = expected[index]
    if (part.startsWith('{')) {
      /* A malformed percent-escape throws rather than returning anything, and a URL
       * carrying one is exactly the sort a human pastes or truncates. Falling back to
       * the raw segment keeps the console mounted and lets the API report what is
       * actually wrong with it; throwing here would take the whole page down, since
       * routing runs outside any error boundary. */
      let value
      try {
        value = decodeURIComponent(actual[index])
      } catch {
        value = actual[index]
      }
      if (!value) return null
      params[part.slice(1, -1)] = value
    } else if (part !== actual[index]) {
      return null
    }
  }
  return params
}

/** Resolve a pathname to a route, or `null` for a URL under /admin we do not serve. */
export function resolve(pathname) {
  const path = pathname.length > 1 ? pathname.replace(/\/+$/, '') : pathname
  for (const route of ROUTES) {
    const params = match(route.path, path)
    if (params) return { ...route, params }
  }
  return null
}

/** Navigate, pushing history. `replace` for changes that are not a new destination. */
export function go(url, { replace = false } = {}) {
  if (replace) history.replaceState(null, '', url)
  else history.pushState(null, '', url)
  window.dispatchEvent(new PopStateEvent('popstate'))
}

/** The current location, re-rendering on every navigation. */
export function useLocation() {
  const [location, setLocation] = useState(() => read())

  useEffect(() => {
    const onPop = () => setLocation(read())
    window.addEventListener('popstate', onPop)
    return () => window.removeEventListener('popstate', onPop)
  }, [])

  return location
}

function read() {
  return {
    pathname: window.location.pathname,
    search: new URLSearchParams(window.location.search),
    route: resolve(window.location.pathname),
  }
}

/**
 * Replace the query string without touching the path, for view state that should be
 * shareable and survive a reload (a filter, an open row) but is not a destination.
 */
export function setQuery(entries, { replace = true } = {}) {
  const search = new URLSearchParams()
  for (const [key, value] of entries) {
    if (value !== undefined && value !== null && value !== '') search.append(key, value)
  }
  const rendered = search.toString()
  go(window.location.pathname + (rendered ? `?${rendered}` : ''), { replace })
}
