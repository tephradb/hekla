/* A colour for a correlation id.
 *
 * The colour answers one question at a glance: which of these rows belong to the same
 * causal chain. That makes stability the property that matters, and it rules out the
 * obvious implementation. A palette indexed by row position repaints the whole table
 * the moment one event is appended, so the colour a chain wore a second ago is now the
 * colour of the chain below it, and a reader who learned it has learned a lie. The
 * colour here is a pure function of the id and of nothing else, so a chain keeps it
 * across pages, across polls, and across views.
 *
 * A pure function into a finite palette collides, and no amount of folding the row's
 * position back in avoids that without giving the stability away again: a run of five
 * events under one id followed by an event under another will sometimes draw the same
 * hue. So grouping is not the colour's job here. The rail draws one capsule per run of
 * consecutive rows sharing an id and breaks between runs (`.corr-rail` in style.css),
 * which is exact where the hue is a guess, and hovering the column dims every chain
 * but the one under the pointer, which settles anything the seam leaves open.
 *
 * Only the hue is derived. Lightness and chroma are tokens, so this module never has
 * to know which theme is on and every id lands on a colour that was chosen rather than
 * hashed: sampling a whole RGB triple is where the muddy browns and the greys that
 * disappear into the background come from.
 */

/* Sixteen 22.5° steps. Enough that two chains meeting on the page rarely share one,
 * few enough that no two steps read as the same colour, which is the failure worth
 * avoiding: two chains that look similar cost a second look, two that look identical
 * cost a wrong answer. */
const BUCKETS = 16

/** FNV-1a, for spreading uuids evenly over the buckets. Not a hash for keeping secrets. */
function fold(text) {
  let hash = 0x811c9dc5
  for (let index = 0; index < text.length; index++) {
    hash ^= text.charCodeAt(index)
    hash = Math.imul(hash, 0x01000193)
  }
  return hash >>> 0
}

/**
 * The hue angle an id hashes to, in degrees.
 *
 * The hash is folded onto itself before the bucket is taken rather than being read
 * straight off the bottom. FNV-1a's low bits carry less of the input than its high
 * ones, and a correlation id is not always a random uuid: `x-correlation-id` is a
 * request header, so a caller is free to hand over its own scheme. Sixteen ids that
 * differ in only a nibble each reached five of the sixteen hues taking the low bits
 * and eleven folded, and the fold costs nothing on ids that were random anyway.
 *
 * The id is hashed exactly as the log records it, case included. Two spellings of one
 * uuid are two correlation ids to every query hekla answers, so colouring them alike
 * would have the console claim a chain the tag index does not.
 */
export function hue(correlationId) {
  const hash = fold(String(correlationId ?? ''))
  /* `^` works on signed 32-bit ints, so the fold lands negative half the time and JS
   * takes its remainder with the dividend's sign. The bucket is the same either way
   * and so is the colour, since CSS reads a hue modulo 360, but back to unsigned so
   * that what the element carries is one of the sixteen angles and not `-292.5`. */
  return ((((hash >>> 16) ^ hash) >>> 0) % BUCKETS) * (360 / BUCKETS)
}

/**
 * The inline style that hands an element its correlation's hue. The stylesheet builds
 * the colour from it, so callers set this and then name `var(--corr)`, or name nothing
 * and let `.corr` do it.
 */
export function tint(correlationId) {
  return { '--corr-hue': String(hue(correlationId)) }
}

/**
 * The rail's classes for one row of a list in log order.
 *
 * Consecutive rows sharing an id draw one unbroken capsule, so the ends are what mark
 * where a chain starts and stops. `rows` is the rendered page: a run that continues
 * onto the next page still closes here, which is the honest thing to draw, because
 * this page is all there is to see.
 */
export function railClass(rows, index) {
  const id = rows[index]?.correlation_id
  const opens = index === 0 || rows[index - 1]?.correlation_id !== id
  const closes = index === rows.length - 1 || rows[index + 1]?.correlation_id !== id
  return `corr-rail${opens ? ' opens' : ''}${closes ? ' closes' : ''}`
}
