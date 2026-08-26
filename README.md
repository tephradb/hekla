# 🌋 hekla

A single-app event-sourcing runtime you write in Starlark, over the Dynamic Consistency Boundary.

Business logic is plain source text: **commands** validate input, replay the history a decision
depends on, and append events; **projectors** consume events into queryable SQLite read models;
**effects** react to events with durable, replay-safe side effects. There is no build step, because
there is nothing to compile. Deploy is restart.

Starlark is pure and sandboxed, so determinism is structural rather than policed: a handler has no
clock, no randomness and no I/O beyond the builtins hekla injects. That is what lets a projector
rebuild and an effect replay reproduce exactly what they did the first time, and what lets the
runtime give effects a Temporal-style journal so a crash mid-handler resumes without re-firing what
already happened.

hekla runs on [tephra] for the event log and SQLite for read models. It is a rewrite of [umari],
which expressed the same model as WASM component modules.

[tephra]: https://github.com/tephradb/tephra
[umari]: https://github.com/tqwewe/umari

## What a project looks like

```starlark
# events/order.star
order_placed = event(
    type = "order.placed",
    fields = {
        "order_id": uuid(),
        "customer_id": uint(),
        # Encrypted under a key scoped to the customer, so erasing them is one
        # key delete. `unique` keeps a global tag that survives that erasure.
        "email": str(subject = "customer_id", unique = True, max_length = 200),
    },
)

# commands/place-order.star
load("events/order.star", "order_placed")

input = schema(order_id = uuid(), customer_id = uint(), email = str())

# The consistency boundary: every order placed with this email. The same query
# guards the append, so a concurrent duplicate loses rather than races.
def query(input):
    return order_placed(email = input.email)

initial = {"taken": False}

fold = {order_placed(): lambda state, event: dict(state, taken = True)}

def handle(input, state):
    if state["taken"]:
        return reject("email_taken", "that email has already ordered")
    return order_placed(
        order_id = input.order_id,
        customer_id = input.customer_id,
        email = input.email,
    )
```

`POST /commands/place-order` runs that. Add a projector and `GET /read/{projector}/{entity}/{key}`
serves what it built. Both routes are generated from the schemas, with an OpenAPI document and a
reference UI at `/docs`.

## Run it

```sh
cargo run -- check examples/orders          # static analysis, for CI and pre-commit
cargo run -- test examples/orders           # scenarios under tests/

HEKLA_MASTER_KEY=$(head -c 32 /dev/urandom | base64) \
  cargo run -- serve examples/orders        # the API on 127.0.0.1:8080
```

`check` is thorough on purpose: it resolves the load graph, verifies every clause filters on a field
its event declares and indexes, and warns about a personal-looking field with no `subject`. Also
`hekla fmt`, `hekla lsp` for editors, `hekla erase` / `hekla rotate` for key management, and
`hekla verify` for the invariant sweep below.

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

`serve --verify` runs the per-operation half continuously: every fold is checked for determinism, and
every completed invocation is replayed as it finishes. A component that breaks an invariant is
quarantined (it stops advancing, its reads return 503, and `/status` names what broke) while the rest
of the runtime keeps serving. Turn it on permanently with `[verify] enabled = true` in `hekla.toml`.

One process at a time: a runtime takes an exclusive lock on its data directory, because tephra does
not lock the segment directory itself.

## Learn more

- [AUTHORING.md] is the complete reference for writing the Starlark: every builtin, every rule.
- [ARCHITECTURE.md] covers the design and the alternatives that were rejected.
- [ROADMAP.md] tracks what is done and what is next.

[AUTHORING.md]: AUTHORING.md
[ARCHITECTURE.md]: ARCHITECTURE.md
[ROADMAP.md]: ROADMAP.md

## License

Apache-2.0.

hekla was built with AI use and careful review.
