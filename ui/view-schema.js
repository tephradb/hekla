/* Schema: the loaded project as a graph.
 *
 * Commands append events; projectors and effects subscribe to them. Both halves are
 * declared, so both edges are drawn from `/admin/schema` and are exact.
 *
 * The edge that is *not* drawn is effect-invokes-command: `/admin/schema` reports each
 * module's subscription and not its call targets, and the journal records each call's
 * result rather than its arguments, so a completed `invoke` does not carry the name
 * either. The page says so rather than drawing an edge it would have to guess at. */

import { html } from './vendor-preact.js'
import { api } from './api.js'
import { Empty, Resource, useResource } from './ui-states.js'
import { Copy } from './ui-copy.js'
import { shortHash, sources, stamp } from './format.js'

export function SchemaView() {
  const state = useResource((signal) => api.schema(signal), [])

  return html`
    <${Resource} state=${state}>
      ${(schema) => {
        const consumers = (type) => [
          ...schema.projectors
            .filter((p) => p.sources.includes(type))
            .map((p) => ({ kind: 'projector', name: p.name })),
          ...schema.effects
            .filter((e) => e.sources.includes(type))
            .map((e) => ({ kind: 'effect', name: e.name })),
        ]
        return html`
          <section class="card">
            <header>
              Events
              <div style=${{ flex: 1 }}></div>
              <span class="tiny faint" style=${{ textTransform: 'none', letterSpacing: 0 }}>
                ${schema.events.length} declared
              </span>
            </header>
            ${schema.events.length === 0
              ? html`<${Empty} title="No events">This project declares none.<//>`
              : html`
                  <div class="body graph">
                    ${schema.events.map((event) => {
                      const subscribers = consumers(event.type)
                      const encrypted = event.fields.filter((f) => f.subject).length
                      return html`
                        <div class="graph-row" key=${event.type}>
                          <div class="graph-event">
                            <code>${event.type}</code>
                            <span class="tiny faint">
                              ${event.fields.length} fields
                              ${encrypted > 0 ? ` · ${encrypted} encrypted` : ''}
                            </span>
                          </div>
                          <div class="graph-arrow" aria-hidden="true">→</div>
                          <div class="graph-consumers">
                            ${subscribers.length === 0 &&
                            html`<span class="tiny faint">nothing subscribes</span>`}
                            ${subscribers.map(
                              (consumer) => html`
                                <span class="pill ${consumer.kind === 'effect' ? 'info' : 'mute'}">
                                  ${consumer.kind === 'effect' ? '⚡' : '▣'} ${consumer.name}
                                </span>
                              `,
                            )}
                          </div>
                        </div>
                        <details class="graph-fields">
                          <summary class="tiny dim">fields</summary>
                          <dl class="kv tiny">
                            ${event.fields.map(
                              (field) => html`
                                <dt>${field.name}</dt>
                                <dd>
                                  ${field.kind}
                                  ${field.subject &&
                                  html`<span class="pill warn">subject ${field.subject}</span>`}
                                  ${field.unique && html`<span class="pill info">unique</span>`}
                                </dd>
                              `,
                            )}
                          </dl>
                        </details>
                      `
                    })}
                  </div>
                `}
          </section>

          <section class="card">
            <header>Commands</header>
            <table class="data">
              <thead>
                <tr>
                  <th scope="col">Name</th>
                  <th scope="col" style=${{ width: '90px' }}>Routed</th>
                  <th scope="col">Input</th>
                  <th scope="col" style=${{ width: '110px' }}>Source</th>
                </tr>
              </thead>
              <tbody>
                ${schema.commands.map(
                  (command) => html`
                    <tr key=${command.name}>
                      <td><code>${command.name}</code></td>
                      <td>
                        ${command.internal
                          ? html`<span class="pill mute" title="not routed over HTTP; invoked only by an effect">
                              internal
                            </span>`
                          : html`<span class="pill ok">public</span>`}
                      </td>
                      <td class="tiny dim mono">
                        ${command.input.map((field) => `${field.name}: ${field.kind}`).join(', ') ||
                        '-'}
                      </td>
                      <td class="tiny dim mono">${shortHash(command.source_hash)}</td>
                    </tr>
                  `,
                )}
              </tbody>
            </table>
          </section>

          <section class="card">
            <header>Effects</header>
            <div class="body">
              ${schema.effects.length === 0 && html`<span class="tiny faint">none declared</span>`}
              ${schema.effects.map(
                (effect) => html`
                  <div class="effect-edges row wrap" key=${effect.name}>
                    <span class="pill info">⚡ ${effect.name}</span>
                    <span class="tiny faint">subscribes to ${sources(effect.sources)}</span>
                  </div>
                `,
              )}
              ${schema.effects.length > 0 &&
              html`
                <p class="tiny faint" style=${{ marginBottom: 0 }}>
                  Which commands an effect invokes is not shown here: the journal records
                  each call's result rather than its arguments, so a completed
                  <code>invoke</code> does not carry the name either. An invocation's
                  journal shows <em>that</em> one happened and what came back.
                </p>
              `}
            </div>
          </section>

          <section class="card">
            <header>Modules</header>
            <table class="data">
              <thead>
                <tr>
                  <th scope="col">Name</th>
                  <th scope="col" style=${{ width: '110px' }}>Kind</th>
                  <th scope="col" style=${{ width: '140px' }}>Source hash</th>
                  <th scope="col">Loaded</th>
                </tr>
              </thead>
              <tbody>
                ${schema.modules.map(
                  (module) => html`
                    <tr key=${module.name}>
                      <td><code>${module.name}</code></td>
                      <td><span class="pill mute">${module.kind}</span></td>
                      <td class="mono tiny row">
                        ${shortHash(module.source_hash)}<${Copy} value=${module.source_hash} />
                      </td>
                      <td class="tiny dim">${stamp(module.loaded_at)}</td>
                    </tr>
                  `,
                )}
              </tbody>
            </table>
          </section>
        `
      }}
    <//>
  `
}

