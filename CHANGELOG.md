# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
