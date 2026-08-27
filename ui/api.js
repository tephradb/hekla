/* The JSON client.
 *
 * Every request names `application/json` explicitly. That is not decoration: the
 * server serves this same URL as the console's HTML shell to anything that asks for
 * `text/html`, and a bare `fetch()` sends `Accept: * / *`, so being explicit is what
 * keeps a data request a data request no matter how the negotiation rule changes.
 *
 * hekla has two error envelopes and one route family that answers before the handler
 * runs. All three are normalised here so no view has to know. */

const JSON_HEADERS = { accept: 'application/json' }

/** An error the console can render: a stable code, a message, and the status. */
export class ApiError extends Error {
  constructor(status, code, message, body) {
    super(message)
    this.name = 'ApiError'
    this.status = status
    this.code = code
    this.body = body
  }

  get notFound() {
    return this.status === 404
  }

  /* A projector that is rebuilding, stale, quarantined or not caught up answers 503
   * with a code naming which. Worth telling apart from a real failure: the answer is
   * to wait, not to debug. */
  get unavailable() {
    return this.status === 503
  }
}

/* `?a=1&a=2` is meaningful on /admin/events, so params arrive as pairs rather than an
 * object, and a null value drops rather than serialising as "null". */
function query(params) {
  const search = new URLSearchParams()
  for (const [key, value] of params ?? []) {
    if (value !== undefined && value !== null && value !== '') search.append(key, value)
  }
  const rendered = search.toString()
  return rendered ? `?${rendered}` : ''
}

async function toError(response) {
  const type = response.headers.get('content-type') ?? ''
  /* axum rejects a malformed integer path segment before the handler, so three routes
   * can answer 400 as plain text rather than the error envelope. Parsing that as JSON
   * would throw and lose the real status. */
  if (!type.includes('application/json')) {
    const text = await response.text().catch(() => '')
    return new ApiError(response.status, 'invalid_input', text.trim() || response.statusText)
  }
  const body = await response.json().catch(() => null)
  const detail = body?.error
  return new ApiError(
    response.status,
    detail?.code ?? 'internal',
    detail?.message ?? response.statusText,
    body,
  )
}

/**
 * One request. `signal` lets a view abandon a fetch when the user navigates away,
 * which matters most on the pages that are slow enough to be worth leaving.
 */
export async function request(path, { params, method = 'GET', body, signal } = {}) {
  const init = { method, headers: { ...JSON_HEADERS }, signal }
  if (body !== undefined) {
    init.headers['content-type'] = 'application/json'
    init.body = JSON.stringify(body)
  }
  let response
  try {
    response = await fetch(path + query(params), init)
  } catch (err) {
    if (err.name === 'AbortError') throw err
    /* A dead server and a dropped connection are the same thing to a browser, and
     * both mean the same thing to an operator: this process is not answering. */
    throw new ApiError(0, 'unreachable', 'hekla is not answering on this address')
  }
  if (!response.ok) throw await toError(response)
  if (response.status === 204) return null
  return response.json()
}

/* --- the surface, one function per endpoint ------------------------------
 *
 * Written out rather than composed from a path builder so `tests/ui.rs` can scan
 * these bytes for `/admin/...` literals and check every one against the router's own
 * table. A clever URL builder would defeat that. */

export const api = {
  status: (signal) => request('/status', { signal }),

  events: ({ types = [], tags = [], cursor, limit, direction }, signal) =>
    request('/admin/events', {
      signal,
      params: [
        ...types.map((type) => ['type', type]),
        ...tags.map((tag) => ['tag', tag]),
        ['cursor', cursor],
        ['limit', limit],
        ['direction', direction],
        /* A list renders no payload, so decrypting one would spend key unwraps on
         * fields nobody reads and would emit an audit line per page. The detail view
         * decrypts, and then one audit line means one operator read one event. */
        ['decrypt', 'false'],
      ],
    }),

  event: (position, { decrypt = true } = {}, signal) =>
    request(`/admin/events/${position}`, {
      signal,
      params: [['decrypt', String(decrypt)]],
    }),

  trace: (correlationId, { cursor, limit } = {}, signal) =>
    request(`/admin/traces/${correlationId}`, {
      signal,
      params: [
        ['cursor', cursor],
        ['limit', limit],
        ['decrypt', 'false'],
      ],
    }),

  effects: (signal) => request('/admin/effects', { signal }),
  effect: (name, signal) => request(`/admin/effects/${encodeURIComponent(name)}`, { signal }),

  invocations: (name, { cursor, limit } = {}, signal) =>
    request(`/admin/effects/${encodeURIComponent(name)}/invocations`, {
      signal,
      params: [
        ['cursor', cursor],
        ['limit', limit],
      ],
    }),

  invocation: (name, position, { cursor, limit } = {}, signal) =>
    request(`/admin/effects/${encodeURIComponent(name)}/invocations/${position}`, {
      signal,
      params: [
        ['cursor', cursor],
        ['limit', limit],
      ],
    }),

  projectors: (signal) => request('/admin/projectors', { signal }),

  projector: (name, { counts = false } = {}, signal) =>
    request(`/admin/projectors/${encodeURIComponent(name)}`, {
      signal,
      params: [['counts', String(counts)]],
    }),

  schema: (signal) => request('/admin/schema', { signal }),
  system: (signal) => request('/admin/system', { signal }),

  subjects: ({ afterField, afterValue, limit } = {}, signal) =>
    request('/admin/subjects', {
      signal,
      params: [
        ['after_field', afterField],
        ['after_value', afterValue],
        ['limit', limit],
      ],
    }),

  subject: (field, value, signal) =>
    request(`/admin/subjects/${encodeURIComponent(field)}/${encodeURIComponent(value)}`, {
      signal,
    }),

  /* The two mutations. They live outside /admin because /admin is read-only by
   * design; the console drives them anyway, since seeing a wedge and being unable to
   * clear it is half a tool. */
  replay: (name) =>
    request(`/projectors/${encodeURIComponent(name)}/replay`, { method: 'POST' }),

  skip: (name, position) =>
    request(`/effects/${encodeURIComponent(name)}/skip/${position}`, { method: 'POST' }),
}
