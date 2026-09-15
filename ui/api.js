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
export async function request(path, { params, method = 'GET', body, headers, signal } = {}) {
  const init = { method, headers: { ...JSON_HEADERS, ...headers }, signal }
  if (body !== undefined) {
    init.headers['content-type'] = 'application/json'
    /* A string body is already JSON text and is posted verbatim. That is what keeps an
     * `Int` past 2^53 exact: parsing one into a JS number and re-serialising it would
     * round it silently, which is the same class of bug `arbitrary_precision` exists to
     * prevent on the way in. */
    init.body = typeof body === 'string' ? body : JSON.stringify(body)
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

  /* The rows themselves come from the public read API rather than from `/admin`, and
   * that is the point rather than a shortcut: what the console renders is what an
   * application sees over this port, decrypted columns and erased ones alike, not a
   * privileged view of the same table. It also means browsing costs the server exactly
   * what a client's own read costs it, with no second query path to keep honest. */
  rows: (projector, entity, { field, value, cursor, limit } = {}, signal) =>
    request(`/read/${encodeURIComponent(projector)}/${encodeURIComponent(entity)}`, {
      signal,
      params: [
        /* The filter is a parameter *named after the column*, so it is spread in
         * rather than named here. `hekla check` refuses an entity field that collides
         * with `limit`/`cursor`/`after`/`timeout_ms`, so this cannot shadow one. */
        ...(field && value ? [[field, value]] : []),
        ['cursor', cursor],
        ['limit', limit],
      ],
    }),

  row: (projector, entity, key, signal) =>
    request(
      `/read/${encodeURIComponent(projector)}/${encodeURIComponent(entity)}/${encodeURIComponent(key)}`,
      { signal },
    ),

  commands: (signal) => request('/admin/commands', { signal }),
  command: (name, signal) => request(`/admin/commands/${encodeURIComponent(name)}`, { signal }),

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

  /* The mutations. They live outside /admin because /admin is read-only by design; the
   * console drives them anyway, since seeing a system it cannot act on is half a tool.
   *
   * `run` is the application's own front door rather than an operator action, and it is
   * exactly as powerful here as `curl` is against the same port. `body` is JSON text,
   * not an object: see `request`. */
  run: (name, body, { idempotencyKey, correlationId } = {}) => {
    const headers = {}
    if (idempotencyKey) headers['idempotency-key'] = idempotencyKey
    if (correlationId) headers['x-correlation-id'] = correlationId
    return request(`/commands/${encodeURIComponent(name)}`, { method: 'POST', body, headers })
  },

  replay: (name) =>
    request(`/projectors/${encodeURIComponent(name)}/replay`, { method: 'POST' }),

  skip: (name, position) =>
    request(`/effects/${encodeURIComponent(name)}/skip/${position}`, { method: 'POST' }),

  /* Whether this deployment folds ad-hoc projections, and the ceilings it folds them
   * within. It answers with the feature off too, so the page can say what to turn on
   * rather than guessing from a 403 nobody can ask about. */
  projections: (signal) => request('/admin/projections', { signal }),

  /**
   * Fold one, watching it go.
   *
   * The console's only hand-rolled fetch, because it is the only response that arrives
   * in pieces and `request` above ends at `response.json()`. `Accept:
   * application/x-ndjson` opts into that; every other caller of this endpoint gets one
   * buffered body and never has to know.
   *
   * `onProgress` is called with `{ position, upto, events }` as the server reports it,
   * about ten times a second while events are matching and not at all while none are.
   * The resolved value is the projection itself: the last line of the stream, and the
   * same value the buffered shape returns.
   */
  project: async (source, params, { onProgress, signal } = {}) => {
    let response
    try {
      response = await fetch('/admin/projections' + query(params), {
        method: 'POST',
        headers: { accept: 'application/x-ndjson', 'content-type': 'text/plain' },
        body: source,
        signal,
      })
    } catch (err) {
      if (err.name === 'AbortError') throw err
      throw new ApiError(0, 'unreachable', 'hekla is not answering on this address')
    }
    /* Everything the request can get wrong is refused before the body opens, so a
     * failure here is still a status and an envelope, exactly like every other call. */
    if (!response.ok) throw await toError(response)

    const reader = response.body.getReader()
    const decoder = new TextDecoder()
    let pending = ''
    let last = null
    const take = (line) => {
      if (!line.trim()) return
      const message = JSON.parse(line)
      if (message.progress) onProgress?.(message.progress)
      else last = message
    }
    for (;;) {
      const { done, value } = await reader.read()
      if (done) break
      pending += decoder.decode(value, { stream: true })
      let breaks
      while ((breaks = pending.indexOf('\n')) >= 0) {
        take(pending.slice(0, breaks))
        pending = pending.slice(breaks + 1)
      }
    }
    /* The flush completes a multi-byte character split across the last two chunks, and
     * what is left over is a final line that arrived without its newline. hekla writes
     * one, but throwing away an answer that is sitting right there because a proxy
     * trimmed a byte would report a fold that worked as a fold that vanished. */
    pending += decoder.decode()
    take(pending)
    /* Told apart by their only top-level key, which works because all three schemas
     * are closed: a projection carries neither `progress` nor `error`. */
    if (last?.error) {
      throw new ApiError(200, last.error.code ?? 'internal', last.error.message ?? '', last)
    }
    if (!last) {
      /* The fold panicked: the sender dropped without writing an answer. The buffered
       * shape would have said 500, and a stream that stops has said the same thing. */
      throw new ApiError(500, 'internal', 'the projection stopped without answering')
    }
    return last
  },
}
