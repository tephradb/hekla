# 🌋 hekla

A single-app event-sourcing runtime you write in [heklang], over the Dynamic Consistency Boundary.

Business logic is plain source text: **commands** validate input, replay the history a decision
depends on, and append events; **projectors** consume events into queryable SQLite read models;
**effects** react to events with durable, replay-safe side effects. There is no build step, because
there is nothing to compile. Deploy is restart.

heklang is pure and sandboxed, so determinism is structural rather than policed: a command cannot
call out, a projector cannot decrypt, and a fold cannot read a clock, each because of what kind of
declaration it is rather than because something checks. That is what lets a projector rebuild and an
effect replay reproduce exactly what they did the first time, and what lets the runtime give effects
a Temporal-style journal so a crash mid-arm resumes without re-firing what already happened.

hekla runs on [tephra] for the event log and SQLite for read models. It is a rewrite of [umari],
which expressed the same model as WASM component modules.

[heklang]: https://git.tqwewe.com/tephra/heklang
[tephra]: https://git.tqwewe.com/tephra/tephra
[umari]: https://github.com/tqwewe/umari

## What a project looks like

```hek
// events/order.hk
event @order.placed {
  order_id: Uuid,
  customer_id: Int,
  shop_id: Int,
  // Encrypted under a key scoped to the customer, so erasing them is one key delete.
  // Optional because an erased column reads back absent, and a type that cannot be
  // absent could not say so.
  email: String? @subject(customer_id) @max(200),
}

// commands/place-order.hk
refusal SoldOut "this shop's launch allocation is gone"

command PlaceOrder(order_id: Uuid, customer_id: Int, shop_id: Int, email: String?) {
  // `fold` is a read declaration, not a binding: it names a slice of the log, folds
  // it, and that slice is what the append conditions on. What you folded is what you
  // conflict on, so a concurrent write inside it loses rather than races.
  //
  // Narrow: this one order, so a retry of the same id is a no-op.
  fold placed: Bool = false
    on @order.placed(order_id) => true

  // Wide on purpose: a launch allocation is a rule about every order in the shop.
  fold sold: Int = 0
    on @order.placed(shop_id) => sold + 1

  if placed {
    return
  }
  if sold >= 100 {
    return reject SoldOut
  }

  emit @order.placed { order_id, customer_id, shop_id, email }
}
```

`POST /commands/PlaceOrder` runs that: the route is the declared name, not the file. Add a projector
and `GET /read/{Projector}/{Entity}/{key}` serves what it built. Both routes are generated from the
declarations, with an OpenAPI document and a reference UI at `/docs`.

## Install

```sh
cargo install hekla
```

Or `nix run git+https://git.tqwewe.com/tephra/hekla` to run it without installing anything, and
`nix build git+https://git.tqwewe.com/tephra/hekla` for the binary alone.

One binary is the whole runtime: it serves the API, runs the scenarios, and does the key
management. [tephra] is embedded as a library and SQLite is bundled, so there is no server to
stand up beside it and nothing to point it at. A project is a directory of `.hk` files, and
`examples/orders` in this repository is a working one.

To drive the runtime from your own Rust rather than the CLI, `cargo add hekla`: the binary is a
shim over the library, and everything it does is public.

## Run it

```sh
hekla check examples/orders   # static analysis, for CI and pre-commit
hekla test examples/orders    # scenarios under tests/

HEKLA_MASTER_KEY=$(head -c 32 /dev/urandom | base64) \
  hekla serve examples/orders # the API on 127.0.0.1:8080
```

`check` reports the compiler's diagnostics plus what only hekla knows: that a declaration sits in
the directory its kind requires, that a read model can be keyed and indexed the way the read API
needs, and two warnings about a boundary too broad or too narrow to do its job. Also `hekla erase`
and `hekla rotate` for key management, `hekla plan` for what a deploy would change (and, with
`--replay`, would do), and `hekla verify` for the invariant sweep below.

From a checkout the same three are `cargo run -- check examples/orders` and so on. The repository
is also a flake: `nix build` for the binary, `nix flake check` for the suite.

## Planning a deploy

The event log is append-only and a read model is rebuilt from it, so a deploy is not a thing you
undo. `hekla plan` says what one would change before it changes it.

```sh
hekla plan . --data-dir /srv/hekla/data
```

```
compared 6 declaration(s) against what is deployed
  behaviour command DoA (commands/a.hk)
  behaviour command DoB (commands/b.hk)
  contract  projector UserStats (projectors/user-stats.hk)
  projector UserStats rebuilds from zero, redoing 12481 position(s)
  because `guard ShopIsConnected` changed: DoA, DoB
0 added, 0 removed, 3 changed; 1 projector(s) would rebuild
```

`behaviour` means the declaration does something different behind a contract that did not move, so
nothing outside the program can tell; `contract` means what is visible outside changed. Both come
from heklang's digest, which hashes the lowered program, so reformatting a file or renaming a local
is not a change and a corrected handler body is.

A `const`, `refusal` or `guard` is spliced into what names it and has no row of its own, so editing
one moves every hash that reaches it. The report groups those and names the cause rather than
leaving a wall of diffs.

Without `--replay` it reads the recorded declarations and the read models and nothing else, so it
opens no event log at all. Either way it takes **no data-directory lock**, so it runs against a
directory a server has open. It exits 0 whether or not anything would change, and `--json` carries
the whole plan for a deploy gate.

### Would it still do the same thing?

That diff says an effect changed. It cannot say whether the change matters, and "would this now send
a different HTTP request" is what the deploy actually turns on. `--replay` answers it.

```sh
hekla plan . --data-dir /srv/hekla/data --replay
```

```
  effect NotifyCustomer @ 4812: it reached a call the recorded run never made (http.post #0)
replayed 312 invocation(s) across 2 affected effect(s); 310 reproduce, 2 diverge
this project retains 7 day(s) of journals; anything older was reclaimed before the replay could see it
0 added, 0 removed, 1 changed; 0 projector(s) would rebuild, 2 recorded invocation(s) would diverge
```

Every recorded invocation of every affected effect is re-run against the candidate code and the
journal the original run left behind. Nothing is mocked: the journal holds the responses that run
really received, so a candidate that branches differently on a response reaches a call the journal
has no entry for, and that miss is the finding. Nothing is sent, appended or erased.

Affected means the effect's own digest changed *or* it names something whose digest changed: a
module `fn`, an event it handles, a record or enum either of those carries. Each of those is a
declaration of its own, so editing the helper that builds a URL, or adding `@subject(...)` to a
field the arm binds, leaves the effect's own hash exactly where it was, and a check that looked
only at that hash would miss both.

This half opens the log, through a read-only follower that takes no lock and creates nothing, so it
too runs against a deployment serving traffic. It pins one committed prefix when it opens, so an
invocation the live server records after that is left out rather than replayed against an event the
reader cannot see.

What it cannot see is counted or named rather than assumed away: an invocation whose subject has been
erased (the plaintext the handler branched on is gone, by design); one an operator skipped, since
nothing ran it to an end and its journal is the prefix of a run that stopped where it wedged; one
whose record could not be read at all, because a busy op-DB is not evidence about the candidate; one
retention already reclaimed (row and journal go together, so it is invisible rather than skipped, and
the window is printed instead) or reclaims while the replay is reading it; and whatever
`--replay-limit` drops,
which defaults to the 1000 most recent invocations per effect and names any effect it bit. The
baseline is also only what is *running*: rows written by a version this deploy is not replacing are
still on disk, and replaying those would report a difference the running code already has.

An effect that `reveal`s needs `HEKLA_MASTER_KEY`, decided per effect rather than per project.
Without one those invocations are counted and the rest still replay, so a CI job can plan against
production without holding the production key. A key that is present and cannot unwrap what is
stored degrades the same way and says so, rather than throwing away a diff that is already computed.

A gate reads `--json`, where `divergences` and `coverage` are both `null` when no replay ran: an
empty divergence list would be a clean replay result, and nothing should read one off a run that
never opened the log.

## Checking the invariants

The event log is append-only, so the bugs worth hunting are the ones you cannot undo: an event that
should never have been appended, an effect that fired twice, a read model that quietly disagrees with
its own history. `hekla verify` checks the properties those rest on, against whatever state a
deployment actually reached rather than against cases someone thought to write down.

```sh
# Verify a stopped instance, or a snapshot of one. A plain `cp -r` of a directory a
# server has open is not crash-consistent (SQLite WAL, a segment mid-append), so the
# sweep could report divergence the copy caused, the worst possible false positive
# for a tool whose whole value is being believed.
cargo run -- verify . --data-dir data   # non-zero exit on any violation, for CI or a nightly job
```

It checks that every projector rebuilt from position 0 matches the live one row for row, and that
every recorded effect invocation still replays without performing anything. The replay runs against
a *sealed* host that can only read the journal and can never send, so verifying an invocation is
structurally incapable of repeating it.

`serve --verify` runs the per-operation half continuously: every completed invocation is replayed as
it finishes. A component that breaks an invariant is quarantined (it stops advancing, its reads
return 503, and `/status` names what broke) while the rest of the runtime keeps serving. Turn it
on permanently with `[verify] enabled = true` in `hekla.toml`.

One process at a time: a runtime takes an exclusive lock on its data directory, because tephra does
not lock the segment directory itself.

## Learn more

- **[heklang/docs/]** is the language: commands, projectors, effects, folds, sealed content, tests.
  hekla does not repeat it.
- [AUTHORING.md] is everything hekla adds around it: the directory convention, the envelope, the
  generated HTTP surface, the CLI, and how subject encryption really works underneath.
- [ARCHITECTURE.md] covers the design and the alternatives that were rejected.
- [ROADMAP.md] tracks what is done and what is next.

[heklang/docs/]: https://git.tqwewe.com/tephra/heklang/src/branch/main/docs

[AUTHORING.md]: AUTHORING.md
[ARCHITECTURE.md]: ARCHITECTURE.md
[ROADMAP.md]: ROADMAP.md

## License

Licensed under either of [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT) at your option.

hekla was built with AI use and careful review.
