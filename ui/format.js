/* Formatting. Every function here is pure and takes the awkward cases seriously,
 * because an admin console that rounds a number or guesses at a missing value is
 * worse than one that shows the raw field. */

const NUMBER = new Intl.NumberFormat()

/** A log position or count, with thousands separators. */
export function count(value) {
  return typeof value === 'number' ? NUMBER.format(value) : '-'
}

/** The first 8 characters of a uuid, which is what a human matches on. */
export function shortId(value) {
  if (typeof value !== 'string') return '-'
  return value.length > 12 ? `${value.slice(0, 8)}…` : value
}

/** A source hash, abbreviated the way git abbreviates one. */
export function shortHash(value) {
  return typeof value === 'string' ? value.slice(0, 8) : '-'
}

/** `12:04:31.24`, the local wall clock at millisecond resolution. */
export function clock(iso) {
  const at = new Date(iso)
  if (Number.isNaN(at.getTime())) return '-'
  const time = at.toLocaleTimeString(undefined, { hour12: false })
  return `${time}.${String(at.getMilliseconds()).padStart(3, '0').slice(0, 2)}`
}

/** The full instant, for a detail view where precision beats brevity. */
export function stamp(iso) {
  const at = new Date(iso)
  return Number.isNaN(at.getTime()) ? (iso ?? '-') : at.toISOString()
}

/** `4m ago`, `just now`, `in 12s`. Coarse on purpose: this is a glance, not a metric. */
export function ago(iso) {
  const at = new Date(iso)
  if (Number.isNaN(at.getTime())) return ''
  const seconds = Math.round((Date.now() - at.getTime()) / 1000)
  const future = seconds < 0
  const magnitude = Math.abs(seconds)
  if (magnitude < 5) return 'just now'
  const rendered = duration(magnitude * 1000)
  return future ? `in ${rendered}` : `${rendered} ago`
}

/** `1.4s`, `2m 14s`, `3h 02m`. */
export function duration(ms) {
  if (typeof ms !== 'number' || !Number.isFinite(ms)) return '-'
  if (ms < 1000) return `${Math.round(ms)}ms`
  const seconds = Math.floor(ms / 1000)
  if (seconds < 60) return `${(ms / 1000).toFixed(1)}s`
  const minutes = Math.floor(seconds / 60)
  if (minutes < 60) return `${minutes}m ${String(seconds % 60).padStart(2, '0')}s`
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return `${hours}h ${String(minutes % 60).padStart(2, '0')}m`
  return `${Math.floor(hours / 24)}d ${String(hours % 24).padStart(2, '0')}h`
}

/** An offset from the start of a trace: `+0ms`, `+292ms`, `+1.4s`. */
export function offset(ms) {
  return ms <= 0 ? '+0ms' : `+${duration(ms)}`
}

/** Bytes, for a payload size. */
export function bytes(value) {
  if (typeof value !== 'number') return '-'
  const units = ['B', 'KB', 'MB', 'GB']
  let size = value
  let unit = 0
  while (size >= 1024 && unit < units.length - 1) {
    size /= 1024
    unit++
  }
  return `${unit === 0 ? size : size.toFixed(1)}${units[unit]}`
}

/**
 * A module's subscription, in the author's own vocabulary.
 *
 * `null` is `all_events()` and `[]` is a module subscribed to nothing. Two different
 * facts, and collapsing them would invert the meaning of the commonest subscription,
 * so the server keeps them apart and so does this.
 */
export function sources(value) {
  if (value === null || value === undefined) return 'all_events()'
  if (value.length === 0) return 'nothing'
  return value.join(' · ')
}
