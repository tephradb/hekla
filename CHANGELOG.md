# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.7.0](https://git.tqwewe.com/tephra/hekla/compare/v0.6.0...v0.7.0) - 2026-09-19

### Added

- [**breaking**] a subject's key is adopted under its declared parent before a deploy serves
- [**breaking**] a subject is a declared type whose values are its ids, and can be deleted with its tenant
- a scan orders by a declared index, and its cursor is the index tuple
- a scan filters on any prefix of a declared index, and ranges on the column after it
- a sealed record, list or map round-trips through the read model and an effect
- an operator can see every wedged lane, not just the one holding the watermark

### Fixed

- [**breaking**] a key namespace written before schema v10 is renamed rather than carried blind
- a cursor is refused when its ordering changed width, and an optional column ranges

### Other

- the orders example says what a customer under a shop actually erases
- heklang 0.9.0, where a subject is a type and a sealed composite decodes

## [0.6.0](https://git.tqwewe.com/tephra/hekla/compare/v0.5.1...v0.6.0) - 2026-09-15

### Added

- the console writes an ad-hoc projection and watches it fold
- an ad-hoc projection answers over HTTP, behind a setting a deployment turns on
- an undeployed projector folds over the live log and prints its rows

### Fixed

- rustls 0.23.45 rejects a handshake message that spans a key change (RUSTSEC-2026-0285)

### Other

- heklang 0.8.0
- a private const is named rather than linked, and a trait link takes its crate path
- heklang 0.7.0, where an answer takes no return and no parens

## [0.5.1](https://git.tqwewe.com/tephra/hekla/compare/v0.5.0...v0.5.1) - 2026-09-12

### Other

- hekla depends on the released heklang 0.6.1

## [0.5.0](https://git.tqwewe.com/tephra/hekla/compare/v0.4.0...v0.5.0) - 2026-09-11

### Added

- [**breaking**] a deploy that cannot read its own log does not start
- *(ui)* the tephra mark replaces the letterform in the rail and in the tab
- *(ui)* the browser's own chrome takes its colour from the theme the console is showing
- *(ui)* the rail becomes a drawer on a narrow screen, and every control is sized for a thumb
- *(ui)* a correlation keeps its colour wherever it appears, and the rail says where a chain ends

### Fixed

- *(ui)* a long enum kind reads as its count, and no kind can squeeze a command's inputs again

### Other

- hekla depends on the released heklang 0.6.0
- a test that omits a field younger than the log waits for a handler that needs one

## [0.4.0](https://git.tqwewe.com/tephra/hekla/compare/v0.3.1...v0.4.0) - 2026-09-09

### Fixed

- [**breaking**] hekla test synthesises the envelope hek test does, not one of its own

## [0.3.1](https://git.tqwewe.com/tephra/hekla/compare/v0.3.0...v0.3.1) - 2026-09-09

### Added

- a scrape says what /status cannot, and no label is ever a lane key
- *(cli)* a redirected log carries no color, and --no-color drops it in a terminal too

## [0.3.0](https://git.tqwewe.com/tephra/hekla/compare/v0.2.0...v0.3.0) - 2026-09-08

### Added

- [**breaking**] a project declares the deployment credentials it needs, and no surface ever shows one
- *(ui)* the console runs a command, and /admin/commands describes every one
- *(ui)* the console browses a projector's rows, and a row links back to the events that built it
- [**breaking**] an on latest arm runs once per key per batch, and its invocation records the range it folded

### Fixed

- *(ui)* the declarations table keeps its columns, a long effect error clips, and a count reads singular at one

### Other

- two private items are named rather than linked, so rustdoc denies nothing
- *(projector)* a completed rebuild logs how long it took

### Added

- a project declares the deployment credentials it needs with `secret NAME`, and hekla resolves each
  from `[secrets]` in `hekla.toml` (`{ env = "..." }` or `{ file = "..." }`) or from
  `HEKLA_SECRET_<NAME>`. `hekla serve` refuses to start when a required one is unset, naming every
  missing one at once
- `hekla secrets` reports every declared credential, where this machine reads it from, and a short
  fingerprint, and exits non-zero when a required one is unset, so it works as a pre-deploy gate
- `hekla plan` reports the same, always present in `--json`, and `--replay` counts invocations it
  could not replay because an effect reads a credential this machine has not set
- `/admin/system` and `/admin/schema` list the declared credentials beside the keystore: a name, a
  source and a fingerprint, never a value
- `hekla check` warns about a declared credential nothing reads, and about a `[secrets]` entry naming
  no declaration. It never reads the environment, so it stays a CI gate
- an `on latest` arm runs once per key per dispatch batch, at the newest matching position in it,
  and the surviving invocation records the range it folded
- `/admin/effects/{name}` reports `latest_collapsed`, the positions folded into another invocation
  since this process started

### Changed

- [**breaking**] an effect arm declaring `on latest` loads and runs, where the previous release
  refused it at load rather than running it as `on`
- an effect's in-flight cap counts units of work rather than positions, so a lane's whole backlog
  collapses to one invocation however long it is
- schema v9 adds `effect_invocation.collapsed_from`; rows written before it read back as null, which
  is the same answer as an invocation that folded nothing
- a transport failure's message now names a credential rather than spelling it. `ureq` writes the url
  it was given into its own error text, and hekla concatenates that onto the wedge message, so a
  webhook whose whole address is the credential used to reach `/status`, `/admin` and the logs on the
  first DNS failure
- heklang 0.5.0

## [0.2.0](https://git.tqwewe.com/tephra/hekla/compare/v0.1.1...v0.2.0) - 2026-09-06

### Added

- *(cli)* hekla rewind takes an effect back over history, against a stopped process
- a key change that would repartition the lanes stops the effect, and hekla plan names it
- on live declines the history that predates an effect, resolved once at first activation
- [**breaking**] an effect arm runs in the lane its key names, so one stuck aggregate blocks nobody else
- [**breaking**] hekla plan replays recorded invocations against the code you are about to deploy
- hekla plan says what a deploy would change, and names why
- [**breaking**] one declaration table holds every version of what the program does

### Other

- heklang 0.3.0, and every effect arm names its lane
- the project builds and releases on the forge it now lives on

### Added

- an effect arm runs in the lane its `@key` names, so one stuck aggregate no longer blocks every
  other one
- `on live` declines the history that predates an effect, resolved once at first activation
- `hekla rewind` takes an effect back to a position, against a stopped process
- `/status` names the lane holding an effect's watermark down, and how many lanes are wedged
- `hekla plan` names the event types whose lane a deploy would repartition

### Changed

- [**breaking**] `@key` is mandatory on every effect arm (heklang 0.3.0)
- [**breaking**] an effect arm declaring `on latest` is refused at load until batch collapse lands,
  rather than run as `on`, which would honour a different guarantee silently
- `[effects] pool_size` is live: it bounds how many lanes run at once, across every effect
- an effect's watermark is a low-water mark, so it no longer advances past a wedged lane
- schema v8 adds `effect_lane` and `effect_activation`; `hekla plan` refuses a data directory this
  build has not migrated, so run `hekla serve` against it once first

## [0.1.1](https://github.com/tephradb/hekla/compare/v0.1.0...v0.1.1) - 2026-09-02

### Fixed

- a timestamp crosses the command boundary in the form a read gave it
- the document and the console name the call kind the journal writes
- a config error keeps its cause and a diagnostic keeps its hint
- an unreadable subject field shows its state, not its ciphertext

### Other

- hekla as a skill an agent can operate it from
- the checker hekla builds against is the one that sees the shorthand
- how to get the runtime, before how to run it ([#2](https://github.com/tephradb/hekla/pull/2))
