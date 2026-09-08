# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
