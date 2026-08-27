/* The shared `/status` poll.
 *
 * One poll for the whole console rather than one per view: `/status` already carries
 * the log head, every projector's readiness and every effect's state, which is what
 * the sidebar badges, the header and half the views need. A view that wants its own
 * data subscribes to the tick and refetches.
 *
 * It pauses when the tab is hidden. A console left open on a second monitor should
 * not be a background load on the process it is watching. */

import { useEffect, useState } from './vendor-preact.js'
import { api } from './api.js'

const INTERVAL = 3000
const KEY = 'hekla.live'

let state = { status: null, error: null, tick: 0 }
let timer = null
let inFlight = null
let queued = null
const listeners = new Set()

function emit() {
  for (const listener of listeners) listener(state)
}

function set(next) {
  state = { ...state, ...next }
  emit()
}

async function poll() {
  try {
    set({ status: await api.status(), error: null, tick: state.tick + 1 })
  } catch (err) {
    set({ error: err, tick: state.tick + 1 })
  }
}

function refresh() {
  /* Overlapping polls would queue up behind a slow process and make it slower. */
  if (inFlight) return inFlight
  inFlight = poll().finally(() => {
    inFlight = null
  })
  return inFlight
}

function paused() {
  try {
    return localStorage.getItem(KEY) === 'off'
  } catch {
    return false
  }
}

function schedule() {
  clearInterval(timer)
  timer = null
  if (paused()) return
  timer = setInterval(() => {
    if (!document.hidden) refresh()
  }, INTERVAL)
}

/** Turn the poll on or off, remembered across reloads. */
export function setLive(on) {
  try {
    localStorage.setItem(KEY, on ? 'on' : 'off')
  } catch {
    /* The setting still applies to this page. */
  }
  schedule()
  if (on) refresh()
  else emit()
}

export function isLive() {
  return !paused()
}

/**
 * Refresh now, whatever the poll is doing.
 *
 * A scheduled poll already in flight was sent before whatever the caller just did, so
 * its answer is stale by construction. Dropping the manual refresh into it left the
 * sidebar and the badges showing pre-skip, pre-replay state until the next tick, and
 * made ⟳ look dead on a slow process. One follow-up covers any number of callers, so
 * a burst coalesces rather than queueing.
 */
export function refreshNow() {
  if (!inFlight) return refresh()
  if (!queued) {
    queued = inFlight.then(() => {
      queued = null
      return refresh()
    })
  }
  return queued
}

/**
 * The latest `/status`, the poll's own error, and a tick that increments on every
 * poll so a view can use it as a refetch dependency.
 */
export function useStatus() {
  const [snapshot, setSnapshot] = useState(state)

  useEffect(() => {
    listeners.add(setSnapshot)
    if (!state.status) refresh()
    /* Reconnect promptly when the tab comes back rather than waiting out the
     * interval: the first thing someone does after switching back is read the page. */
    const onVisible = () => {
      if (!document.hidden && isLive()) refresh()
    }
    document.addEventListener('visibilitychange', onVisible)
    return () => {
      listeners.delete(setSnapshot)
      document.removeEventListener('visibilitychange', onVisible)
    }
  }, [])

  return snapshot
}

schedule()
