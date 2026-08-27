/* Typed confirmation for the two operator actions the console can take.
 *
 * `replay` and `skip` are the only writes here, and both are consequential: a replay
 * tears down and rebuilds a read model, and a skip advances an effect past an event it
 * never processed, permanently. Neither is undoable, so neither is one click away.
 * Typing the module's name is the standard shape for this because it makes the
 * confirmation specific: you cannot muscle-memory your way through it on the wrong
 * row. */

import { html, useEffect, useRef, useState } from './vendor-preact.js'

export function Confirm({ title, confirmWord, danger, children, onCancel, onConfirm }) {
  const [typed, setTyped] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState(null)
  const field = useRef(null)
  /* The parent re-renders on every 3s status poll and hands down a fresh `onCancel`
   * arrow each time. Reading it through a ref keeps it out of the dependency arrays
   * below, so neither effect re-runs on a tick. Without that, focus was pulled back
   * into the text field every three seconds and the confirm button could never be
   * reached from the keyboard. */
  const latest = useRef({ onCancel, busy })
  latest.current = { onCancel, busy }

  // Focus once, when the dialog opens.
  useEffect(() => {
    field.current?.focus()
  }, [])

  useEffect(() => {
    const onKey = (event) => {
      if (event.key === 'Escape' && !latest.current.busy) latest.current.onCancel()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [])

  const ready = typed === confirmWord && !busy

  const go = async () => {
    if (!ready) return
    setBusy(true)
    setError(null)
    try {
      await onConfirm()
    } catch (err) {
      /* Stay open on failure with the real code showing. Closing would leave the
       * operator unsure whether the action landed, which is the worst outcome for
       * something that is not undoable. */
      setError(err)
      setBusy(false)
    }
  }

  return html`
    <div class="modal-scrim" onClick=${() => !busy && onCancel()}>
      <div
        class="modal"
        role="dialog"
        aria-modal="true"
        aria-label=${title}
        onClick=${(event) => event.stopPropagation()}
      >
        <h2>${title}</h2>
        <div class="tiny dim">${children}</div>
        <label class="confirm-field">
          <span class="tiny">
            Type <code>${confirmWord}</code> to confirm
          </span>
          <input
            ref=${field}
            value=${typed}
            disabled=${busy}
            onInput=${(event) => setTyped(event.target.value)}
            onKeyDown=${(event) => event.key === 'Enter' && go()}
            aria-label=${`Type ${confirmWord} to confirm`}
            spellcheck="false"
            autocomplete="off"
          />
        </label>
        ${error &&
        html`
          <p class="tiny" style=${{ color: 'var(--err)' }}>
            <code>${error.code}</code> ${error.message}
          </p>
        `}
        <div class="modal-actions">
          <button type="button" class="btn" onClick=${onCancel} disabled=${busy}>Cancel</button>
          <button
            type="button"
            class=${danger ? 'btn danger' : 'btn primary'}
            onClick=${go}
            disabled=${!ready}
          >
            ${busy ? 'Working…' : title}
          </button>
        </div>
      </div>
    </div>
  `
}
