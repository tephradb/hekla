/* System: what this process is, where it keeps its data, and what it was configured
 * with. The configuration panel is the one that earns its place: `Runtime` used to
 * discard the config at boot, so "is this deployment actually running with the
 * hekla.toml I think it is" was unanswerable without a restart. */

import { html } from './vendor-preact.js'
import { api } from './api.js'
import { Resource, useResource } from './ui-states.js'
import { count, duration, shortHash } from './format.js'

function Panel({ title, children }) {
  return html`
    <section class="card">
      <header>${title}</header>
      <div class="body"><dl class="kv">${children}</dl></div>
    </section>
  `
}

function Row({ label, children }) {
  return html`
    <dt>${label}</dt>
    <dd>${children}</dd>
  `
}

/* The config arrives as the nested tables of hekla.toml. Flattening to `table.key`
 * keeps it honest: an operator comparing this against the file on disk is looking for
 * the exact key they wrote. */
function flatten(config, prefix = '') {
  const out = []
  for (const [key, value] of Object.entries(config ?? {})) {
    const name = prefix ? `${prefix}.${key}` : key
    if (value && typeof value === 'object' && !Array.isArray(value)) out.push(...flatten(value, name))
    else out.push([name, value])
  }
  return out
}

export function SystemView() {
  const state = useResource((signal) => api.system(signal), [])

  return html`
    <${Resource} state=${state}>
      ${(system) => html`
        <div class="grid">
          <${Panel} title="Process">
            <${Row} label="version">${system.version}<//>
            <${Row} label="uptime">${duration(system.uptime_seconds * 1000)}<//>
            <${Row} label="verify">
              ${system.verify
                ? html`<span class="pill ok">on</span>`
                : html`<span class="pill mute">off</span>`}
            <//>
          <//>

          <${Panel} title="Storage">
            <${Row} label="log head">${count(system.log_head)}<//>
            <${Row} label="data dir">${system.data_dir}<//>
            <${Row} label="opdb">schema v${system.opdb_schema_version}<//>
          <//>

          <${Panel} title="Keystore">
            <${Row} label="configured">
              ${system.keystore.configured
                ? html`<span class="pill ok">yes</span>`
                : html`<span class="pill mute">no</span>`}
            <//>
            <${Row} label="masters">${system.keystore.master_key_ids.length || '0'}<//>
            ${system.keystore.master_key_ids.map(
              (id) => html`<${Row} label="key">${shortHash(id)}<//>`,
            )}
            ${system.keystore.master_key_ids.length > 1 &&
            html`
              <dt></dt>
              <dd class="tiny dim">
                More than one master means a rotation started and has not finished.
              </dd>
            `}
          <//>
        </div>

        <section class="card">
          <header>
            Configuration
            <span class="spacer" style=${{ flex: 1 }}></span>
            <span class="tiny faint" style=${{ textTransform: 'none', letterSpacing: 0 }}>
              the effective values, defaults included
            </span>
          </header>
          <div class="body">
            <dl class="kv">
              ${flatten(system.config).map(
                ([key, value]) => html`
                  <dt>${key}</dt>
                  <dd>${typeof value === 'number' ? count(value) : String(value)}</dd>
                `,
              )}
            </dl>
          </div>
        </section>
      `}
    <//>
  `
}
