# Vendored third-party code

`vendor-preact.js` is committed rather than fetched, so the admin console works with
no network at all. That is the point of embedding it in the binary: an operator
debugging a wedged effect on an air-gapped host should not need a CDN to see the
journal. It is not served from `/admin/assets/` by accident; it is in the asset
table like every other file.

| | |
|---|---|
| Source | `https://cdn.jsdelivr.net/npm/htm@3.1.1/preact/standalone.module.js` |
| Retrieved | 2026-08-27 |
| Size | 13,194 bytes |
| SHA-256 | `72284e8e9079c87817145df1110f74e8a2aa040b2fc384922e18dfcb46fc1fd7` |

One file, bundling three packages:

| Package | Licence |
|---|---|
| [Preact](https://github.com/preactjs/preact) | MIT |
| `preact/hooks` | MIT |
| [htm](https://github.com/developit/htm) | Apache-2.0 |

It exports `html`, `render`, `h`, `Component`, `createContext`, and the hooks
(`useState`, `useReducer`, `useEffect`, `useLayoutEffect`, `useRef`,
`useImperativeHandle`, `useMemo`, `useCallback`, `useContext`, `useDebugValue`,
`useErrorBoundary`).

## Why this and not a build step

htm parses JSX-shaped markup out of tagged template literals at runtime, so the
console gets a real component model with real diffing and hekla keeps `cargo build`
as its only build. Svelte and a JSX-based React both need a compiler, which would
mean npm in a repository that has no JavaScript tooling at all.

## Updating it

Re-fetch the same path at a new version, update the table above, and run the test
suite: `tests/ui.rs` pins that every asset serves and that the shell references only
files the binary carries, which is what catches a bundle whose exports moved.
