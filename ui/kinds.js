/* Reading a declared kind.
 *
 * `/admin` reports a type as its author wrote it: `Uuid`, `String? @max(200)`,
 * `Money(2)?`, `(Low | Normal | Urgent)?`. That is the right thing to show, and it is
 * also everything a client needs to decide how to render a value or which control to
 * offer, so these three parsers are what stands between that string and a form.
 *
 * The grammar they lean on is small and fixed by `FieldKind::describe`: the type comes
 * first, an optional marker binds to it, and any constraint follows. A union is
 * bracketed when optional precisely so the marker has something to bind to. */

/* Every kind whose name is fixed. Anything else is an enum, whose rendering is its
 * variant list rather than its name, because the wire form does not carry the name. */
const PRIMITIVES = new Set(['String', 'Int', 'Bool', 'Uuid', 'Timestamp', 'Json'])

/**
 * The word that decides how a value is drawn: `String? @max(200)` is `String`,
 * `Money(2)?` stays `Money(2)`.
 *
 * The type comes first and any constraint follows it, so the first token is the whole
 * answer. An enum reduces to its first variant, which is not its kind; ask
 * [`enumVariants`] before trusting this on one.
 */
export function baseKind(kind) {
  return String(kind ?? '')
    .split(' ')[0]
    .replace(/\?$/, '')
}

/** Whether a value of this kind is read by its last digit, and so aligns right. */
export function numeric(kind) {
  const base = baseKind(kind)
  return base === 'Int' || base.startsWith('Money(')
}

/**
 * The variants of an enum kind, or `null` for anything else.
 *
 * Keyed on the kind *not* being a primitive rather than on the presence of a `|`, so a
 * single-variant enum is still an enum instead of quietly becoming a text field.
 */
export function enumVariants(kind) {
  const base = baseKind(kind)
  if (PRIMITIVES.has(base) || base.startsWith('Money(')) return null
  const text = String(kind ?? '').trim()
  const inner =
    text.startsWith('(') && text.endsWith(')?') ? text.slice(1, -2) : text.replace(/\?$/, '')
  return inner
    .split('|')
    .map((variant) => variant.trim())
    .filter(Boolean)
}

/* Past this many characters a kind has stopped being a label and become a sentence. */
const INLINE = 40

/**
 * A kind short enough to sit inline, next to a name or inside a table cell.
 *
 * A union's rendering *is* its variant list, because the wire form carries no name to
 * show instead, and that is right until the list is twelve long: `title:
 * DistributedSystems | ProgrammingLanguages | …` is 200 characters that push every
 * column after it off the page and squeeze a command's own inputs to nothing.
 *
 * Past the cap the list becomes its count. Nothing is lost, only moved: the form beside
 * it is a `select` holding every variant, the schema view still writes them out in
 * full, and every caller here keeps the whole string on the element's `title`.
 */
export function shortKind(kind) {
  const text = String(kind ?? '').trim()
  if (text.length <= INLINE) return text
  const variants = enumVariants(kind)
  // Nothing to summarise: a long non-union kind is long because it says something.
  if (!variants || variants.length < 2) return text
  const summary = `one of ${variants.length}`
  // The same bracket an optional union already needs, for the same reason: the marker
  // has to bind to the whole thing and not to the last word of it.
  return text.endsWith('?') ? `(${summary})?` : summary
}

/** The `@max(n)` a string kind declares, or `null` when it declares none. */
export function maxLength(kind) {
  const found = /@max\((\d+)\)/.exec(String(kind ?? ''))
  return found ? Number(found[1]) : null
}
