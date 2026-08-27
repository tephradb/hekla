/* Loading, empty and error states, plus the `useResource` hook every view loads
 * through.
 *
 * An empty state says *why* it is empty and an error state shows the real `error.code`
 * rather than a friendly paraphrase. This is a debugging tool: the code is the thing
 * an operator can look up, and hiding it behind "something went wrong" would waste the
 * one useful fact in the response. */

import { html, useEffect, useRef, useState } from './vendor-preact.js'

export function Skeleton({ rows = 5 }) {
  return html`
    <div class="skeleton-stack" aria-busy="true" aria-label="loading">
      ${Array.from(
        { length: rows },
        (_, index) =>
          html`<div class="skeleton" style=${{ width: `${88 - index * 9}%` }}></div>`,
      )}
    </div>
  `
}

export function Empty({ title, children }) {
  return html`
    <div class="empty">
      <h3>${title}</h3>
      <div class="tiny">${children}</div>
    </div>
  `
}

export function ErrorState({ error, onRetry }) {
  if (!error) return null
  return html`
    <div class="error-state" role="alert">
      <h3>${error.status ? `${error.status} ${error.code}` : error.code}</h3>
      <p class="tiny">${error.message}</p>
      ${onRetry &&
      html`<button type="button" class="btn" onClick=${onRetry}>Try again</button>`}
    </div>
  `
}

/**
 * Load `fetcher` whenever `deps` change.
 *
 * Aborts the previous request on every change, which is what keeps a fast click
 * through a list from rendering an earlier page's response over a later one, and
 * keeps a slow page from holding a connection open after the user has left it.
 */
export function useResource(fetcher, deps) {
  const [state, setState] = useState({ data: null, error: null, loading: true })
  const [attempt, setAttempt] = useState(0)
  const latest = useRef(0)

  useEffect(() => {
    const controller = new AbortController()
    const generation = ++latest.current
    setState((current) => ({ ...current, loading: true }))
    fetcher(controller.signal)
      .then((data) => {
        if (generation === latest.current) setState({ data, error: null, loading: false })
      })
      .catch((err) => {
        if (err.name === 'AbortError' || generation !== latest.current) return
        setState({ data: null, error: err, loading: false })
      })
    return () => controller.abort()
    // eslint-disable-next-line
  }, [...deps, attempt])

  return { ...state, reload: () => setAttempt((value) => value + 1) }
}

/**
 * The usual three-way render. Keeps stale data on screen while a poll refetches, so
 * a live view does not flash a skeleton every few seconds.
 */
export function Resource({ state, empty, children }) {
  if (state.error) return html`<${ErrorState} error=${state.error} onRetry=${state.reload} />`
  if (state.loading && !state.data) return html`<${Skeleton} />`
  if (!state.data) return null
  const blank = empty?.(state.data)
  if (blank) return blank
  return children(state.data)
}
