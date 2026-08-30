# 🌋 hekla

A single-app event-sourcing runtime you write in [heklang](../heklang), over the Dynamic Consistency
Boundary.

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

[tephra]: https://github.com/tephradb/tephra
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
command PlaceOrder(order_id: Uuid, customer_id: Int, shop_id: Int, email: String?) {
  // `state` is a read declaration, not a binding: it names a slice of the log, folds
  // it, and that slice is what the append conditions on. What you folded is what you
  // conflict on, so a concurrent write inside it loses rather than races.
  //
  // Narrow: this one order, so a retry of the same id is a no-op.
  state placed: Bool = fold false
    on @order.placed(order_id) => true

  // Wide on purpose: a launch allocation is a rule about every order in the shop.
  state sold: Int = fold 0
    on @order.placed(shop_id) => sold + 1

  if placed {
    return
  }
  if sold >= 100 {
    return reject("sold_out", "this shop's launch allocation is gone")
  }

  emit @order.placed { order_id, customer_id, shop_id, email }
}
```

`POST /commands/PlaceOrder` runs that: the route is the declared name, not the file. Add a projector
and `GET /read/{Projector}/{Entity}/{key}` serves what it built. Both routes are generated from the
declarations, with an OpenAPI document and a reference UI at `/docs`.

## Run it

```sh
cargo run -- check examples/orders          # static analysis, for CI and pre-commit
cargo run -- test examples/orders           # scenarios under tests/

HEKLA_MASTER_KEY=$(head -c 32 /dev/urandom | base64) \
  cargo run -- serve examples/orders        # the API on 127.0.0.1:8080
```

`check` reports the compiler's diagnostics plus what only hekla knows: that a declaration sits in
the directory its kind requires, that a read model can be keyed and indexed the way the read API
needs, and three warnings, including a personal-looking field with no `@subject`. Also `hekla erase`
and `hekla rotate` for key management, and `hekla verify` for the invariant sweep below.

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

[heklang/docs/]: ../heklang/docs/

[AUTHORING.md]: AUTHORING.md
[ARCHITECTURE.md]: ARCHITECTURE.md
[ROADMAP.md]: ROADMAP.md

## License

Apache-2.0.

hekla was built with AI use and careful review.
