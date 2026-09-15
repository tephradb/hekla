/* A source editor for heklang, hand-written like the rest of the console.
 *
 * A `<textarea>` is the only thing in a browser that edits text properly: it brings
 * undo, selection, IME, spellcheck-off, accessibility and every key a person expects,
 * and none of that is worth reimplementing to get colour. So the colour goes behind it.
 * A `<pre>` paints the highlighted source, the textarea sits on top with transparent
 * text and a visible caret, and the two stay aligned because they share a font, a
 * line-height, a padding and a `white-space: pre`.
 *
 * Not wrapping is what makes that reliable rather than fragile. With `pre` and a
 * horizontal scrollbar, one source line is one painted line forever, so a gutter number
 * is always beside its line and no measurement is involved.
 *
 * The tokens are a transcription of heklang 0.6.1's `src/lex.rs`, not a guess at them.
 * A word the lexer does not know renders as plain text, so a keyword added to the
 * language is a missing colour and never a broken editor. */

import { html, useEffect, useRef } from './vendor-preact.js'

/** Every word `Keyword::lookup` knows, in its order (`heklang/src/lex.rs`). */
const KEYWORDS = new Set([
  'as', 'command', 'const', 'delete', 'effect', 'else', 'emit', 'entity', 'enum',
  'event', 'false', 'fn', 'fold', 'for', 'guard', 'if', 'in', 'invalid', 'invoke',
  'let', 'none', 'on', 'patch', 'projector', 'put', 'record', 'refusal', 'reject',
  'return', 'test', 'true', 'update',
])

/* In the lexer's own precedence: a comment swallows the rest of its line, a triple
 * quote beats a single one, and a word is only a keyword after it fails to be anything
 * else. The closing quotes are optional so a string being typed highlights as one
 * instead of desynchronising the paint from the caret. */
const TOKEN =
  /(\/\/[^\n]*)|("""[\s\S]*?(?:"""|$)|"(?:[^"\\\n]|\\.)*"?)|(@[A-Za-z_][A-Za-z0-9_.]*)|(\d+(?:\.\d+)?)|([A-Za-z_][A-Za-z0-9_]*)/g

/** What one indent is, here and in every example in the docs. */
const INDENT = '  '

function paint(kind, text) {
  return html`<span class=${kind}>${text}</span>`
}

/**
 * Heklang source as an array of vnodes: spans for what the lexer recognises, bare
 * strings for everything else.
 *
 * Vnodes rather than a string of markup, so there is no escaping to get wrong. A `<`
 * in a string literal is a text node here and can never be a tag.
 */
export function highlight(source) {
  const out = []
  let last = 0
  let match
  TOKEN.lastIndex = 0
  while ((match = TOKEN.exec(source)) !== null) {
    if (match.index > last) out.push(source.slice(last, match.index))
    const [text, comment, string, path, number, word] = match
    if (comment) out.push(paint('hk-comment', text))
    else if (string) out.push(paint('hk-string', text))
    else if (path) out.push(paint('hk-path', text))
    else if (number) out.push(paint('hk-number', text))
    else if (KEYWORDS.has(word)) out.push(paint('hk-keyword', text))
    else out.push(text)
    last = match.index + text.length
  }
  out.push(source.slice(last))
  /* A `<pre>` drops one trailing newline and a textarea does not, so without this the
   * paint is one line short of the caret the moment the source ends in a return. */
  out.push('\n')
  return out
}

/** Where `line` starts in `source`, counting lines from one. */
function offsetOf(source, line) {
  let at = 0
  for (let seen = 1; seen < line; seen++) {
    const next = source.indexOf('\n', at)
    if (next < 0) return at
    at = next + 1
  }
  return at
}

/**
 * The editor.
 *
 * `markers` is a set of 1-based line numbers to light up in the gutter. `reveal` is
 * `{ line, column, nonce }` or null: the caret goes there whenever `nonce` changes,
 * which is what lets clicking the same diagnostic twice work the second time.
 */
export function SourceEditor({ value, onInput, onRun, markers, reveal, label, placeholder }) {
  const input = useRef(null)
  const painted = useRef(null)
  const gutter = useRef(null)
  const lines = value.split('\n').length

  /* The textarea is the only thing that scrolls; the other two are told where it got
   * to. Doing it the other way round means two scrollables racing each other. */
  const sync = () => {
    const box = input.current
    if (!box) return
    if (painted.current) {
      painted.current.scrollTop = box.scrollTop
      painted.current.scrollLeft = box.scrollLeft
    }
    if (gutter.current) gutter.current.scrollTop = box.scrollTop
  }

  useEffect(() => {
    if (!reveal || !input.current) return
    const box = input.current
    const start = offsetOf(box.value, reveal.line)
    const ends = box.value.indexOf('\n', start)
    box.focus()
    /* The whole line, not the reported column: a diagnostic points at where the
     * compiler noticed, which is often one token past where the fix goes, and a
     * selection that covers the line is right either way. */
    box.setSelectionRange(start, ends < 0 ? box.value.length : ends)
    sync()
  }, [reveal?.nonce])

  const keyed = (pressed) => {
    if (pressed.key === 'Enter' && (pressed.metaKey || pressed.ctrlKey)) {
      pressed.preventDefault()
      onRun?.()
      return
    }
    if (pressed.key === 'Escape') {
      /* Tab is taken below, so this gives keyboard navigation a way out: Escape, then
       * Tab. It also stops the window handler in `ui-panel.js`, which would otherwise
       * read an Escape aimed at the editor as one aimed at an open panel. */
      pressed.stopPropagation()
      pressed.target.blur()
      return
    }
    if (pressed.key !== 'Tab') return
    // Tab indents rather than leaving the box, which is what the Escape above is for.
    pressed.preventDefault()
    const box = pressed.target
    const { selectionStart: start, selectionEnd: end, value: text } = box
    const from = text.lastIndexOf('\n', start - 1) + 1
    const block = text.slice(from, end)

    if (pressed.shiftKey) {
      const outdented = block.replace(/^ {1,2}/gm, '')
      if (outdented === block) return
      onInput(text.slice(0, from) + outdented + text.slice(end))
      queueCaret(box, Math.max(from, start - INDENT.length), from + outdented.length)
      return
    }
    if (start === end) {
      onInput(text.slice(0, start) + INDENT + text.slice(start))
      queueCaret(box, start + INDENT.length, start + INDENT.length)
      return
    }
    const indented = block.replace(/^/gm, INDENT)
    onInput(text.slice(0, from) + indented + text.slice(end))
    queueCaret(box, start + INDENT.length, from + indented.length)
  }

  return html`
    <div class="hk-editor">
      <div class="hk-gutter" ref=${gutter} aria-hidden="true">
        ${Array.from({ length: lines }, (_, index) => {
          const number = index + 1
          return html`<div class=${markers?.has(number) ? 'bad' : undefined}>${number}</div>`
        })}
      </div>
      <div class="hk-scroll">
        <pre class="hk-paint" ref=${painted} aria-hidden="true">${highlight(value)}</pre>
        <textarea
          class="hk-input"
          ref=${input}
          value=${value}
          onInput=${(typed) => onInput(typed.target.value)}
          onScroll=${sync}
          onKeyDown=${keyed}
          aria-label=${label}
          placeholder=${placeholder}
          spellcheck="false"
          autocapitalize="off"
          autocomplete="off"
          autocorrect="off"
        ></textarea>
      </div>
    </div>
  `
}

/* The value is controlled, so the DOM has the old text until preact re-renders; putting
 * the caret back has to wait for that. A microtask is enough and, unlike an effect,
 * does not need the caret threaded through the parent's state to get here. */
function queueCaret(box, start, end) {
  queueMicrotask(() => box.setSelectionRange(start, end))
}
