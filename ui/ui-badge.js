/* Status pills.
 *
 * The vocabulary is the server's, not the console's: `state` on an effect and
 * `readiness` on a projector are computed in Rust precisely so a client does not
 * re-derive "is this stuck" from three counters and get it wrong. This module maps
 * those words to colours and nothing else. */

import { html } from './vendor-preact.js'

/* `rebuild_failed` and `quarantined` are the two a projector cannot leave on its own,
 * so they are the two that read as errors rather than warnings. */
const READINESS = {
  ready: 'ok',
  rebuilding: 'warn',
  stale: 'warn',
  rebuild_failed: 'err',
  quarantined: 'err',
}

const EFFECT_STATE = {
  healthy: 'ok',
  lagging: 'warn',
  wedged: 'err',
  quarantined: 'err',
}

/* Every persisted invocation is `running` or `terminal`, and `terminal` covers
 * success, an operator skip and a terminal failure alike. Rendering it as success
 * would claim more than the row knows, so it stays neutral. */
const INVOCATION = {
  running: 'info',
  terminal: 'mute',
}

const SUBJECT = {
  decrypted: 'ok',
  encrypted: 'mute',
  erased: 'err',
  stale: 'warn',
  unreadable: 'warn',
}

const CALL_KIND = {
  http: 'info',
  invoke: 'ok',
  now: 'mute',
  erase: 'warn',
}

const MAPS = {
  readiness: READINESS,
  effect: EFFECT_STATE,
  invocation: INVOCATION,
  subject: SUBJECT,
  call: CALL_KIND,
}

export function Badge({ kind, value, title }) {
  if (value === null || value === undefined) {
    return html`<span class="pill mute plain" title=${title}>unknown</span>`
  }
  const tone = (MAPS[kind] ?? {})[value] ?? 'mute'
  return html`<span class=${`pill ${tone}`} title=${title}>${value}</span>`
}

/** An HTTP status from a journaled call, coloured by class. */
export function StatusCode({ status }) {
  if (typeof status !== 'number') return html`<span class="pill mute plain">-</span>`
  const tone = status < 300 ? 'ok' : status < 400 ? 'info' : status < 500 ? 'warn' : 'err'
  return html`<span class=${`pill ${tone} plain mono`}>${status}</span>`
}

/** Lag, worth colour only once it is large enough to mean something. */
export function Lag({ value }) {
  /* Tested the way `Badge` tests it: a falsy check also caught a missing or null lag
   * and drew `0`, claiming "fully caught up" about a projector whose lag is unknown. */
  if (value === null || value === undefined) {
    return html`<span class="pill mute plain">unknown</span>`
  }
  if (value === 0) return html`<span class="faint">0</span>`
  const tone = value > 1000 ? 'err' : value > 50 ? 'warn' : 'mute'
  return html`<span class=${`pill ${tone} plain mono`}>${value}</span>`
}
