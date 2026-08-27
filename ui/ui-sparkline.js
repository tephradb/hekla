/* An inline SVG sparkline.
 *
 * hekla has no metrics endpoint and this does not pretend otherwise: the series is
 * bucketed client-side from the timestamps on one page of events, so it describes the
 * recent tail of the log and nothing more. Labelled as such where it is used, because
 * a chart that looks like a metric and is not one is worse than no chart. */

import { html } from './vendor-preact.js'

const WIDTH = 120
const HEIGHT = 26

/** Bucket ISO timestamps into `buckets` equal slices ending now. */
export function bucket(timestamps, windowMs, buckets = 24) {
  const now = Date.now()
  const counts = new Array(buckets).fill(0)
  const size = windowMs / buckets
  for (const iso of timestamps) {
    const at = new Date(iso).getTime()
    if (Number.isNaN(at)) continue
    const age = now - at
    if (age < 0 || age > windowMs) continue
    const index = buckets - 1 - Math.floor(age / size)
    if (index >= 0 && index < buckets) counts[index]++
  }
  return counts
}

export function Sparkline({ values, label }) {
  if (!values?.length) return null
  const peak = Math.max(...values, 1)
  const step = WIDTH / values.length

  /* An area rather than a line: at this size a 1px stroke over a handful of buckets
   * reads as noise, while a filled shape reads as shape. */
  const points = values
    .map((value, index) => `${(index * step).toFixed(1)},${(HEIGHT - (value / peak) * HEIGHT).toFixed(1)}`)
    .join(' ')

  return html`
    <svg
      class="spark"
      viewBox=${`0 0 ${WIDTH} ${HEIGHT}`}
      preserveAspectRatio="none"
      role="img"
      aria-label=${label ?? `peak ${peak} per bucket`}
    >
      <polygon points=${`0,${HEIGHT} ${points} ${WIDTH},${HEIGHT}`} fill="currentColor" opacity="0.18" />
      <polyline points=${points} fill="none" stroke="currentColor" stroke-width="1.5" />
    </svg>
  `
}
