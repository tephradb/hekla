//! End-to-end checks of the project loader and validation pass.
//!
//! Most of what this file used to assert is now heklang's, and is gone rather than
//! ported. `hek check` is the loader plus three lints; the language checks the
//! language. What moved, by group:
//!
//! - **Every static check about a boundary**: an unknown event type, an undeclared
//!   field, an ill-typed constraint value, a filter on a `@no_index` field, a filter on
//!   sealed content. A slice is resolved from real arguments and checked at parse
//!   time, so each of these is a diagnostic the loader surfaces rather than a rule
//!   hekla re-derives from a stubbed `query()`.
//! - **Every check about the shape of a `fold`**: a key that is not a clause, a value
//!   that is not a function, an empty map, an `initial` that is not data. A `state` is
//!   a declaration with a type, so none of those shapes exists to be wrong.
//! - **`fold` without `query` and `query` without `fold`**, in commands and effects
//!   alike: a `state` *is* its own slice declaration, so the two cannot disagree.
//! - **A fold arm the query never returns**, for the same reason.
//! - **Everything about `load()`**: cycles, escaping paths, broken libraries,
//!   re-exports, a redeclared type. Every `.hk` file in a project is one program and
//!   there is no import to get wrong.
//! - **`unique`**, and the two checks that guarded it.
//! - **`source =` on a projector and a function `handle`**, which are Starlark shapes.
//! - **`read()` and `scan()` as effect builtins**, which no longer exist to remove.
//!
//! What is left here is what hekla still owns: where a declaration may live, what a
//! read model may be keyed and indexed on, the reserved tag namespace, the three lints
//! in `validate.rs`, and `hek test` against hekla's world rather than heklang's
//! harness, which is the only place a test meets real ciphertext and a really-deleted
//! key.

use std::fs;
use std::process::ExitCode;

use hekla::loader::{LoadedProject, Severity};
use hekla::testing;

mod support;

use support::{assert_clean, assert_error, errors, example_dir, findings, write_project};

/// The warning-severity findings, rendered as `location: message`. The shared
/// harness only exposes the error half, and these cases are about the other one.
fn warnings(project: &LoadedProject) -> Vec<String> {
    findings(project)
        .into_iter()
        .filter(|finding| finding.severity == Severity::Warning)
        .map(|finding| format!("{}: {}", finding.location, finding.message))
        .collect()
}

/// A shared event file used by the temp-project cases.
const EVENTS: &str = r#"
event @thing.happened {
  thing_id: Uuid,
  // Free text nobody queries: opting out keeps it out of the tag index.
  note: String @max(200) @no_index,
}
"#;

/// A command that emits [`EVENTS`], for cases where only the file's location matters.
const TRIVIAL_COMMAND: &str = r#"
command DoThing(thing_id: Uuid) {
  emit @thing.happened { thing_id, note: "" }
}
"#;

// --- the shipped examples --------------------------------------------------

#[test]
fn example_project_checks_clean() {
    let project = LoadedProject::load(&example_dir("users"));
    assert!(
        errors(&project).is_empty(),
        "examples/users must check clean: {:?}",
        errors(&project)
    );
    assert_eq!(project.commands.len(), 4);
    assert_eq!(project.projectors.len(), 2);
    assert_eq!(project.effects.len(), 1);
}

#[test]
fn orders_example_checks_clean() {
    let project = LoadedProject::load(&example_dir("orders"));
    assert!(
        errors(&project).is_empty(),
        "examples/orders must check clean: {:?}",
        errors(&project)
    );
    assert_eq!(project.commands.len(), 1);
    assert_eq!(project.projectors.len(), 1);
    assert_eq!(project.effects.len(), 1);
}

// --- where a declaration may live ------------------------------------------

/// The directory a declaration sits in is what makes it routable, internal, or a
/// projector at all, so it is hekla's rule and not the language's: heklang would
/// happily accept any of these anywhere.
#[test]
fn a_declaration_outside_its_directory_is_an_error() {
    assert_error(
        &[
            ("events/thing.hk", EVENTS),
            ("lib/stray.hk", TRIVIAL_COMMAND),
        ],
        "must be declared under commands/",
    );
    assert_error(
        &[
            ("events/thing.hk", EVENTS),
            (
                "commands/stray.hk",
                r#"
projector Stray {
  entity Thing {
    thing_id: Uuid @key,
  }

  on @thing.happened { thing_id } {
    put Thing { thing_id }
  }
}
"#,
            ),
        ],
        "must be declared under projectors/",
    );
    assert_error(
        &[
            ("events/thing.hk", EVENTS),
            (
                "projectors/stray.hk",
                r#"
effect Stray {
  on @thing.happened {
    log("hi")
  }
}
"#,
            ),
        ],
        "must be declared under effects/",
    );
}

/// `commands/internal/` is the one directory whose meaning is not "this is a command":
/// it is a command that no route reaches, invocable only by an effect. Nothing in the
/// declaration says so, which is why the loader has to.
#[test]
fn a_command_under_internal_loads_but_is_not_routed() {
    let dir = write_project(&[
        ("events/thing.hk", EVENTS),
        ("commands/do-thing.hk", TRIVIAL_COMMAND),
        (
            "commands/internal/record.hk",
            r#"
command RecordThing(thing_id: Uuid) {
  emit @thing.happened { thing_id, note: "internal" }
}
"#,
        ),
    ]);
    let project = LoadedProject::load(dir.path());
    assert!(errors(&project).is_empty(), "{:?}", errors(&project));
    let internal: Vec<&str> = project
        .commands
        .iter()
        .filter(|unit| unit.internal)
        .map(|unit| unit.def.name())
        .collect();
    assert_eq!(internal, vec!["RecordThing"]);
}

/// A project whose tree cannot be fully read must fail rather than report success on
/// the part it managed to see: a silently missing command would deploy.
#[cfg(unix)]
#[test]
fn an_unwalkable_project_subdirectory_is_reported() {
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;

    let dir = write_project(&[
        ("events/thing.hk", EVENTS),
        ("commands/do-thing.hk", TRIVIAL_COMMAND),
    ]);
    let blocked = dir.path().join("commands/internal");
    fs::create_dir_all(&blocked).unwrap();
    fs::write(blocked.join("b.hk"), TRIVIAL_COMMAND).unwrap();
    fs::set_permissions(&blocked, Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(&blocked).is_ok() {
        return; // running as root, where the permission bits deny nothing
    }

    let project = LoadedProject::load(dir.path());
    let errs = errors(&project);
    // Restore before the assertion so the temp dir can still be cleaned up.
    fs::set_permissions(&blocked, Permissions::from_mode(0o755)).unwrap();

    assert!(errs.iter().any(|err| err.contains("walking:")), "{errs:?}");
}

// --- what a read model may be keyed and indexed on --------------------------

/// A projector for the `Thing` entity with `columns` and `indexes` spliced in, so each
/// case below differs by one line.
fn entity_project(columns: &str) -> Vec<(String, String)> {
    vec![
        ("events/thing.hk".to_owned(), EVENTS.to_owned()),
        (
            "projectors/things.hk".to_owned(),
            format!(
                r#"
projector Things {{
  entity Thing {{
{columns}
  }}

  on @thing.happened {{ thing_id, note }} {{
    put Thing {{ thing_id, note }}
  }}
}}
"#
            ),
        ),
    ]
}

fn entity_error(columns: &str, needle: &str) {
    let files = entity_project(columns);
    let borrowed: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, body)| (path.as_str(), body.as_str()))
        .collect();
    assert_error(&borrowed, needle);
}

/// The read API paginates by the key as an opaque cursor and binds it as a typed
/// filter, so the key has to be a present, orderable, plaintext scalar. Each of these
/// would otherwise truncate pagination or serve nothing, silently.
#[test]
fn a_key_the_read_api_cannot_paginate_by_is_an_error() {
    // Money is stored as its decimal string, so `ORDER BY` and the `key > ?` cursor
    // would compare lexicographically: "2" sorts after "10". heklang refuses this one
    // before hekla sees it, which is the right order: it is a fact about the type.
    entity_error(
        "    total: Money(2) @key,\n    thing_id: Uuid,\n    note: String @max(200),",
        "cannot be an entity key",
    );
    // An optional key has no cursor at all, and heklang refuses that one too, for the
    // same reason: it is a property of the type rather than of this read API.
    entity_error(
        "    thing_id: Uuid? @key,\n    note: String @max(200),",
        "cannot be an entity key",
    );
}

/// An index over a sealed column could never match: a filter arrives as plaintext and,
/// without the subject, cannot derive the key to compare against the ciphertext. An
/// index over a column the entity does not have is heklang's, checked where the index
/// is written.
#[test]
fn an_index_over_a_sealed_column_is_an_error() {
    assert_error(
        &[
            (
                "events/thing.hk",
                r#"
event @thing.happened {
  thing_id: Uuid,
  owner: Int,
  secret: String? @subject(owner) @max(50),
}
"#,
            ),
            (
                "projectors/things.hk",
                r#"
projector Things {
  entity Thing {
    thing_id: Uuid @key,
    owner: Int,
    // Receives sealed content, so rule 9 propagates the seal onto the column and
    // the index below covers ciphertext.
    secret: String? @max(50),

    index (secret)
  }

  on @thing.happened { thing_id, owner, secret } {
    put Thing { thing_id, owner, secret }
  }
}
"#,
            ),
        ],
        "subject-encrypted column",
    );
}

/// A read filter targets the key or an index-leading column, and the read API owns
/// `limit`, `cursor`, `after` and `timeout_ms` in the same query string. A filterable
/// column named like one of those could never be filtered, so it is refused at load
/// rather than left as a silent no-op at request time.
#[test]
fn a_filterable_column_colliding_with_a_reserved_query_param_is_an_error() {
    let files = vec![
        ("events/thing.hk", EVENTS),
        (
            "projectors/things.hk",
            r#"
projector Things {
  entity Thing {
    thing_id: Uuid @key,
    cursor: String @max(200) @index,
  }

  on @thing.happened { thing_id, note } {
    put Thing { thing_id, cursor: note }
  }
}
"#,
        ),
    ];
    assert_error(&files, "reserved read query param");
}

// --- the reserved tag namespace --------------------------------------------

/// `_hekla_` is where the idempotency and correlation tags live. An event field there
/// could forge a host tag, and with it an append condition, so the namespace is closed
/// to programs.
#[test]
fn an_event_field_in_the_reserved_namespace_is_an_error() {
    assert_error(
        &[(
            "events/sneaky.hk",
            "event @sneaky.happened { _hekla_idem: String @max(20) }\n",
        )],
        "_hekla_",
    );
}

// --- a sealed column has to be able to be absent ----------------------------

/// Erasure destroys the key and rewrites nothing, so hekla answers a column it cannot
/// decrypt with absence. A column whose type cannot say "absent" then breaks two
/// boundaries at once, and only after a real erasure: the projector stalls for good on
/// the next stored load, and the read API serves a body missing a field its own OpenAPI
/// schema marks required.
#[test]
fn a_non_optional_sealed_column_is_an_error() {
    assert_error(
        &[
            (
                "events/order.hk",
                "event @order.placed { order_id: Uuid, customer_id: Int, \
                 email: String @subject(customer_id) @max(100) }\n",
            ),
            (
                "commands/place-order.hk",
                "command PlaceOrder(order_id: Uuid, customer_id: Int, email: String) \
                 { emit @order.placed { order_id, customer_id, email } }\n",
            ),
            (
                "projectors/orders.hk",
                r#"
projector Orders {
  entity Order {
    order_id: Uuid @key,
    customer_id: Int @index,
    email: String @max(100),
  }

  on @order.placed { order_id, customer_id, email } {
    put Order { order_id, customer_id, email }
  }
}
"#,
            ),
        ],
        "make it optional",
    );
}

/// The other half, and the reason this is not a lint about a name: rule 9 propagates
/// the subject onto the column, so the two declarations below differ by one `?` and
/// nothing else, and only one of them can survive an erasure.
#[test]
fn an_optional_sealed_column_is_accepted() {
    assert_clean(&[
        (
            "events/order.hk",
            "event @order.placed { order_id: Uuid, customer_id: Int, \
             email: String? @subject(customer_id) @max(100) }\n",
        ),
        (
            "commands/place-order.hk",
            "command PlaceOrder(order_id: Uuid, customer_id: Int, email: String?) \
             { emit @order.placed { order_id, customer_id, email } }\n",
        ),
        (
            "projectors/orders.hk",
            r#"
projector Orders {
  entity Order {
    order_id: Uuid @key,
    customer_id: Int @index,
    email: String? @max(100),
  }

  on @order.placed { order_id, customer_id, email } {
    put Order { order_id, customer_id, email }
  }
}
"#,
        ),
    ]);
}

// --- parse errors ----------------------------------------------------------

#[test]
fn a_parse_error_is_reported_without_crashing() {
    let dir = write_project(&[("commands/broken.hk", "command Broken( {\n")]);
    let project = LoadedProject::load(dir.path());
    assert!(
        !errors(&project).is_empty(),
        "a malformed file must be reported"
    );
    assert!(project.commands.is_empty());
}

/// A diagnostic carries its span, so a finding points at a line and column rather than
/// at a file. That is the whole reason `Finding::from_diagnostic` exists.
#[test]
fn a_finding_from_a_diagnostic_keeps_its_position() {
    let dir = write_project(&[
        ("events/thing.hk", EVENTS),
        (
            "commands/do-thing.hk",
            "command DoThing(thing_id: Uuid) {\n  emit @no.such.event { thing_id }\n}\n",
        ),
    ]);
    let project = LoadedProject::load(dir.path());
    let finding = findings(&project)
        .into_iter()
        .find(|finding| finding.severity == Severity::Error)
        .expect("an error for the unknown event");
    let span = finding.span.expect("a diagnostic carries a span");
    assert_eq!(span.line, 2, "{finding:?}");
    assert!(span.column > 0, "{finding:?}");
    assert_eq!(finding.location, "commands/do-thing.hk");
}

// --- the boundary lints ---------------------------------------------------

/// Both stay warnings. An error here would stop a valid project deploying over a
/// judgement call, and each of these is a judgement call.
#[test]
fn weak_boundaries_warn_without_failing_the_check() {
    // (a) A slice with no filter on a high-cardinality field guards a broad set of
    // events, which defeats the append's fast reject.
    let dir = write_project(&[
        (
            "events/person.hk",
            "event @person.signed_up { person_id: Uuid, active: Bool }\n",
        ),
        (
            "commands/sign-up.hk",
            r#"
refusal Busy "someone signed up already"

command SignUp(person_id: Uuid) {
  state seen: Bool = fold false
    on @person.signed_up(active: true) => true

  if seen {
    return reject Busy
  }

  emit @person.signed_up { person_id, active: true }
}
"#,
        ),
    ]);
    let project = LoadedProject::load(dir.path());
    let warns = warnings(&project);
    assert!(
        warns.iter().any(
            |warn| warn.starts_with("commands/sign-up.hk") && warn.contains("high-cardinality")
        ),
        "expected the selectivity warning, got {warns:?}"
    );
    assert!(errors(&project).is_empty(), "must not fail the check");

    // (b) A slice pinning nearly every field of an event is usually a copied `emit`:
    // a boundary is a subset match, so it would match almost nothing.
    let dir = write_project(&[
        (
            "events/person.hk",
            r#"
event @person.signed_up {
  person_id: Uuid,
  email: String @max(200),
  plan: String @max(20),
  region: String @max(20),
}
"#,
        ),
        (
            "commands/sign-up.hk",
            r#"
refusal Dup "already signed up"

command SignUp(person_id: Uuid, email: String, plan: String, region: String) {
  state seen: Bool = fold false
    on @person.signed_up(person_id, email, plan, region) => true

  if seen {
    return reject Dup
  }

  emit @person.signed_up { person_id, email, plan, region }
}
"#,
        ),
    ]);
    let project = LoadedProject::load(dir.path());
    let warns = warnings(&project);
    assert!(
        warns
            .iter()
            .any(|warn| warn.starts_with("commands/sign-up.hk")
                && warn.contains("looks like a copied `emit`")),
        "expected the over-constraint warning, got {warns:?}"
    );
    assert!(errors(&project).is_empty(), "must not fail the check");
}

/// A slice narrowed by an id is what a boundary is for, so it must stay quiet.
#[test]
fn a_selective_boundary_does_not_warn() {
    assert_clean(&[
        ("events/thing.hk", EVENTS),
        (
            "commands/do-thing.hk",
            r#"
refusal Dup "that thing already happened"

command DoThing(thing_id: Uuid) {
  state seen: Bool = fold false
    on @thing.happened(thing_id) => true

  if seen {
    return reject Dup
  }

  emit @thing.happened { thing_id, note: "" }
}
"#,
        ),
    ]);
}

// --- `hek test`, against hekla's world -------------------------------------

/// The runner is heklang's and the world is hekla's: real tephra, a real SQLite read
/// model, a real `KeyStore`, and a stubbed network. `heklang/docs/testing.md` rule 8
/// holds either way, because the only world-dependent assertion is a row and it is
/// read through `Rows::row`, which is `patch`'s own read.
///
/// What that buys is everything below. In heklang's own harness a seal carries
/// plaintext and `erased` is a flag, so an erasure assertion could not fail; here the
/// column holds AES-SIV ciphertext and the key is really deleted.
fn run(files: &[(&str, &str)]) -> String {
    format!("{:?}", testing::run(write_project(files).path()))
}

fn ok() -> String {
    format!("{:?}", ExitCode::SUCCESS)
}

fn failed() -> String {
    format!("{:?}", ExitCode::FAILURE)
}

const ACCOUNT_EVENTS: &str = r#"
event @account.registered {
  account_id: Uuid,
  handle: String @max(100),
  email: String? @subject(account_id) @max(100),
}
"#;

const REGISTER_ACCOUNT: &str = r#"
refusal HandleTaken "that handle is already registered"

command RegisterAccount(account_id: Uuid, handle: String, email: String?) {
  state taken: Bool = fold false
    on @account.registered(handle) => true

  if taken {
    return reject HandleTaken
  }

  emit @account.registered { account_id, handle, email }
}
"#;

/// The second case is the load-bearing one: it passes only if the seeded event's tag
/// matches the tag the slice lowers to, against a real append condition.
#[test]
fn a_scenario_runs_a_command_over_a_subject_encrypted_event() {
    assert_eq!(
        run(&[
            ("events/account.hk", ACCOUNT_EVENTS),
            ("commands/register-account.hk", REGISTER_ACCOUNT),
            (
                "tests/accounts.hk",
                r#"
test "registers a new handle" {
  run RegisterAccount {
    account_id: "11111111-1111-1111-1111-111111111111",
    handle: "alice",
    email: "alice@example.com",
  }
  expect @account.registered {
    account_id: "11111111-1111-1111-1111-111111111111",
    handle: "alice",
    email: "alice@example.com",
  }
}

test "rejects a handle another account already took" {
  given @account.registered {
    account_id: "22222222-2222-2222-2222-222222222222",
    handle: "alice",
    email: "other@example.com",
  }
  run RegisterAccount {
    account_id: "33333333-3333-3333-3333-333333333333",
    handle: "alice",
    email: "alice@example.com",
  }
  expect reject HandleTaken
}
"#,
            ),
        ]),
        ok()
    );
}

#[test]
fn a_scenario_whose_expectation_does_not_hold_fails() {
    assert_eq!(
        run(&[
            ("events/account.hk", ACCOUNT_EVENTS),
            ("commands/register-account.hk", REGISTER_ACCOUNT),
            (
                "tests/accounts.hk",
                r#"
test "the wrong handle" {
  run RegisterAccount {
    account_id: "11111111-1111-1111-1111-111111111111",
    handle: "alice",
    email: "alice@example.com",
  }
  expect @account.registered { handle: "bob" }
}
"#,
            ),
        ]),
        failed()
    );
}

const THING_EVENTS: &str = r#"
event @thing.happened {
  thing_id: Uuid,
  owner: Int,
  secret: String? @subject(owner) @max(50),
}
"#;

const THING_PROJECTOR: &str = r#"
projector Things {
  entity Thing {
    thing_id: Uuid @key,
    owner: Int @index,
    secret: String? @max(50),
  }

  on @thing.happened { thing_id, owner, secret } {
    put Thing { thing_id, owner, secret }
  }
}
"#;

/// A row assertion reads through the same decrypt the read API does, so a sealed
/// column comes back as plaintext and an erased one comes back absent. Neither is a
/// flag: the ciphertext is real and the key is really gone.
#[test]
fn a_scenario_projects_given_events_and_asserts_the_decrypted_rows() {
    assert_eq!(
        run(&[
            ("events/thing.hk", THING_EVENTS),
            ("projectors/things.hk", THING_PROJECTOR),
            (
                "tests/rows.hk",
                r#"
test "the row holds what the event carried" {
  given @thing.happened {
    thing_id: "11111111-1111-1111-1111-111111111111",
    owner: 7,
    secret: "hunter2",
  }
  project Things
  expect Thing["11111111-1111-1111-1111-111111111111"] {
    owner: 7,
    secret: "hunter2",
  }
}

test "an erased subject's column reads back absent" {
  given @thing.happened {
    thing_id: "22222222-2222-2222-2222-222222222222",
    owner: 7,
    secret: "hunter2",
  }
  erased owner "7"
  project Things
  expect Thing["22222222-2222-2222-2222-222222222222"] {
    owner: 7,
    secret: none,
  }
}

test "an event nothing wrote has no row" {
  project Things
  expect no Thing["33333333-3333-3333-3333-333333333333"]
}
"#,
            ),
        ]),
        ok()
    );
}

#[test]
fn a_row_that_does_not_match_fails() {
    assert_eq!(
        run(&[
            ("events/thing.hk", THING_EVENTS),
            ("projectors/things.hk", THING_PROJECTOR),
            (
                "tests/rows.hk",
                r#"
test "the wrong secret" {
  given @thing.happened {
    thing_id: "11111111-1111-1111-1111-111111111111",
    owner: 7,
    secret: "hunter2",
  }
  project Things
  expect Thing["11111111-1111-1111-1111-111111111111"] { secret: "wrong" }
}
"#,
            ),
        ]),
        failed()
    );
}

const RELAY_EFFECT: &str = r#"
effect Relay {
  on @thing.happened { thing_id, owner, secret } {
    let response = http.post("https://relay.test/first", { "id": thing_id })
    if response.status >= 400 {
      log("relay rejected with status {response.status}")
      return
    }
    http.post("https://relay.test/second", { "secret": reveal(secret) })
  }
}
"#;

/// Calls are asserted in the order the arm made them, and `reveal` really decrypts:
/// the address in the second body came out of the key store.
#[test]
fn a_scenario_runs_an_effect_and_asserts_its_calls_in_order() {
    assert_eq!(
        run(&[
            ("events/thing.hk", THING_EVENTS),
            ("effects/relay.hk", RELAY_EFFECT),
            (
                "tests/relay.hk",
                r#"
test "relays the revealed secret" {
  given @thing.happened {
    thing_id: "11111111-1111-1111-1111-111111111111",
    owner: 7,
    secret: "hunter2",
  }
  respond "https://relay.test/first" 200
  respond "https://relay.test/second" 200
  deliver Relay
  expect http.post("https://relay.test/first", {
    "id": "11111111-1111-1111-1111-111111111111",
  })
  expect http.post("https://relay.test/second", { "secret": "hunter2" })
}
"#,
            ),
        ]),
        ok()
    );
}

/// A stubbed status the arm can act on drives its branch, and a retryable one cannot
/// be stubbed at all: rule 5 absorbs it before the arm sees it, so a test that could
/// stub one would be asserting something no program can observe.
#[test]
fn a_stubbed_status_drives_the_arms_branch() {
    let files = |status: &str| {
        vec![
            ("events/thing.hk".to_owned(), THING_EVENTS.to_owned()),
            ("effects/relay.hk".to_owned(), RELAY_EFFECT.to_owned()),
            (
                "tests/relay.hk".to_owned(),
                format!(
                    r#"
test "a rejection is logged rather than relayed" {{
  given @thing.happened {{
    thing_id: "11111111-1111-1111-1111-111111111111",
    owner: 7,
    secret: "hunter2",
  }}
  respond "https://relay.test/first" {status}
  deliver Relay
  expect http.post("https://relay.test/first")
  expect log("relay rejected with status {status}")
}}
"#
                ),
            ),
        ]
    };
    let run_with = |status: &str| {
        let owned = files(status);
        let borrowed: Vec<(&str, &str)> = owned
            .iter()
            .map(|(path, body)| (path.as_str(), body.as_str()))
            .collect();
        run(&borrowed)
    };
    assert_eq!(run_with("422"), ok(), "a 4xx reaches the arm");
    // A 429 is absorbed and re-sent, so the arm never logs and the queued reply runs
    // out: whichever way it fails, it must not pass.
    assert_eq!(
        run_with("429"),
        failed(),
        "a retryable status must not be observable by a program"
    );
}

/// Calls out of order is a distinct failure from calls missing, because a replay
/// re-runs them in sequence: an arm whose order changed is an arm whose journal no
/// longer matches.
#[test]
fn calls_made_in_the_wrong_order_fail() {
    assert_eq!(
        run(&[
            ("events/thing.hk", THING_EVENTS),
            ("effects/relay.hk", RELAY_EFFECT),
            (
                "tests/relay.hk",
                r#"
test "backwards" {
  given @thing.happened {
    thing_id: "11111111-1111-1111-1111-111111111111",
    owner: 7,
    secret: "hunter2",
  }
  respond "https://relay.test/first" 200
  respond "https://relay.test/second" 200
  deliver Relay
  expect http.post("https://relay.test/second")
  expect http.post("https://relay.test/first")
}
"#,
            ),
        ]),
        failed()
    );
}

/// An `erase` is asserted the same way a call is, and it really runs against the
/// case's own key store: the `reveal` after it fails exactly as it would live, which
/// is `docs/effects.md` rule 9's cost made visible.
#[test]
fn an_erase_is_assertable_and_the_key_is_really_gone() {
    let shred = r#"
effect Shred {
  on @thing.happened { owner } {
    erase(owner)
  }
}
"#;
    assert_eq!(
        run(&[
            ("events/thing.hk", THING_EVENTS),
            ("effects/shred.hk", shred),
            (
                "tests/shred.hk",
                r#"
test "shreds the owner" {
  given @thing.happened {
    thing_id: "11111111-1111-1111-1111-111111111111",
    owner: 7,
    secret: "hunter2",
  }
  deliver Shred
  expect erase(owner, "7")
}
"#,
            ),
        ]),
        ok()
    );

    // A subject erased before the arm runs makes `reveal` terminal, and the runner
    // sees the skip rather than a wedge or a `none`.
    assert_eq!(
        run(&[
            ("events/thing.hk", THING_EVENTS),
            ("effects/relay.hk", RELAY_EFFECT),
            (
                "tests/relay.hk",
                r#"
test "an erased owner skips rather than sending plaintext" {
  given @thing.happened {
    thing_id: "11111111-1111-1111-1111-111111111111",
    owner: 7,
    secret: "hunter2",
  }
  erased owner "7"
  respond "https://relay.test/first" 200
  deliver Relay
  // The first call is made and journaled; the `reveal` behind the second is what
  // cannot be recovered, so the skip is what the invocation ends as.
  expect http.post("https://relay.test/first")
  expect skipped
}
"#,
            ),
        ]),
        ok()
    );
}

/// An effect that does nothing observable says so, which is what stops "no calls" from
/// being indistinguishable from "the arm never ran".
#[test]
fn an_effect_that_does_nothing_asserts_nothing() {
    assert_eq!(
        run(&[
            ("events/thing.hk", THING_EVENTS),
            (
                "effects/quiet.hk",
                r#"
effect Quiet {
  on @thing.happened { owner } {
    if owner > 100 {
      log("big owner")
    }
  }
}
"#,
            ),
            (
                "tests/quiet.hk",
                r#"
test "a small owner does nothing" {
  given @thing.happened {
    thing_id: "11111111-1111-1111-1111-111111111111",
    owner: 7,
    secret: "hunter2",
  }
  deliver Quiet
  expect nothing
}
"#,
            ),
        ]),
        ok()
    );
}
