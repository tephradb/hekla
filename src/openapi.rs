//! Generated OpenAPI 3.1 for hekla's HTTP surface.
//!
//! The document is generated from the loaded project rather than maintained by
//! hand, and it covers every route [`crate::server::app`] declares: one concrete
//! path per public command, two per projector entity, and the operator endpoints.
//! Internal commands are absent because they are not routed.
//!
//! `GET /openapi.json` and `hekla openapi` both go through [`Surface::from_project`]
//! and [`build`], so the served document and the dumped one cannot disagree.
//!
//! The field-type mapping is the wire form hekla validates: `money` is a decimal
//! string (not an integer, despite its storage type), `one_of` is a string enum, and
//! an `optional` field is simply omitted from `required`.
//!
//! Event schemas are the one part describing something other than a request or
//! response body. An event's fields never reach the wire: a command's 200 reports
//! each emitted event as its type and its plaintext tags, and there is no endpoint
//! that serves event payloads. So `components/schemas/event.*` documents what the log
//! holds and says as much in its own description. The declared event set does become
//! load-bearing in one place, as the `enum` of `EmittedEvent.type`.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use crate::introspect;
use crate::loader::LoadedProject;
use crate::read_api;
use crate::read_model::key_kind;
use crate::schema::{EntityDef, EventDef, FieldKind, FieldMeta, InputSchema, ModuleDef};
use crate::server::{self, READ_WAIT_DEFAULT, READ_WAIT_MAX};
use crate::ui;

const COMMANDS_TAG: &str = "commands";
const OPERATIONS_TAG: &str = "operations";
const INTROSPECTION_TAG: &str = "introspection";

// ---------------------------------------------------------------------------
// The surface
// ---------------------------------------------------------------------------

/// Everything the document is generated from, borrowed from a loaded project.
///
/// Every list is sorted by name. Object keys serialize sorted on their own, but
/// arrays (`tags`, every `enum`) keep insertion order, and a document that reorders
/// between runs cannot be committed and diffed.
pub struct Surface<'a> {
    /// Public commands only. An internal one is not routed, so it is not described.
    pub commands: Vec<(&'a str, &'a InputSchema)>,
    pub projectors: Vec<ProjectorSurface<'a>>,
    pub effects: Vec<EffectSurface<'a>>,
    pub events: Vec<(&'a str, &'a EventDef)>,
}

pub struct ProjectorSurface<'a> {
    pub name: &'a str,
    pub entities: &'a [EntityDef],
    /// The event types it subscribes to.
    pub sources: Vec<&'a str>,
}

pub struct EffectSurface<'a> {
    pub name: &'a str,
    /// The event types it subscribes to.
    pub sources: Vec<&'a str>,
}

impl<'a> Surface<'a> {
    pub fn from_project(project: &'a LoadedProject) -> Surface<'a> {
        let mut commands = Vec::new();
        for unit in &project.commands {
            if unit.internal {
                continue;
            }
            if let ModuleDef::Command { name, input } = &unit.def {
                commands.push((name.as_str(), input));
            }
        }
        commands.sort_by_key(|(name, _)| *name);

        let mut projectors = Vec::new();
        for unit in &project.projectors {
            if let ModuleDef::Projector {
                name,
                entities,
                sources,
            } = &unit.def
            {
                projectors.push(ProjectorSurface {
                    name: name.as_str(),
                    entities: entities.as_slice(),
                    sources: sources.iter().map(String::as_str).collect(),
                });
            }
        }
        projectors.sort_by_key(|projector| projector.name);

        let mut effects = Vec::new();
        for unit in &project.effects {
            if let ModuleDef::Effect { name, sources } = &unit.def {
                effects.push(EffectSurface {
                    name: name.as_str(),
                    sources: sources.iter().map(String::as_str).collect(),
                });
            }
        }
        effects.sort_by_key(|effect| effect.name);

        let mut events: Vec<(&str, &EventDef)> = project
            .events
            .iter()
            .map(|(event_type, def)| (event_type.as_str(), def))
            .collect();
        events.sort_by_key(|(event_type, _)| *event_type);

        Surface {
            commands,
            projectors,
            effects,
            events,
        }
    }

    fn projector_names(&self) -> Vec<&str> {
        self.projectors.iter().map(|p| p.name).collect()
    }

    fn effect_names(&self) -> Vec<&str> {
        self.effects.iter().map(|e| e.name).collect()
    }

    fn event_types(&self) -> Vec<&str> {
        self.events
            .iter()
            .map(|(event_type, _)| *event_type)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// The document
// ---------------------------------------------------------------------------

const INFO_DESCRIPTION: &str = "\
Generated from the loaded project. Every path below is a route this server serves, \
and every schema is derived from a declaration in the project's `.hk` files.

Commands append events through a consistency boundary. Read models are materialised \
by projectors and queried by key or by a declared index. The operator endpoints \
report and steer the runtime.

`components/schemas/event.*` describes what the event log holds. Those are not \
request or response bodies: a command's 200 reports each emitted event as its type \
and its plaintext tags, never its fields.";

const COMMANDS_TAG_DESCRIPTION: &str = "\
Append events through a command's consistency boundary. Each request folds the events \
its `query` selects, decides on that state, and appends conditionally, so a boundary \
that changed underneath is a 409 rather than a lost update. Internal commands \
(`commands/internal/`) are invokable by effects but never routed, so they are absent \
from this document.";

const OPERATIONS_TAG_DESCRIPTION: &str = "\
Operator and diagnostic endpoints. `/health` is a liveness check; `/status` reports \
per-module positions and lag. The replay and skip endpoints are explicit manual \
actions, never automatic.";

const INTROSPECTION_TAG_DESCRIPTION: &str = "\
Read-only introspection: browse the event log, follow a request through the causal \
chain it set off, see what a wedged effect actually did, and read back what this \
process loaded and is configured with. Every endpoint here is a `GET` and none of \
them writes; replaying a projector and skipping an effect stay under `operations`.\n\n\
Like the rest of this API, none of it is authenticated. The bind address is the \
boundary, and it defaults to loopback. A single prefix is what lets a deployment that \
binds wider deny it in a proxy.";

pub fn build(surface: &Surface) -> Value {
    // Assigned before anything emits a `$ref`, because a ref has to name the key the
    // schema is finally stored under.
    let names = ComponentNames::assign(surface);

    let mut paths = Map::new();
    for (name, schema) in &surface.commands {
        paths.insert(format!("/commands/{name}"), command_path(name, schema));
    }
    for projector in &surface.projectors {
        for entity in projector.entities {
            paths.insert(
                format!("/read/{}/{}", projector.name, entity.name),
                scan_path(projector, entity, &names),
            );
            paths.insert(
                format!("/read/{}/{}/{{key}}", projector.name, entity.name),
                get_one_path(projector, entity, &names),
            );
        }
    }
    for (path, item) in operation_paths(surface) {
        paths.insert(path, item);
    }
    for (path, item) in introspection_paths(surface) {
        paths.insert(path, item);
    }
    disambiguate_operation_ids(&mut paths);

    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "hekla",
            "version": env!("CARGO_PKG_VERSION"),
            "description": INFO_DESCRIPTION,
        },
        "tags": tags(surface),
        "paths": Value::Object(paths),
        "components": { "schemas": schemas(surface, &names) },
    })
}

/// The component key each event type and each entity is stored under, assigned once so
/// a `$ref` and the schema it points at cannot disagree.
///
/// Uniqueness is structural rather than argued. [`component_name`] rewrites every
/// character outside OpenAPI's `[A-Za-z0-9._-]` to `_`, and an event type is an
/// unvalidated author-supplied string (`event(type = "order placed")` is legal), so two
/// distinct types can fold to one key. Without this the second `insert` would silently
/// replace the first, leaving one schema describing two event types while
/// `EmittedEvent.type` still lists both.
struct ComponentNames {
    events: HashMap<String, String>,
    entities: HashMap<(String, String), String>,
}

impl ComponentNames {
    fn assign(surface: &Surface) -> ComponentNames {
        // Seeded with the fixed names so a pathological event type cannot displace
        // `Error` or `Status`. `surface`'s lists are sorted, so which claimant keeps the
        // bare key is stable across runs.
        let mut taken: Vec<String> = FIXED_SCHEMAS
            .iter()
            .map(|name| (*name).to_owned())
            .collect();
        let mut claim = |candidate: String| -> String {
            let mut unique = candidate.clone();
            let mut suffix = 2;
            while taken.contains(&unique) {
                unique = format!("{candidate}_{suffix}");
                suffix += 1;
            }
            taken.push(unique.clone());
            unique
        };

        let mut events = HashMap::new();
        for (event_type, _) in &surface.events {
            let key = claim(format!("event.{}", component_name(event_type)));
            events.insert((*event_type).to_owned(), key);
        }
        let mut entities = HashMap::new();
        for projector in &surface.projectors {
            for entity in projector.entities {
                let key = claim(format!(
                    "entity.{}.{}",
                    component_name(projector.name),
                    component_name(&entity.name)
                ));
                entities.insert((projector.name.to_owned(), entity.name.clone()), key);
            }
        }
        ComponentNames { events, entities }
    }

    fn event(&self, event_type: &str) -> &str {
        &self.events[event_type]
    }

    fn entity(&self, projector: &str, entity: &str) -> &str {
        &self.entities[&(projector.to_owned(), entity.to_owned())]
    }
}

/// The schemas that are always present, whatever the project declares.
const FIXED_SCHEMAS: [&str; 22] = [
    "ErrorDetail",
    "Error",
    "CommandError",
    "CommandAccepted",
    "EmittedEvent",
    "Status",
    "ProjectorStatus",
    "EffectStatus",
    "LogEvent",
    "SubjectState",
    "EffectDetail",
    "EffectInvocation",
    "EffectInvocationDetail",
    "TraceInvocation",
    "JournalCall",
    "FieldDetail",
    "EventDetail",
    "EntityDetail",
    "ProjectorDetail",
    "DeclarationSummary",
    "SystemInfo",
    "SubjectEntry",
];

/// Every kind a `declaration` row can carry: heklang's set minus `test`.
///
/// `Digest::entries` holds the tests back, so no recorded declaration can ever be one.
/// A `const`, a `refusal` and a `guard` are absent for a different reason: heklang
/// inlines all three before a program exists, so their content is already inside every
/// declaration that names them and they have no entry to record.
const DECLARATION_KINDS: [&str; 7] = [
    "event",
    "enum",
    "record",
    "function",
    "command",
    "projector",
    "effect",
];

/// Make every `operationId` unique, which OpenAPI requires and a client generator
/// depends on to name its functions.
///
/// [`identifier`] folds every character a Rust or TypeScript identifier cannot hold
/// down to `_`, so two module names that differ only there (`a-b` and `a_b`) produce
/// one id. Rather than reason about which names can collide, this makes uniqueness
/// structural: the second and later claimants get a numeric suffix. `paths` is a
/// `BTreeMap`, so which one keeps the bare id is stable across runs.
///
/// [`ComponentNames`] does the same job for schema keys, for the same reason.
fn disambiguate_operation_ids(paths: &mut Map<String, Value>) {
    let mut seen: Vec<String> = Vec::new();
    for item in paths.values_mut() {
        let Some(operations) = item.as_object_mut() else {
            continue;
        };
        for operation in operations.values_mut() {
            let Some(operation) = operation.as_object_mut() else {
                continue;
            };
            let Some(id) = operation.get("operationId").and_then(Value::as_str) else {
                continue;
            };
            let mut unique = id.to_owned();
            let mut suffix = 2;
            while seen.contains(&unique) {
                unique = format!("{id}_{suffix}");
                suffix += 1;
            }
            seen.push(unique.clone());
            operation.insert("operationId".to_owned(), Value::String(unique));
        }
    }
}

/// The tags, in the order a reference UI should render them: commands first, then
/// one section per projector's read model, then the operator endpoints.
fn tags(surface: &Surface) -> Value {
    let mut out = vec![json!({
        "name": COMMANDS_TAG,
        "description": COMMANDS_TAG_DESCRIPTION,
    })];
    for projector in &surface.projectors {
        out.push(json!({
            "name": read_tag(projector.name),
            "description": projector_tag_description(projector),
        }));
    }
    out.push(json!({
        "name": OPERATIONS_TAG,
        "description": OPERATIONS_TAG_DESCRIPTION,
    }));
    out.push(json!({
        "name": INTROSPECTION_TAG,
        "description": INTROSPECTION_TAG_DESCRIPTION,
    }));
    Value::Array(out)
}

fn read_tag(projector: &str) -> String {
    format!("read: {projector}")
}

fn projector_tag_description(projector: &ProjectorSurface) -> String {
    let built_from = if projector.sources.is_empty() {
        "no declared source".to_owned()
    } else {
        backticked(&projector.sources)
    };
    let entities: Vec<&str> = projector
        .entities
        .iter()
        .map(|entity| entity.name.as_str())
        .collect();
    let materialises = if entities.is_empty() {
        "no entities".to_owned()
    } else {
        backticked(&entities)
    };
    format!(
        "The `{}` projector's read model, built from {built_from} and materialising \
         {materialises}. Rows are a view of the log rather than the log itself: they \
         are eventually consistent, and every response carries the log `position` it \
         was read at. Pass `after` for read-your-writes.",
        projector.name,
    )
}

fn backticked(items: &[&str]) -> String {
    format!("`{}`", items.join("`, `"))
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

const IDEMPOTENCY_KEY_DESCRIPTION: &str = "\
A client-chosen key that makes this request retry-safe. Surrounding whitespace is \
trimmed and a blank value counts as no key. The key alone identifies the request, so \
reusing one replays the first outcome verbatim rather than running the new body; a \
different outcome needs a different key.";

const CORRELATION_ID_DESCRIPTION: &str = "\
A uuid tying this request to a trace. It is echoed in the response and carried onto \
every event the command appends. An absent or malformed value gets a fresh uuid \
rather than an error.";

fn command_path(name: &str, schema: &InputSchema) -> Value {
    json!({
        "post": {
            "tags": [COMMANDS_TAG],
            "operationId": format!("execute_{}", identifier(name)),
            "summary": format!("execute the `{name}` command"),
            "parameters": [
                header_param("Idempotency-Key", IDEMPOTENCY_KEY_DESCRIPTION),
                header_param("X-Correlation-Id", CORRELATION_ID_DESCRIPTION),
            ],
            "requestBody": {
                "required": schema.fields.iter().any(|(_, kind)| !kind.is_nullable()),
                "content": {
                    "application/json": { "schema": input_schema(schema) },
                },
            },
            "responses": {
                "200": response(
                    "committed; the body carries the appended positions and emitted events",
                    schema_ref("CommandAccepted"),
                ),
                "400": response("the input was malformed", schema_ref("CommandError")),
                "409": response(
                    "the consistency boundary kept changing; retry",
                    schema_ref("CommandError"),
                ),
                "422": response(
                    "the command rejected the request on state grounds",
                    schema_ref("CommandError"),
                ),
                "500": response("internal error", schema_ref("CommandError")),
                "503": response(
                    "the store was unavailable; retry",
                    schema_ref("CommandError"),
                ),
            },
        }
    })
}

fn input_schema(schema: &InputSchema) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for (field, kind) in &schema.fields {
        // An optional input may be omitted *or* sent as an explicit null: `check_value`
        // (`schema.rs`) accepts null before it looks at the inner kind. Absence
        // alone is what `required` encodes, so the null has to be in the type too, or a
        // validating client refuses a body the server accepts.
        let mut property = if kind.is_nullable() {
            nullable(field_schema(kind))
        } else {
            field_schema(kind)
        };
        // RFC 3339 is what `field_schema` declares and what a read response holds, so it
        // is the form to write against. The binder also takes rule 8's epoch
        // microseconds, which is what a tag holds and what an older client sends, and a
        // schema that hid that would make a documented request body a lie in the other
        // direction.
        if matches!(kind.base(), FieldKind::Timestamp) {
            property["description"] = json!("RFC 3339, or epoch microseconds as an integer");
        }
        properties.insert(field.clone(), property);
        if !kind.is_nullable() {
            required.push(Value::String(field.clone()));
        }
    }
    json!({
        "type": "object",
        "properties": Value::Object(properties),
        "required": required,
        "additionalProperties": false,
    })
}

// ---------------------------------------------------------------------------
// Read models
// ---------------------------------------------------------------------------

/// Appended to both read descriptions. Callers supply the separating space: a
/// leading one here would be eaten by the `\`-continuation, which strips the newline
/// and the next line's indentation together.
const READ_YOUR_WRITES: &str = "\
Pass `after` with the `positions.last` a command returned to block until this \
projector has caught up to that position, then serve the normal snapshot. Without it \
the read is served immediately and may be behind the log.";

const READ_UNAVAILABLE: &str = "\
the read model cannot be served right now. `error.code` says which: `rebuilding` (the \
model is being rebuilt), `stale` (built from a different definition with auto-rebuild \
off), `rebuild_failed`, `quarantined` (it failed an invariant check and stopped \
advancing), or `not_caught_up` (an `after` wait timed out).";

/// The 503 both read paths return.
///
/// Its `Retry-After` is declared rather than only described, so a client can back off
/// against a typed value. It is optional because only the two cases that resolve on
/// their own carry it: a timed-out `after` wait, and a projector mid-rebuild.
/// `stale`, `rebuild_failed` and `quarantined` need an operator, so retrying is not
/// the advice and the header is absent.
fn read_unavailable_response() -> Value {
    let mut response = response(READ_UNAVAILABLE, schema_ref("Error"));
    if let Some(object) = response.as_object_mut() {
        object.insert(
            "headers".to_owned(),
            json!({
                "Retry-After": {
                    "description": "Seconds to wait before retrying. Present only for \
                        `rebuilding` and `not_caught_up`, which resolve on their own; \
                        the codes that need an operator omit it.",
                    "required": false,
                    "schema": { "type": "integer", "minimum": 0 },
                },
            }),
        );
    }
    response
}

fn get_one_path(projector: &ProjectorSurface, entity: &EntityDef, names: &ComponentNames) -> Value {
    let mut parameters = vec![path_param(
        "key",
        &format!("The row's `{}`.", entity.key),
        field_schema(key_kind(entity)),
    )];
    parameters.extend(wait_params());

    let item = json!({
        "type": "object",
        "properties": {
            "item": schema_ref(names.entity(projector.name, &entity.name)),
            "position": position_schema("The projector's log position, read in the same snapshot as the row."),
        },
        "required": ["item", "position"],
        "additionalProperties": false,
    });

    json!({
        "get": {
            "tags": [read_tag(projector.name)],
            "operationId": operation_id("read", projector.name, &entity.name),
            "summary": format!("read one `{}` row by key", entity.name),
            "description": format!(
                "Returns the row whose `{}` equals `key`, with the projector's log \
                 position read in the same snapshot, so the data and the position \
                 agree. {READ_YOUR_WRITES}",
                entity.key,
            ),
            "parameters": parameters,
            "responses": {
                "200": response("the row, and the position it was read at", item),
                "400": response("a query parameter was malformed", schema_ref("Error")),
                "404": response(
                    "no such projector, entity, or row",
                    schema_ref("Error"),
                ),
                "503": read_unavailable_response(),
                "500": response("internal error", schema_ref("Error")),
            },
        }
    })
}

fn scan_path(projector: &ProjectorSurface, entity: &EntityDef, names: &ComponentNames) -> Value {
    let page = json!({
        "type": "object",
        "properties": {
            "items": {
                "type": "array",
                "items": schema_ref(names.entity(projector.name, &entity.name)),
            },
            "next_cursor": {
                "type": ["string", "null"],
                "description": "Pass as `cursor` for the next page. Null on the last page.",
            },
            "position": position_schema("The projector's log position, read in the same snapshot as the rows."),
        },
        "required": ["items", "next_cursor", "position"],
        "additionalProperties": false,
    });

    json!({
        "get": {
            "tags": [read_tag(projector.name)],
            "operationId": operation_id("scan", projector.name, &entity.name),
            "summary": format!("scan the `{}` entity", entity.name),
            "description": format!(
                "An ordered, cursor-paginated scan of `{}`, ordered by its `{}` key. \
                 Filtering is restricted to the key and the leftmost column of a \
                 declared index, and at most one filter per request: anything else \
                 would be a table scan, so it is a 400 rather than a slow \
                 query. {READ_YOUR_WRITES}",
                entity.name, entity.key,
            ),
            "parameters": scan_params(entity),
            "responses": {
                "200": response("one page of rows, and the position they were read at", page),
                "400": response(
                    "a malformed parameter, or a filter on a field that is neither the key nor an index's leftmost column",
                    schema_ref("Error"),
                ),
                "404": response("no such projector or entity", schema_ref("Error")),
                "503": read_unavailable_response(),
                "500": response("internal error", schema_ref("Error")),
            },
        }
    })
}

fn scan_params(entity: &EntityDef) -> Vec<Value> {
    let mut params = vec![
        // No `maximum`, deliberately. The handler clamps into `[1, MAX_LIMIT]` and
        // serves a 200; it does not reject. Declaring the ceiling as a bound would make
        // a validating client refuse `limit=1000` locally rather than receive the page
        // of 500 the server would happily return.
        query_param(
            "limit",
            &format!(
                "Page size. Defaults to {}. Values outside 1 to {} are clamped into \
                 range rather than rejected; only a non-integer is a 400.",
                read_api::DEFAULT_LIMIT,
                read_api::MAX_LIMIT,
            ),
            json!({
                "type": "integer",
                "minimum": 0,
                "default": read_api::DEFAULT_LIMIT,
            }),
        ),
        query_param(
            "cursor",
            "An opaque forward cursor from a previous page's `next_cursor`. Pagination \
             is by key, never by offset, so rows are neither skipped nor repeated when \
             the model changes between pages.",
            json!({ "type": "string" }),
        ),
    ];
    params.extend(wait_params());
    // Deduplicated here rather than in `filterable_fields`, which stays lazy for the
    // read path's membership check. Two indexes leading with the same column would
    // otherwise emit that query parameter twice, which is not a valid operation.
    let mut filters: Vec<&str> = Vec::new();
    for field in read_api::filterable_fields(entity) {
        if !filters.contains(&field) {
            filters.push(field);
        }
    }
    for field in filters {
        params.push(filter_param(entity, field));
    }
    params
}

/// One indexed-filter query param. Only the key and each index's leftmost column
/// reach here, which is [`read_api::filterable_fields`]' contract and the same source
/// the handler's 400 is decided from.
///
/// Every field that reaches here is plaintext. `EntityDef::validate` rejects a
/// subject-encrypted key and a subject-encrypted column in any index at load, for the
/// reason that would otherwise matter here: a filter arrives as plaintext and the
/// column holds ciphertext, so it could only ever match nothing.
fn filter_param(entity: &EntityDef, field: &str) -> Value {
    // Not an `Option`: `EntityDef::validate` rejects a key or an index column that is
    // not a declared field, so a miss here is a broken invariant. Falling back to a
    // bare string would emit a silently wrong type for a `uint` column instead.
    let (_, meta) = entity
        .fields
        .iter()
        .find(|(name, _)| name == field)
        .expect("a filterable field is a declared field");
    let mut notes = vec![format!(
        "Filter on `{field}`. At most one filter may be supplied per request; a second \
         is a 400."
    )];
    if field == entity.key {
        notes.push("This is the entity's key.".to_owned());
    }
    if matches!(meta.kind.base(), FieldKind::Bool) {
        notes.push(
            "The server additionally accepts `1` and `0` here, beyond the `true` and \
             `false` this schema declares."
                .to_owned(),
        );
    }
    query_param(field, &notes.join(" "), field_schema(&meta.kind))
}

fn wait_params() -> Vec<Value> {
    let default_ms = READ_WAIT_DEFAULT.as_millis() as u64;
    let max_ms = READ_WAIT_MAX.as_millis() as u64;
    vec![
        query_param(
            "after",
            "Read-your-writes: block until this projector reaches this log position \
             before reading. Use the `positions.last` a command returned. On timeout \
             the read fails closed with a 503 rather than serving stale data.",
            json!({ "type": "integer", "minimum": 0 }),
        ),
        // Capped rather than rejected, like `limit`, so no `maximum` here either.
        query_param(
            "timeout_ms",
            &format!(
                "How long an `after` wait may block, in milliseconds. Defaults to \
                 {default_ms}. A larger value is capped at {max_ms} rather than \
                 rejected. `0` checks once without waiting. Ignored when `after` is \
                 absent.",
            ),
            // A `default` states what happens when the caller omits it, which is a
            // claim no validating client can wrongly enforce, unlike a `maximum`. It is
            // the one parameter where the default matters most: a caller passing
            // `after` blocks for five seconds without knowing it otherwise.
            json!({ "type": "integer", "minimum": 0, "default": default_ms }),
        ),
    ]
}

// ---------------------------------------------------------------------------
// Operator endpoints
// ---------------------------------------------------------------------------

/// The operator paths, keyed by `server`'s own route constants. The command and read
/// paths are formatted per module so they cannot use them, but these six are the same
/// literals the router registers and there is no reason to write them twice.
fn operation_paths(surface: &Surface) -> Vec<(String, Value)> {
    vec![
        (
            server::REPLAY_ROUTE.to_owned(),
            replay_path(&surface.projector_names()),
        ),
        (
            server::SKIP_ROUTE.to_owned(),
            skip_path(&surface.effect_names()),
        ),
        (server::STATUS_ROUTE.to_owned(), status_path()),
        (server::HEALTH_ROUTE.to_owned(), health_path()),
        (server::OPENAPI_ROUTE.to_owned(), openapi_path()),
        (server::DOCS_ROUTE.to_owned(), docs_path()),
    ]
}

fn replay_path(projectors: &[&str]) -> Value {
    let accepted = json!({
        "type": "object",
        "properties": {
            "status": { "type": "string", "enum": ["replay_scheduled"] },
            "projector": { "type": "string" },
        },
        "required": ["status", "projector"],
        "additionalProperties": false,
    });
    json!({
        "post": {
            "tags": [OPERATIONS_TAG],
            "operationId": "replay_projector",
            "summary": "schedule a projector rebuild",
            "description": "Schedules a rebuild-and-swap of the projector's read model. \
                Returns 202 at once: the projector picks the request up between batches, \
                and callers watch its lag in `/status`. Reads keep being served from the \
                existing model until the rebuilt one is swapped in atomically.",
            "parameters": [
                path_param("name", "The projector to rebuild.", name_schema(projectors)),
            ],
            "responses": {
                "202": response("the rebuild was scheduled", accepted),
                "404": response("no such projector", schema_ref("Error")),
                "503": response(
                    "the projector's thread has stopped, so nothing is left to act on the request",
                    schema_ref("Error"),
                ),
            },
        }
    })
}

fn skip_path(effects: &[&str]) -> Value {
    let accepted = json!({
        "type": "object",
        "properties": {
            "status": { "type": "string", "enum": ["skip_scheduled"] },
            "effect": { "type": "string" },
            "position": position_schema("The position that will be marked terminal."),
        },
        "required": ["status", "effect", "position"],
        "additionalProperties": false,
    });
    json!({
        "post": {
            "tags": [OPERATIONS_TAG],
            "operationId": "skip_effect_position",
            "summary": "advance a wedged effect past one event",
            "description": "Marks one position terminal without processing it, so a wedged \
                effect can move on from a genuinely unprocessable event. This is an \
                explicit manual action and never automatic: the runtime otherwise retries \
                a failing invocation forever rather than skipping it. Returns 202; the \
                driver applies it at its next backoff check.",
            "parameters": [
                path_param("name", "The wedged effect.", name_schema(effects)),
                path_param(
                    "position",
                    "The log position to mark terminal, from the effect's `position` in `/status`.",
                    json!({ "type": "integer", "minimum": 0 }),
                ),
            ],
            "responses": {
                "202": response("the skip was scheduled", accepted),
                // A typed path parameter, so the request can be rejected before a
                // handler runs. That rejection is axum's, not hekla's, so it is plain
                // text rather than the `Error` envelope. The introspection paths with an
                // integer segment document the same thing.
                "400": {
                    "description": "`position` was not a non-negative integer. This one is \
                        rejected by the routing layer before the handler runs, so the body \
                        is plain text rather than the usual JSON error envelope.",
                    "content": { "text/plain": { "schema": { "type": "string" } } },
                },
                "404": response("no such effect", schema_ref("Error")),
            },
        }
    })
}

fn status_path() -> Value {
    json!({
        "get": {
            "tags": [OPERATIONS_TAG],
            "operationId": "get_status",
            "summary": "per-module positions, lag and errors",
            "description": "Everything needed to tell a lagging module from a wedged one: \
                each projector's and effect's log position and lag, each effect's \
                consecutive-failure count and last error, and the process-wide fold \
                counters. Not a liveness check; use `/health` for that.",
            "responses": {
                "200": response("the runtime's current state", schema_ref("Status")),
            },
        }
    })
}

fn health_path() -> Value {
    let body = json!({
        "type": "object",
        "properties": { "status": { "type": "string", "enum": ["ok"] } },
        "required": ["status"],
        "additionalProperties": false,
    });
    json!({
        "get": {
            "tags": [OPERATIONS_TAG],
            "operationId": "get_health",
            "summary": "liveness check",
            "description": "Answers only whether the process is serving. It reports none of \
                `/status`'s per-module detail, deliberately: a lagging projector is not a \
                reason to restart the process.",
            "responses": { "200": response("the process is serving", body) },
        }
    })
}

fn openapi_path() -> Value {
    json!({
        "get": {
            "tags": [OPERATIONS_TAG],
            "operationId": "get_openapi",
            "summary": "this document",
            "description": "The generated OpenAPI document, serialized once at startup. \
                `hekla openapi <dir>` prints the same document from a project directory \
                without booting a runtime.",
            "responses": {
                "200": response("the OpenAPI document", json!({ "type": "object" })),
            },
        }
    })
}

fn docs_path() -> Value {
    json!({
        "get": {
            "tags": [OPERATIONS_TAG],
            "operationId": "get_docs",
            "summary": "API reference UI",
            "description": "A Scalar reference over `/openapi.json`. The page loads Scalar \
                from a CDN, so it needs a network connection.",
            "responses": {
                "200": {
                    "description": "the reference UI",
                    "content": { "text/html": { "schema": { "type": "string" } } },
                },
            },
        }
    })
}

// ---------------------------------------------------------------------------
// Introspection
// ---------------------------------------------------------------------------

/// The `/admin` paths, keyed by `server`'s own route constants for the same reason
/// the operator paths are.
fn introspection_paths(surface: &Surface) -> Vec<(String, Value)> {
    let projectors = surface.projector_names();
    let effects = surface.effect_names();
    vec![
        (server::ADMIN_ROUTE.to_owned(), admin_index_path()),
        (server::ADMIN_EVENTS_ROUTE.to_owned(), events_path(surface)),
        (server::ADMIN_EVENT_ROUTE.to_owned(), event_path()),
        (server::ADMIN_TRACE_ROUTE.to_owned(), trace_path()),
        (server::ADMIN_EFFECTS_ROUTE.to_owned(), effects_path()),
        (server::ADMIN_EFFECT_ROUTE.to_owned(), effect_path(&effects)),
        (
            server::ADMIN_INVOCATIONS_ROUTE.to_owned(),
            invocations_path(&effects),
        ),
        (
            server::ADMIN_INVOCATION_ROUTE.to_owned(),
            invocation_path(&effects),
        ),
        (server::ADMIN_PROJECTORS_ROUTE.to_owned(), projectors_path()),
        (
            server::ADMIN_PROJECTOR_ROUTE.to_owned(),
            projector_path(&projectors),
        ),
        (server::ADMIN_SCHEMA_ROUTE.to_owned(), schema_path()),
        (server::ADMIN_SYSTEM_ROUTE.to_owned(), system_path()),
        (server::ADMIN_SUBJECTS_ROUTE.to_owned(), subjects_path()),
        (server::ADMIN_SUBJECT_ROUTE.to_owned(), subject_path()),
        (server::ADMIN_ASSETS_ROUTE.to_owned(), assets_path()),
    ]
}

/// `GET /admin/assets/{file}`: one file of the bundled console.
///
/// Described from [`ui::ASSETS`] alone and never from the `HEKLA_UI_DIR` override, so
/// [`build`] stays a pure function of the loaded project. Otherwise two developers
/// would generate different documents from the same source, which is exactly what the
/// single-generator guarantee exists to prevent.
fn assets_path() -> Value {
    json!({
        "get": {
            "tags": [INTROSPECTION_TAG],
            "operationId": "get_admin_asset",
            "summary": "one file of the bundled admin console",
            "description": "The console is compiled into the binary and this serves one \
                of its files. The name is a closed set: the route captures a single path \
                segment and the files are flat, so nothing outside this enum resolves. \n\n\
                Unlike every other path under this prefix, this one is not \
                content-negotiated. It delivers the console rather than being a view of \
                it, so a browser asking for it gets the file.",
            "parameters": [path_param(
                "file",
                "Which file. Every name the binary carries is listed here.",
                name_schema(&ui::asset_names()),
            )],
            "responses": {
                "200": { "description": "the file", "content": ui::media_types() },
                "404": response("no asset by that name", schema_ref("Error")),
            },
        }
    })
}

/// The `?decrypt=` parameter, shared by every endpoint that renders a stored payload.
fn decrypt_param() -> Value {
    query_param(
        "decrypt",
        "Whether to decrypt subject-scoped fields (default `true`). Decryption here is \
         the same boundary `GET /read/...` already crosses for a projector's subject \
         columns; it is not a way around erasure, which removes the key itself. Pass \
         `false` to see the stored ciphertext instead.",
        json!({ "type": "boolean", "default": true }),
    )
}

/// The `?limit=` parameter. No `maximum`, because the handler clamps.
fn admin_limit_param(what: &str) -> Value {
    query_param(
        "limit",
        &format!(
            "How many {what} to return. Larger values are clamped to {} rather than \
             rejected.",
            introspect::MAX_LIMIT
        ),
        json!({ "type": "integer", "minimum": 1, "default": introspect::DEFAULT_LIMIT }),
    )
}

fn admin_index_path() -> Value {
    let body = json!({
        "type": "object",
        "properties": {
            "endpoints": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "description": { "type": "string" },
                    },
                    "required": ["path", "description"],
                    "additionalProperties": false,
                },
            },
        },
        "required": ["endpoints"],
        "additionalProperties": false,
    });
    json!({
        "get": {
            "tags": [INTROSPECTION_TAG],
            "operationId": "get_admin_index",
            "summary": "what is under /admin",
            "description": "A directory of the introspection endpoints. Static: it describes \
                the surface, not the state of the process. The startup log points here.",
            "responses": { "200": response("the introspection endpoints", body) },
        }
    })
}

fn events_path(surface: &Surface) -> Value {
    let page = json!({
        "type": "object",
        "properties": {
            "events": { "type": "array", "items": schema_ref("LogEvent") },
            "next_cursor": position_nullable("The `cursor` for the next page, or null at the end."),
            "log_head": position_schema("The log's head position, which is also its event count."),
        },
        "required": ["events", "next_cursor", "log_head"],
        "additionalProperties": false,
    });
    json!({
        "get": {
            "tags": [INTROSPECTION_TAG],
            "operationId": "list_events",
            "summary": "page the event log",
            "description": "Newest first by default. `type` and `tag` may each be repeated: \
                types are OR'd and tags are AND'd, which is one query item in the store's own \
                terms, so nothing is reinterpreted on the way through. Pagination is by log \
                position rather than an opaque cursor, because positions are dense and 1-based.",
            "parameters": [
                query_param(
                    "type",
                    &format!(
                        "Restrict to these event types. Repeatable; an event matching any of \
                         them is returned. Deliberately not an enum of the declared set: the \
                         log outlives any one deployment, and an event type this project no \
                         longer declares (the case `LogEvent.declared` exists to report) is \
                         exactly what an operator needs to filter for. This project declares \
                         {}.",
                        backticked(&surface.event_types()),
                    ),
                    json!({ "type": "array", "items": { "type": "string" } }),
                ),
                query_param(
                    "tag",
                    "Restrict to events carrying all of these stored tags, each rendered \
                     `key:value` (or bare `key`). Repeatable. A subject-encrypted field's tag \
                     holds ciphertext, so filtering on one means pasting the ciphertext from a \
                     rendered event, not the plaintext.",
                    json!({ "type": "array", "items": { "type": "string" } }),
                ),
                query_param(
                    "direction",
                    "`back` (default) walks from newest to oldest; `forward` walks from oldest \
                     to newest.",
                    json!({ "type": "string", "enum": ["back", "forward"], "default": "back" }),
                ),
                query_param(
                    "cursor",
                    "A log position: the exclusive upper bound going back, the exclusive lower \
                     bound going forward. Pass the previous page's `next_cursor`.",
                    json!({ "type": "integer", "minimum": 0, "format": "int64" }),
                ),
                admin_limit_param("events"),
                decrypt_param(),
            ],
            "responses": {
                "200": response("a page of the log", page),
                "400": response("a malformed filter, direction, cursor or limit", schema_ref("Error")),
                "500": response("the log could not be read", schema_ref("Error")),
            },
        }
    })
}

fn event_path() -> Value {
    json!({
        "get": {
            "tags": [INTROSPECTION_TAG],
            "operationId": "get_event",
            "summary": "one event",
            "description": "The stored event at a log position: its envelope identity, its \
                payload, the state of every subject-scoped field, and its tags including the \
                host's own.",
            "parameters": [
                path_param(
                    "position",
                    "The event's log position. Positions are dense and 1-based.",
                    json!({ "type": "integer", "minimum": 1, "format": "int64" }),
                ),
                decrypt_param(),
            ],
            "responses": {
                "200": response("the event", schema_ref("LogEvent")),
                "400": path_or_query_400(
                    "`decrypt` was not a boolean (JSON), or `position` was not a \
                     non-negative integer (plain text)",
                ),
                "404": response("the log holds no event at that position", schema_ref("Error")),
                "500": response("the log could not be read", schema_ref("Error")),
            },
        }
    })
}

fn trace_path() -> Value {
    let body = json!({
        "type": "object",
        "properties": {
            "correlation_id": { "type": "string", "format": "uuid" },
            "events": { "type": "array", "items": schema_ref("LogEvent") },
            "invocations": {
                "type": "array",
                "items": schema_ref("TraceInvocation"),
                "description": "Which effect invocations ran on the positions on this page. \
                    An event's envelope records that an effect produced it but not which one; \
                    the journal is keyed by effect and position, so this answers that exactly. \
                    An invocation the retention sweeper has already reclaimed is absent. \
                    So is one by an effect this process no longer loads: the join names the \
                    effects the running project declares, so a renamed or deleted effect's \
                    work reads the same as no effect having run. Both absences are silent by \
                    design, and neither means the position was never processed.",
            },
            "complete": {
                "type": "boolean",
                "description": "False when `limit` cut the chain off. A causal chain read \
                    partially is worse than one read whole, so this is stated rather than \
                    left to be inferred from the count. Always the inverse of whether \
                    `next_cursor` is set.",
            },
            "next_cursor": position_nullable(
                "The `cursor` for the rest of the chain, or null when this page is all of it.",
            ),
        },
        "required": ["correlation_id", "events", "invocations", "complete", "next_cursor"],
        "additionalProperties": false,
    });
    json!({
        "get": {
            "tags": [INTROSPECTION_TAG],
            "operationId": "get_trace",
            "summary": "every event of one correlated flow",
            "description": "The whole causal chain a request set off, in log order: the \
                command's own events, then any appended by an effect that reacted to them, \
                transitively. A command response's `correlation_id` is the key. \n\n\
                This is an indexed tag lookup, not a scan: every event carries its flow's \
                correlation as a reserved tag. Events appended before that tag existed carry \
                no correlation tag and cannot appear here.",
            "parameters": [
                path_param(
                    "correlation_id",
                    "The `correlation_id` from a command response, or the `X-Correlation-Id` \
                     the caller sent.",
                    json!({ "type": "string", "format": "uuid" }),
                ),
                query_param(
                    "cursor",
                    "A log position, exclusive: pass the previous page's `next_cursor` to \
                     continue a chain longer than one page.",
                    json!({ "type": "integer", "minimum": 0, "format": "int64" }),
                ),
                admin_limit_param("events"),
                decrypt_param(),
            ],
            "responses": {
                "200": response("the flow's events", body),
                "400": response("the correlation id is not a uuid", schema_ref("Error")),
                "500": response("the log could not be read", schema_ref("Error")),
            },
        }
    })
}

fn effects_path() -> Value {
    let body = json!({
        "type": "object",
        "properties": {
            "effects": { "type": "array", "items": schema_ref("EffectDetail") },
            "log_head": position_schema("The log's head position."),
        },
        "required": ["effects", "log_head"],
        "additionalProperties": false,
    });
    json!({
        "get": {
            "tags": [INTROSPECTION_TAG],
            "operationId": "list_effects",
            "summary": "every effect and its durable state",
            "description": "What `/status` reports, plus the durable watermark and the \
                quarantine record behind the flag.",
            "responses": {
                "200": response("every effect", body),
                "500": response("the operational database could not be read", schema_ref("Error")),
            },
        }
    })
}

fn effect_path(effects: &[&str]) -> Value {
    json!({
        "get": {
            "tags": [INTROSPECTION_TAG],
            "operationId": "get_effect",
            "summary": "one effect",
            "parameters": [
                path_param("name", "The effect.", name_schema(effects)),
            ],
            "responses": {
                "200": response("the effect", schema_ref("EffectDetail")),
                "404": response("no such effect", schema_ref("Error")),
                "500": response("the operational database could not be read", schema_ref("Error")),
            },
        }
    })
}

fn invocations_path(effects: &[&str]) -> Value {
    let body = json!({
        "type": "object",
        "properties": {
            "effect": { "type": "string" },
            "invocations": { "type": "array", "items": schema_ref("EffectInvocation") },
            "next_cursor": position_nullable("The `cursor` for the next page, or null at the end."),
        },
        "required": ["effect", "invocations", "next_cursor"],
        "additionalProperties": false,
    });
    json!({
        "get": {
            "tags": [INTROSPECTION_TAG],
            "operationId": "list_effect_invocations",
            "summary": "an effect's invocations, newest first",
            "description": "One row per event position the effect has reacted to. A `running` \
                row is either in flight or wedged; the two are told apart by the effect's \
                `consecutive_failures`. Completed invocations are swept after the retention \
                window, so this is not the whole history of a long-lived effect.",
            "parameters": [
                path_param("name", "The effect.", name_schema(effects)),
                query_param(
                    "cursor",
                    "An invocation position: the exclusive upper bound of the next page.",
                    json!({ "type": "integer", "minimum": 0, "format": "int64" }),
                ),
                admin_limit_param("invocations"),
            ],
            "responses": {
                "200": response("a page of invocations", body),
                "400": response("a malformed cursor or limit", schema_ref("Error")),
                "404": response("no such effect", schema_ref("Error")),
                "500": response("the operational database could not be read", schema_ref("Error")),
            },
        }
    })
}

fn invocation_path(effects: &[&str]) -> Value {
    json!({
        "get": {
            "tags": [INTROSPECTION_TAG],
            "operationId": "get_effect_invocation",
            "summary": "one invocation and every call it journaled",
            "description": "The ordered sequence of impure calls the handler made and what \
                each returned. This is what a wedged effect is diagnosed from: the calls \
                already listed are recorded, so a retry replays them rather than re-firing, \
                and the first one missing is where it is stuck.\n\n\
                A call's *arguments* are not stored, only hashed, so this reports what came \
                back and not what was sent. That is deliberate: a request body can hold \
                plaintext that came out of `reveal()`, which would then outlive the erasure \
                of the subject it belonged to.",
            "parameters": [
                path_param("name", "The effect.", name_schema(effects)),
                path_param(
                    "position",
                    "The log position of the event that triggered the invocation.",
                    json!({ "type": "integer", "minimum": 0, "format": "int64" }),
                ),
                query_param(
                    "cursor",
                    "The `next_cursor` of the previous page: the `seq` of its last call. \
                     The calls page, because reading a truncated list as the whole \
                     sequence would point at the wrong call as the one it is stuck on.",
                    json!({ "type": "integer", "minimum": 0, "format": "int64" }),
                ),
                admin_limit_param("journaled calls"),
            ],
            "responses": {
                "200": response(
                    "the invocation and its journaled calls",
                    schema_ref("EffectInvocationDetail"),
                ),
                "400": path_or_query_400(
                    "a malformed `cursor` or `limit` (JSON), or a `position` that is not a \
                     non-negative integer (plain text)",
                ),
                "404": response("no such effect, or no invocation at that position", schema_ref("Error")),
                "500": response("the operational database could not be read", schema_ref("Error")),
            },
        }
    })
}

fn projectors_path() -> Value {
    let body = json!({
        "type": "object",
        "properties": {
            "projectors": { "type": "array", "items": schema_ref("ProjectorDetail") },
            "log_head": position_schema("The log's head position."),
        },
        "required": ["projectors", "log_head"],
        "additionalProperties": false,
    });
    json!({
        "get": {
            "tags": [INTROSPECTION_TAG],
            "operationId": "list_projectors",
            "summary": "every projector and its entities",
            "description": "Reads no projector database, so `definition_hash` and each \
                entity's `rows` are null here. Ask for one projector to get those.",
            "responses": { "200": response("every projector", body) },
        }
    })
}

fn projector_path(projectors: &[&str]) -> Value {
    json!({
        "get": {
            "tags": [INTROSPECTION_TAG],
            "operationId": "get_projector",
            "summary": "one projector, with the definition its model was built under",
            "description": "`definition_hash` is read out of the read model itself, so it is \
                what the stored rows were actually built from rather than what the loaded \
                project declares. The two differing is what a rebuild resolves.",
            "parameters": [
                path_param("name", "The projector.", name_schema(projectors)),
                query_param(
                    "counts",
                    "Include each entity's row count (default `false`). Opt-in because a count \
                     is a full table scan per entity, and it requires a `ready` projector: a \
                     model still at a previous definition's shape has no table to count.",
                    json!({ "type": "boolean", "default": false }),
                ),
            ],
            "responses": {
                "200": response("the projector", schema_ref("ProjectorDetail")),
                "400": response("a malformed `counts`", schema_ref("Error")),
                "404": response("no such projector", schema_ref("Error")),
                "503": response(
                    "counts were asked for but the read model is not servable",
                    schema_ref("Error"),
                ),
                "500": response("the read model could not be opened", schema_ref("Error")),
            },
        }
    })
}

fn schema_path() -> Value {
    let body = json!({
        "type": "object",
        "properties": {
            "events": { "type": "array", "items": schema_ref("EventDetail") },
            "commands": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "internal": {
                            "type": "boolean",
                            "description": "An internal command is not routed, so it is absent \
                                from this document. It is reported here because it exists and \
                                an effect can invoke it.",
                        },
                        "path": { "type": "string", "description": "Project-relative source path." },
                        "hash": { "type": "string", "description": "Its digest entry hash." },
                        // Not a `FieldDetail`: a command's `input = schema(...)` carries
                        // a name and a kind and nothing else. Tagging, subjects and
                        // uniqueness are event and entity policy, and `schema()` rejects
                        // them outright, so there is no `indexed`/`subject`/`unique` to
                        // report and claiming otherwise would fail every validator.
                        "input": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "name": { "type": "string" },
                                    "kind": {
                                        "type": "string",
                                        "description": "The field constructor as declared, \
                                            e.g. `uuid()` or `optional(str())`.",
                                    },
                                },
                                "required": ["name", "kind"],
                                "additionalProperties": false,
                            },
                        },
                    },
                    "required": ["name", "internal", "path", "hash", "input"],
                    "additionalProperties": false,
                },
            },
            "projectors": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "sources": sources_schema(),
                        "entities": { "type": "array", "items": { "type": "string" } },
                    },
                    "required": ["name", "sources", "entities"],
                    "additionalProperties": false,
                },
            },
            "effects": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "sources": sources_schema(),
                    },
                    "required": ["name", "sources"],
                    "additionalProperties": false,
                },
            },
            "declarations": { "type": "array", "items": schema_ref("DeclarationSummary") },
        },
        "required": ["events", "commands", "projectors", "effects", "declarations"],
        "additionalProperties": false,
    });
    json!({
        "get": {
            "tags": [INTROSPECTION_TAG],
            "operationId": "get_project_schema",
            "summary": "the project this process loaded",
            "description": "The declared vocabulary and the modules serving it, with the digest \
                hash of each as recorded at boot. Answers \"is what is running what I \
                deployed?\" without shelling into the host.",
            "responses": {
                "200": response("the loaded project", body),
                "500": response("the operational database could not be read", schema_ref("Error")),
            },
        }
    })
}

fn system_path() -> Value {
    json!({
        "get": {
            "tags": [INTROSPECTION_TAG],
            "operationId": "get_system",
            "summary": "version, uptime, storage and effective configuration",
            "responses": {
                "200": response("the process", schema_ref("SystemInfo")),
                "500": response("the operational database could not be read", schema_ref("Error")),
            },
        }
    })
}

fn subjects_path() -> Value {
    let body = json!({
        "type": "object",
        "properties": {
            "counts": {
                "type": ["array", "null"],
                "description": "Live key counts per subject field. Present on the first page \
                    and null on every continuation: a count of a group is an aggregate no \
                    limit can bound, and it cannot change between pages of one walk.",
                "items": {
                    "type": "object",
                    "properties": {
                        "subject_field": { "type": "string" },
                        "live_keys": { "type": "integer", "minimum": 0, "format": "int64" },
                    },
                    "required": ["subject_field", "live_keys"],
                    "additionalProperties": false,
                },
            },
            "subjects": { "type": "array", "items": schema_ref("SubjectEntry") },
            "next": {
                "type": ["object", "null"],
                "properties": {
                    "after_field": { "type": "string" },
                    "after_value": { "type": "string" },
                },
                "required": ["after_field", "after_value"],
                "additionalProperties": false,
                "description": "Pass both back as query parameters for the next page.",
            },
        },
        "required": ["counts", "subjects", "next"],
        "additionalProperties": false,
    });
    json!({
        "get": {
            "tags": [INTROSPECTION_TAG],
            "operationId": "list_subjects",
            "summary": "which subjects still hold key material",
            "description": "Never the key material itself. A subject absent from this list has \
                either been erased or never had a value encrypted under it: erasure deletes the \
                row, so on disk the two are one state. The reserved global uniqueness secret is \
                excluded, since it is not a subject and cannot be erased.",
            "parameters": [
                query_param("after_field", "The previous page's `next.after_field`.", json!({ "type": "string" })),
                query_param("after_value", "The previous page's `next.after_value`.", json!({ "type": "string" })),
                admin_limit_param("subjects"),
            ],
            "responses": {
                "200": response("the subject-key inventory", body),
                "400": response("a malformed limit, or only one half of the cursor", schema_ref("Error")),
                "500": response("the operational database could not be read", schema_ref("Error")),
            },
        }
    })
}

fn subject_path() -> Value {
    let body = json!({
        "type": "object",
        "properties": {
            "subject_field": { "type": "string" },
            "subject_value": { "type": "string" },
            "state": {
                "type": "string",
                "enum": ["live", "absent"],
                "description": "`absent` rather than `erased`: a subject that never had a value \
                    encrypted under it is indistinguishable from an erased one, because erasure \
                    deletes the row.",
            },
        },
        "required": ["subject_field", "subject_value", "state"],
        "additionalProperties": false,
    });
    json!({
        "get": {
            "tags": [INTROSPECTION_TAG],
            "operationId": "get_subject",
            "summary": "whether one subject still has a key",
            "parameters": [
                path_param("field", "The subject field, e.g. `customer_id`.", json!({ "type": "string" })),
                path_param("value", "The subject id value.", json!({ "type": "string" })),
            ],
            "responses": {
                "200": response("the subject's key state", body),
                "500": response("the operational database could not be read", schema_ref("Error")),
            },
        }
    })
}

/// The `400` for a path whose parameters can be rejected in two different layers.
///
/// A bad query parameter reaches the handler and comes back as the JSON `Error`
/// envelope; a bad typed path segment is rejected by axum before the handler runs and
/// comes back as plain text. Documenting only the first would make a client that parses
/// `error.code` on every 4xx fail on the second.
fn path_or_query_400(description: &str) -> Value {
    json!({
        "description": description,
        "content": {
            "application/json": { "schema": schema_ref("Error") },
            "text/plain": { "schema": { "type": "string" } },
        },
    })
}

/// A module's declared subscription: the event types its arms select. Empty is a
/// module subscribed to nothing, which is the only way to select none of them.
fn sources_schema() -> Value {
    json!({
        "type": "array",
        "items": { "type": "string" },
        "description": "The event types this module subscribes to.",
    })
}

// ---------------------------------------------------------------------------
// Schemas
// ---------------------------------------------------------------------------

fn schemas(surface: &Surface, names: &ComponentNames) -> Value {
    let mut out = Map::new();
    out.insert("ErrorDetail".to_owned(), error_detail_schema());
    out.insert("Error".to_owned(), error_schema());
    out.insert("CommandError".to_owned(), command_error_schema());
    out.insert("CommandAccepted".to_owned(), command_accepted_schema());
    out.insert("EmittedEvent".to_owned(), emitted_event_schema(surface));
    out.insert("Status".to_owned(), status_schema());
    out.insert("ProjectorStatus".to_owned(), projector_status_schema());
    out.insert("EffectStatus".to_owned(), effect_status_schema());
    out.insert("LogEvent".to_owned(), log_event_schema());
    out.insert("SubjectState".to_owned(), subject_state_schema());
    out.insert("EffectDetail".to_owned(), effect_detail_schema());
    out.insert("EffectInvocation".to_owned(), effect_invocation_schema());
    out.insert(
        "EffectInvocationDetail".to_owned(),
        effect_invocation_detail_schema(),
    );
    out.insert("TraceInvocation".to_owned(), trace_invocation_schema());
    out.insert("JournalCall".to_owned(), journal_call_schema());
    out.insert("FieldDetail".to_owned(), field_detail_schema());
    out.insert("EventDetail".to_owned(), event_detail_schema());
    out.insert("EntityDetail".to_owned(), entity_detail_schema());
    out.insert("ProjectorDetail".to_owned(), projector_detail_schema());
    out.insert(
        "DeclarationSummary".to_owned(),
        declaration_summary_schema(),
    );
    out.insert("SystemInfo".to_owned(), system_info_schema());
    out.insert("SubjectEntry".to_owned(), subject_entry_schema());
    for (event_type, def) in &surface.events {
        out.insert(
            names.event(event_type).to_owned(),
            event_schema(event_type, def),
        );
    }
    for projector in &surface.projectors {
        for entity in projector.entities {
            out.insert(
                names.entity(projector.name, &entity.name).to_owned(),
                entity_schema(projector, entity),
            );
        }
    }
    debug_assert!(
        FIXED_SCHEMAS.iter().all(|name| out.contains_key(*name)),
        "FIXED_SCHEMAS seeds the name assignment, so it must list exactly what is inserted above"
    );
    Value::Object(out)
}

fn error_detail_schema() -> Value {
    json!({
        "type": "object",
        "description": "A machine-readable code and a human-readable message.",
        "properties": {
            "code": {
                "type": "string",
                "description": "A stable reason to branch on, e.g. `invalid_input`, \
                    `not_found`, `unindexed_filter`, `concurrency_conflict`, or the code \
                    derived from the name of a refusal the command declared.",
            },
            "message": {
                "type": "string",
                "description": "Human-readable detail. Not stable; do not parse it.",
            },
        },
        "required": ["code", "message"],
        "additionalProperties": false,
    })
}

fn error_schema() -> Value {
    json!({
        "type": "object",
        "description": "The error envelope the read and operator endpoints return. The \
            command endpoints return `CommandError`, which adds the request's \
            correlation identity.",
        "properties": { "error": schema_ref("ErrorDetail") },
        "required": ["error"],
        "additionalProperties": false,
    })
}

fn command_error_schema() -> Value {
    json!({
        "type": "object",
        "description": "The error envelope a command endpoint returns. Every error path \
            shares it, so a 400, a 409 and a 422 have the same shape.",
        "properties": {
            "correlation_id": { "type": "string", "format": "uuid" },
            "causation_id": { "type": "string", "format": "uuid" },
            "error": schema_ref("ErrorDetail"),
        },
        "required": ["correlation_id", "causation_id", "error"],
        "additionalProperties": false,
    })
}

fn command_accepted_schema() -> Value {
    json!({
        "type": "object",
        "description": "A committed command. Byte-identical whether the command ran now \
            or its outcome was recovered from the log under an idempotency key, so a \
            client cannot tell a retry from a first attempt.",
        "properties": {
            "correlation_id": {
                "type": "string",
                "format": "uuid",
                "description": "Echoes the request's `X-Correlation-Id`, or a fresh uuid \
                    when none was supplied.",
            },
            "causation_id": { "type": "string", "format": "uuid" },
            "positions": {
                "type": ["object", "null"],
                "description": "The range of log positions the appended events occupy. \
                    Null when the command committed no events. Pass `last` as a read's \
                    `after` for read-your-writes.",
                "properties": {
                    "first": position_schema("The first appended position."),
                    "last": position_schema("The last appended position."),
                },
                "required": ["first", "last"],
                "additionalProperties": false,
            },
            "events": {
                "type": "array",
                "description": "The events the command appended, in order. Empty when it \
                    decided to append none, which is a success rather than a rejection.",
                "items": schema_ref("EmittedEvent"),
            },
        },
        "required": ["correlation_id", "causation_id", "positions", "events"],
        "additionalProperties": false,
    })
}

fn emitted_event_schema(surface: &Surface) -> Value {
    let mut event_type = name_schema(&surface.event_types());
    if let Some(object) = event_type.as_object_mut() {
        object.insert(
            "description".to_owned(),
            Value::String(
                "The event type. Its declared fields are documented under the matching \
                 `event.` schema."
                    .to_owned(),
            ),
        );
    }
    json!({
        "type": "object",
        "description": "One appended event as reported on the wire: its type and its \
            derived tags. Not the event's fields, which live only in the log. See the \
            `event.*` schemas for what each type declares.",
        "properties": {
            "type": event_type,
            "tags": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Derived tags, rendered `key:value` (or a bare `key` for a \
                    valueless tag) and sorted. Subject-encrypted and `unique` tags are \
                    omitted: their stored form is ciphertext and the idempotent-recovery \
                    path cannot reconstruct them, so omitting them keeps a fresh response \
                    and a recovered one identical.",
            },
        },
        "required": ["type", "tags"],
        "additionalProperties": false,
    })
}

fn status_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "log_head": position_schema("The log's head position."),
            "uptime_seconds": { "type": "integer", "minimum": 0 },
            "verify": {
                "type": "boolean",
                "description": "Whether the continuous invariant checks are running \
                    (`[verify] enabled`, or `serve --verify`).",
            },
            "commands": {
                "type": "object",
                "properties": {
                    "public": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Routed under `/commands/{name}`.",
                    },
                    "internal": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Invokable by effects, never routed over HTTP.",
                    },
                },
                "required": ["public", "internal"],
                "additionalProperties": false,
            },
            "projectors": { "type": "array", "items": schema_ref("ProjectorStatus") },
            "effects": { "type": "array", "items": schema_ref("EffectStatus") },
            "events": {
                "type": "integer",
                "minimum": 0,
                "description": "How many event types the project declares.",
            },
        },
        "required": [
            "log_head", "uptime_seconds", "verify", "commands",
            "projectors", "effects", "events",
        ],
        "additionalProperties": false,
    })
}

fn projector_status_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "position": position_schema("The last checkpoint it committed."),
            "lag": {
                "type": "integer",
                "minimum": 0,
                "description": "`log_head - position`. Ordinary lag, not an error.",
            },
            "readiness": {
                "type": "string",
                "enum": ["ready", "rebuilding", "stale", "rebuild_failed", "quarantined"],
                "description": "Whether the read API can serve this projector. Anything \
                    but `ready` is a 503 on its read paths.",
            },
            "running": {
                "type": "boolean",
                "description": "Whether its thread is alive. A stopped projector still \
                    serves its frozen model but will never advance or replay again.",
            },
            "failed": { "type": "boolean" },
            "replays_completed": {
                "type": "integer",
                "minimum": 0,
                "description": "Rebuilds finished since boot. A rebuild happens into a \
                    sibling file and swaps in by rename, so this is the only thing that \
                    says one happened at all.",
            },
            "replays_failed": {
                "type": "integer",
                "minimum": 0,
                "description": "Rebuild attempts that failed since boot. A non-zero count \
                    with `failed` set is a projector serving a frozen model.",
            },
            "last_error": { "type": ["string", "null"] },
        },
        "required": [
            "name", "position", "lag", "readiness", "running", "failed",
            "replays_completed", "replays_failed", "last_error",
        ],
        "additionalProperties": false,
    })
}

/// The one-word effect state, written once because `/status` and `/admin/effects`
/// both report it and it comes from one function in the runtime.
fn effect_state_schema() -> Value {
    json!({
        "type": "string",
        "enum": ["healthy", "lagging", "quarantined", "wedged"],
        "description": "What the counters below add up to, derived once in the runtime \
            so no two readers disagree. `quarantined` outranks `wedged`, which outranks \
            `lagging`: a quarantine restored from an earlier process carries no failure \
            count, and a wedged effect lags precisely because it is wedged, so reporting \
            the symptom would bury the cause. \n\n\
            `lagging` is normal and transient. The driver polls on an interval while the \
            log head is read per request, so every append leaves a healthy effect briefly \
            behind. Treat a sustained `lagging` as a signal and a momentary one as noise.",
    })
}

fn effect_status_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "state": effect_state_schema(),
            "position": position_schema("Its durable watermark."),
            "lag": { "type": "integer", "minimum": 0 },
            "consecutive_failures": {
                "type": "integer",
                "minimum": 0,
                "description": "How many times the current invocation has failed. A number \
                    that keeps climbing is a wedge; the runtime retries forever rather \
                    than skipping, so this is how a wedge is distinguished from lag.",
            },
            "last_error": { "type": ["string", "null"] },
            "quarantined": {
                "type": "boolean",
                "description": "Set when it broke an invariant under `--verify` and stopped \
                    advancing. The rest of the runtime keeps serving.",
            },
            "terminal_skips": {
                "type": "integer",
                "minimum": 0,
                "description": "How many positions were marked terminal without being \
                    processed, by an operator skip or a terminal failure.",
            },
            "last_terminal_error": { "type": ["string", "null"] },
        },
        "required": [
            "name", "state", "position", "lag", "consecutive_failures", "last_error",
            "quarantined", "terminal_skips", "last_terminal_error",
        ],
        "additionalProperties": false,
    })
}

fn event_schema(event_type: &str, def: &EventDef) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for (name, meta) in &def.fields {
        properties.insert(name.clone(), annotated_event_field(meta));
        if !meta.kind.is_nullable() {
            required.push(Value::String(name.clone()));
        }
    }
    json!({
        "type": "object",
        "title": event_type,
        "description": format!(
            "The `{event_type}` event as recorded in the log. This documents the domain \
             vocabulary, not a request or response body: an event's fields never appear \
             on the wire. A command's 200 reports only each event's type and plaintext \
             tags."
        ),
        "properties": Value::Object(properties),
        "required": required,
        "additionalProperties": false,
    })
}

/// An event field: its schema, its declared policy as prose, and the same policy as
/// `x-hekla-*` so a generator can read it without parsing English.
fn annotated_event_field(meta: &FieldMeta) -> Value {
    let mut extensions = vec![("x-hekla-indexed", Value::Bool(meta.indexed))];
    if let Some(subject) = &meta.subject {
        extensions.push(("x-hekla-subject", Value::String(subject.clone())));
    }
    annotated(&meta.kind, event_field_notes(meta), extensions)
}

fn event_field_notes(meta: &FieldMeta) -> Vec<String> {
    let mut notes = Vec::new();
    if let Some(subject) = &meta.subject {
        notes.push(format!(
            "Encrypted under the subject `{subject}`: stored as ciphertext, and readable \
             only through `reveal()` in an effect. Erasing that subject makes it \
             permanently unreadable."
        ));
    }
    notes.push(
        if meta.indexed {
            "Indexed, so a query, fold or dispatch clause can filter on it."
        } else {
            "Not indexed, so no clause can filter on it."
        }
        .to_owned(),
    );
    notes
}

fn entity_schema(projector: &ProjectorSurface, entity: &EntityDef) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for (name, meta) in &entity.fields {
        properties.insert(name.clone(), annotated_entity_field(entity, name, meta));
        // A NULL column is omitted from a row, and a subject column whose key was erased
        // (or that will not decrypt under the current key) is removed rather than nulled,
        // so neither can be required.
        if !meta.kind.is_nullable() && meta.subject.is_none() {
            required.push(Value::String(name.clone()));
        }
    }
    let indexes: Vec<Value> = entity
        .indexes
        .iter()
        .map(|index| json!({ "name": index.name, "columns": index.columns }))
        .collect();
    json!({
        "type": "object",
        "title": entity.name,
        "description": format!(
            "A row of the `{}` entity in the `{}` projector's read model.",
            entity.name, projector.name,
        ),
        "properties": Value::Object(properties),
        "required": required,
        "additionalProperties": false,
        "x-hekla-key": entity.key,
        "x-hekla-indexes": indexes,
    })
}

/// A read-model column. Deliberately not the same annotations as an event field:
/// `indexed` and `unique` are event-field policy, and an entity column carries
/// neither. What a reader needs here is whether it can be filtered on, which comes
/// from the key and the declared indexes rather than from the field's own metadata.
fn annotated_entity_field(entity: &EntityDef, name: &str, meta: &FieldMeta) -> Value {
    // Answered once and passed down. Each call rescans the index list, and two call
    // sites could drift into disagreeing about the same column in the flag and in the
    // prose right beside it.
    let filterable = read_api::is_filterable(entity, name);
    let mut extensions = vec![("x-hekla-filterable", Value::Bool(filterable))];
    // Not `x-hekla-key`: the entity schema already uses that name for the key's *name*,
    // and one extension carrying a string in one place and a boolean in another forces
    // a reader to branch on the JSON type to learn what it means.
    if name == entity.key {
        extensions.push(("x-hekla-is-key", Value::Bool(true)));
    }
    if let Some(subject) = &meta.subject {
        extensions.push(("x-hekla-subject", Value::String(subject.clone())));
    }
    annotated(
        &meta.kind,
        entity_field_notes(entity, name, meta, filterable),
        extensions,
    )
}

fn entity_field_notes(
    entity: &EntityDef,
    name: &str,
    meta: &FieldMeta,
    filterable: bool,
) -> Vec<String> {
    let mut notes = Vec::new();
    if name == entity.key {
        notes.push("The entity's key. Unique, and always filterable.".to_owned());
    } else if filterable {
        notes.push("Filterable: the leftmost column of a declared index.".to_owned());
    } else {
        notes.push(
            "Not filterable. Only the key and the leftmost column of each declared index \
             are, so filtering on this is a 400."
                .to_owned(),
        );
    }
    if let Some(subject) = &meta.subject {
        notes.push(format!(
            "Encrypted under the subject `{subject}`, and decrypted on read. Absent from \
             the row when that subject's key has been erased or the value will not \
             decrypt under the current master."
        ));
    }
    notes
}

/// A field's schema plus its declared metadata, as prose and as `x-hekla-*`, so it is
/// readable and machine-consumable at once. The caller picks the extensions, because
/// what is worth recording differs between an event field and a read-model column.
fn annotated(kind: &FieldKind, notes: Vec<String>, extensions: Vec<(&str, Value)>) -> Value {
    let mut schema = field_schema(kind);
    let Some(object) = schema.as_object_mut() else {
        return schema;
    };
    // Appended, never assigned. `field_schema` already describes the kinds whose wire
    // form is not obvious from `type` alone (`money` is a decimal string, `uint` states
    // a ceiling no numeric bound can carry), and overwriting that leaves a `money`
    // column indistinguishable from any other string.
    if !notes.is_empty() {
        let existing = object.get("description").and_then(Value::as_str);
        let described = match existing {
            Some(existing) => format!("{existing}. {}", notes.join(" ")),
            None => notes.join(" "),
        };
        object.insert("description".to_owned(), Value::String(described));
    }
    for (key, value) in extensions {
        object.insert(key.to_owned(), value);
    }
    schema
}

/// Widen a schema to also admit null.
///
/// Both halves are needed. `type` becomes a two-element array, and an `enum` has to
/// gain null as well, because a `one_of` field's enum would otherwise reject the null
/// its own `type` now permits.
///
/// Only request bodies use this. A read-model row omits a null column rather than
/// sending one, so an entity schema encodes optionality by absence from `required`.
fn nullable(schema: Value) -> Value {
    let mut schema = schema;
    let Some(object) = schema.as_object_mut() else {
        return schema;
    };
    match object.get("type") {
        Some(Value::String(single)) => {
            let widened = json!([single, "null"]);
            object.insert("type".to_owned(), widened);
        }
        // `json()` is `{}`, which already admits null.
        _ => return schema,
    }
    if let Some(Value::Array(variants)) = object.get_mut("enum") {
        variants.push(Value::Null);
    }
    schema
}

/// One field's JSON Schema. `optional` reaches through to the inner kind; whether the
/// caller also widens it with [`nullable`] depends on whether that position accepts an
/// explicit null on the wire.
fn field_schema(kind: &FieldKind) -> Value {
    match kind.base() {
        FieldKind::Text { max_length } => {
            let mut schema = json!({ "type": "string" });
            if let Some(max) = max_length {
                schema["maxLength"] = json!(max);
            }
            schema
        }
        FieldKind::Uuid => json!({ "type": "string", "format": "uuid" }),
        FieldKind::Timestamp => json!({ "type": "string", "format": "date-time" }),
        FieldKind::Money { .. } => json!({ "type": "string", "description": "decimal amount" }),
        FieldKind::OneOf(variants) => json!({ "type": "string", "enum": variants }),
        FieldKind::I64 => json!({ "type": "integer", "format": "int64" }),
        FieldKind::Bool => json!({ "type": "boolean" }),
        FieldKind::Json => json!({}),
        FieldKind::Optional(_) => unreachable!("base() strips Optional"),
    }
}

/// A stored event as introspection renders it.
fn log_event_schema() -> Value {
    json!({
        "type": "object",
        "description": "One event as the log holds it. Unlike `EmittedEvent`, which reports \
            what a command appended, this is the stored form: the full payload, the host's \
            own reserved tags, and the state of every subject-scoped field.",
        "properties": {
            "position": position_schema("The event's log position. Dense and 1-based."),
            "type": { "type": "string" },
            "declared": {
                "type": "boolean",
                "description": "Whether the loaded project declares this event type. False \
                    means the log holds an event this deployment no longer knows about, which \
                    is a fact about the two disagreeing rather than corruption.",
            },
            "event_id": { "type": "string", "format": "uuid" },
            "timestamp": { "type": "string", "format": "date-time" },
            "correlation_id": {
                "type": "string",
                "format": "uuid",
                "description": "The flow this event belongs to. Pass it to `/admin/traces/{correlation_id}`.",
            },
            "causation_id": {
                "type": "string",
                "format": "uuid",
                "description": "The command execution that produced this event. Every event of \
                    one execution shares it.",
            },
            "triggering_event_id": {
                "type": "string",
                "format": "uuid",
                "description": "The event that triggered the command that produced this one. \
                    Absent for a command called directly over HTTP.",
            },
            "data": {
                "type": "object",
                "description": "The event's payload. A subject-scoped field holds its plaintext \
                    when it could be decrypted and its stored ciphertext otherwise; `subjects` \
                    says which.",
            },
            "subjects": {
                "type": "object",
                "additionalProperties": schema_ref("SubjectState"),
                "description": "One entry per subject-scoped field the event declares. Empty \
                    when it declares none.",
            },
            "tags": {
                "type": "array",
                "items": { "type": "string" },
                "description": "The event's stored tags, `key:value` or bare `key`. A \
                    subject-scoped field's tag holds ciphertext, which is what makes it a usable \
                    join key without revealing the value.",
            },
            "hekla_tags": {
                "type": "array",
                "items": { "type": "string" },
                "description": "The host's own reserved tags: the correlation tag, a keyed \
                    command's idempotency tag, and a `unique` field's global-key tag. Stripped \
                    from command responses; shown here because they are what the log holds.",
            },
        },
        "required": [
            "position", "type", "declared", "event_id", "timestamp",
            "correlation_id", "causation_id", "data", "subjects", "tags", "hekla_tags"
        ],
    })
}

fn subject_state_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "subject": {
                "type": "string",
                "description": "The sibling field whose value scopes this field's key.",
            },
            "subject_value": {
                "type": ["string", "null"],
                "description": "That sibling's value, in plaintext (a subject id is not itself \
                    encrypted, and subjects do not chain). Null if it could not be read.",
            },
            "state": {
                "type": "string",
                "enum": ["decrypted", "encrypted", "erased", "stale", "unreadable"],
                "description": "`decrypted`: the value in `data` is plaintext. `erased`: the \
                    subject's key is gone, so `data` holds ciphertext and always will. \
                    `stale`: the subject has a key, but this value was written under a \
                    superseded one (erased, then recreated by a later event) or is corrupt, \
                    so it does not decrypt under the current key. `unreadable`: the key \
                    could not be obtained at all, from a corrupt wrapping or a master that \
                    is not configured; the server log names it. `encrypted`: nothing was \
                    attempted, because the request passed `decrypt=false` or no master key \
                    is configured.",
            },
        },
        "required": ["subject", "subject_value", "state"],
        "additionalProperties": false,
    })
}

fn effect_detail_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "state": effect_state_schema(),
            "position": position_schema("The effect's in-memory watermark."),
            "lag": position_schema("Log head minus position."),
            "retry_in_ms": {
                "type": ["integer", "null"],
                "minimum": 0,
                "format": "int64",
                "description": "Milliseconds until the next attempt, so a client can count \
                    down rather than poll blindly. A remaining duration rather than an \
                    instant on purpose: the reader's clock is a different machine's, and a \
                    published deadline would render as a negative or hour-long countdown for \
                    a retry that is actually moments away. Null when nothing is waiting; \
                    null alongside a non-zero `consecutive_failures` means an attempt is in \
                    flight right now.",
            },
            "sources": sources_schema(),
            "watermark": {
                "type": ["integer", "null"],
                "minimum": 0,
                "format": "int64",
                "description": "The durable resume point. Null means the effect has never run, \
                    which the driver itself treats the same as zero but an operator should not.",
            },
            "consecutive_failures": {
                "type": "integer",
                "minimum": 0,
                "format": "int64",
                "description": "Retries at the current position. Non-zero with a `running` \
                    invocation is a wedge.",
            },
            "last_error": { "type": ["string", "null"] },
            "terminal_skips": {
                "type": "integer",
                "minimum": 0,
                "format": "int64",
                "description": "Invocations completed without doing their work since this \
                    process started. Process-local: a restart resets it, and the durable trace \
                    of a skipped position is a terminal invocation row indistinguishable from a \
                    completed one.",
            },
            "last_terminal_error": { "type": ["string", "null"] },
            "quarantined": { "type": "boolean" },
            "quarantine": {
                "type": ["object", "null"],
                "properties": {
                    "position": position_schema("Where the invariant broke."),
                    "reason": { "type": "string" },
                    "at": { "type": "string" },
                },
                "required": ["position", "reason", "at"],
                "additionalProperties": false,
                "description": "The durable quarantine record. Unlike a projector's, which is \
                    in memory only, this survives a restart by design.",
            },
        },
        "required": [
            "name", "state", "position", "lag", "retry_in_ms", "sources", "watermark",
            "consecutive_failures", "last_error", "terminal_skips", "last_terminal_error",
            "quarantined", "quarantine"
        ],
        "additionalProperties": false,
    })
}

/// An invocation as a trace sees it: enough to name it and link to its journal, and
/// no more. The full row is `EffectInvocation`, one request away.
fn trace_invocation_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "effect": { "type": "string" },
            "position": position_schema("The log position of the triggering event."),
            "status": {
                "type": "string",
                "enum": ["running", "terminal"],
                "description": "`running` on an event in this chain means the effect is \
                    still working on it, or is wedged on it.",
            },
        },
        "required": ["effect", "position", "status"],
        "additionalProperties": false,
    })
}

fn effect_invocation_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "position": position_schema("The log position of the triggering event."),
            "status": {
                "type": "string",
                "enum": ["running", "terminal"],
                "description": "The only two persisted states. `terminal` covers success, an \
                    operator skip and a terminal `reveal()` alike: the runtime records all three \
                    as done.",
            },
            "script_hash": {
                "type": "string",
                "description": "The effect's digest entry hash at the time the invocation ran. \
                    A replay check skips an invocation whose effect has since changed behaviour; \
                    a reformat is not such a change, so the check keeps its coverage across one.",
            },
            "created_at": { "type": "string" },
            "completed_at": { "type": ["string", "null"] },
        },
        "required": ["position", "status", "script_hash", "created_at", "completed_at"],
        "additionalProperties": false,
    })
}

/// The invocation view: the row plus its page of journaled calls.
///
/// Spelled out rather than composed with `allOf` over [`EffectInvocation`]. An `allOf`
/// branch is validated against the whole instance, so a branch carrying
/// `additionalProperties: false` rejects the very key the other branch adds, and the
/// document would declare a response nothing can satisfy.
fn effect_invocation_detail_schema() -> Value {
    let Value::Object(mut base) = effect_invocation_schema() else {
        unreachable!("an invocation schema is an object");
    };
    let Some(Value::Object(properties)) = base.get_mut("properties") else {
        unreachable!("an invocation schema has properties");
    };
    properties.insert(
        "calls".to_owned(),
        json!({ "type": "array", "items": schema_ref("JournalCall") }),
    );
    properties.insert(
        "next_cursor".to_owned(),
        position_nullable(
            "The `cursor` for the next page of calls, or null when this page is the \
             end of the sequence.",
        ),
    );
    let Some(Value::Array(required)) = base.get_mut("required") else {
        unreachable!("an invocation schema has a required list");
    };
    required.push(json!("calls"));
    required.push(json!("next_cursor"));
    base.insert(
        "description".to_owned(),
        json!("One invocation and one page of the calls it journaled."),
    );
    Value::Object(base)
}

fn journal_call_schema() -> Value {
    json!({
        "type": "object",
        "description": "One journaled call, in the order the handler made it. A call recorded \
            here will replay on the next attempt rather than fire again.",
        "properties": {
            "seq": {
                "type": "integer",
                "minimum": 0,
                "description": "The call's ordinal in the whole sequence, which keeps \
                    counting across pages.",
            },
            "kind": {
                "type": ["string", "null"],
                "enum": ["http", "invoke", "now", "erase", null],
                "description": "Which builtin made the call. Null for a row written before the \
                    runtime recorded it; the kind is otherwise unrecoverable, since it exists \
                    only inside the hash pre-image.",
            },
            "call_hash": {
                "type": "string",
                "description": "The content hash of the kind and arguments. The arguments \
                    themselves are not stored.",
            },
            "disambiguator": {
                "type": "integer",
                "minimum": 0,
                "format": "int64",
                "description": "Counts repeats of a byte-identical call within one invocation, \
                    so a handler that loops genuinely re-fires rather than replaying itself.",
            },
            "result": { "description": "What the call returned, as recorded." },
            "created_at": { "type": "string" },
        },
        "required": ["seq", "kind", "call_hash", "disambiguator", "result", "created_at"],
        "additionalProperties": false,
    })
}

fn field_detail_schema() -> Value {
    json!({
        "type": "object",
        "description": "One declared field, in the vocabulary its author wrote it in.",
        "properties": {
            "name": { "type": "string" },
            "kind": {
                "type": "string",
                "description": "The field constructor as declared, e.g. `uuid()` or \
                    `optional(str(max_length = 80))`.",
            },
            "optional": { "type": "boolean" },
            "indexed": {
                "type": "boolean",
                "description": "Whether the field becomes a store tag, and so whether a query \
                    can filter on it.",
            },
            "subject": {
                "type": ["string", "null"],
                "description": "The sibling field whose value scopes this field's encryption key.",
            },
            "unique": { "type": "boolean" },
        },
        "required": ["name", "kind", "optional", "indexed", "subject", "unique"],
        "additionalProperties": false,
    })
}

fn event_detail_schema() -> Value {
    json!({
        "type": "object",
        "description": "A declared event type and its fields. Unlike the `event.*` schemas, \
            which describe payload shape for a reader, this is the declaration itself.",
        "properties": {
            "type": { "type": "string" },
            "fields": { "type": "array", "items": schema_ref("FieldDetail") },
        },
        "required": ["type", "fields"],
        "additionalProperties": false,
    })
}

fn entity_detail_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "key": { "type": "string" },
            "key_kind": { "type": "string" },
            "fields": { "type": "array", "items": schema_ref("FieldDetail") },
            "indexes": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "columns": { "type": "array", "items": { "type": "string" } },
                    },
                    "required": ["name", "columns"],
                    "additionalProperties": false,
                },
            },
            "filterable": {
                "type": "array",
                "items": { "type": "string" },
                "description": "The fields `GET /read/...` accepts as a filter: the key plus \
                    each index's leftmost column.",
            },
            "rows": {
                "type": ["integer", "null"],
                "minimum": 0,
                "format": "int64",
                "description": "Null unless the request asked for counts.",
            },
        },
        "required": ["name", "key", "key_kind", "fields", "indexes", "filterable", "rows"],
        "additionalProperties": false,
    })
}

fn projector_detail_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "position": position_schema("The projector's checkpoint."),
            "lag": position_schema("Log head minus position."),
            "readiness": {
                "type": "string",
                "enum": ["ready", "rebuilding", "stale", "rebuild_failed", "quarantined"],
            },
            "running": { "type": "boolean" },
            "failed": { "type": "boolean" },
            "replays_completed": { "type": "integer", "minimum": 0 },
            "replays_failed": { "type": "integer", "minimum": 0 },
            "last_error": { "type": ["string", "null"] },
            "sources": sources_schema(),
            "definition_hash": {
                "type": ["string", "null"],
                "description": "The projector's digest entry hash at the time the stored rows \
                    were built, read from the read model itself. It covers the handler bodies as \
                    well as the subscription and the entity shapes, so a corrected handler \
                    rebuilds; it ignores layout, so a reformat does not. Null on the list \
                    endpoint, which opens no database.",
            },
            "entities": { "type": "array", "items": schema_ref("EntityDetail") },
        },
        "required": [
            "name", "position", "lag", "readiness", "running", "failed",
            "replays_completed", "replays_failed", "last_error", "sources",
            "definition_hash", "entities"
        ],
        "additionalProperties": false,
    })
}

fn declaration_summary_schema() -> Value {
    json!({
        "type": "object",
        "description": "One declaration as recorded at boot, hashed by what it does rather than \
            by how it was written, so a reformat leaves every hash where it was. The only place a \
            projector's or effect's hash survives: the units carrying it move into their threads.",
        "properties": {
            "kind": { "type": "string", "enum": DECLARATION_KINDS },
            "name": {
                "type": "string",
                "description": "As heklang names it, so an event keeps its `@` sigil.",
            },
            "hash": {
                "type": "string",
                "description": "What this declaration does. Two builds differing here behave \
                    differently.",
            },
            "signature_hash": {
                "type": ["string", "null"],
                "description": "What of it is visible from outside: an event's fields, a \
                    command's parameters and refusal codes, a projector's columns, an effect's \
                    subscription. Null for a `fn`, which nothing outside can name.",
            },
            "module": {
                "type": ["string", "null"],
                "description": "The file it was declared in. Outside the hash on purpose, so \
                    moving a declaration between files does not read as a change. Null for an \
                    event and an enum, whose declarations carry no module.",
            },
            "first_seen": { "type": "string" },
            "last_seen": { "type": "string" },
        },
        "required": ["kind", "name", "hash", "signature_hash", "module", "first_seen", "last_seen"],
        "additionalProperties": false,
    })
}

fn system_info_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "version": { "type": "string" },
            "uptime_seconds": { "type": "integer", "minimum": 0, "format": "int64" },
            "log_head": position_schema("The log's head position, which is also its event count."),
            "data_dir": { "type": "string" },
            "opdb_schema_version": { "type": "integer", "minimum": 0 },
            "verify": {
                "type": "boolean",
                "description": "Whether the continuous invariant checks are running.",
            },
            "keystore": {
                "type": "object",
                "properties": {
                    "configured": { "type": "boolean" },
                    "master_key_ids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Which masters stored key material is wrapped under. More \
                            than one means a rotation has begun and not finished.",
                    },
                },
                "required": ["configured", "master_key_ids"],
                "additionalProperties": false,
            },
            "config": {
                "type": "object",
                "description": "The effective configuration, after `hekla.toml` and any flag \
                    that overrides it.",
                "properties": {
                    "effects": {
                        "type": "object",
                        "properties": { "pool_size": { "type": "integer", "minimum": 1 } },
                        "required": ["pool_size"],
                        "additionalProperties": false,
                    },
                    "retention": {
                        "type": "object",
                        "properties": { "effect_journal_days": { "type": "integer", "minimum": 0 } },
                        "required": ["effect_journal_days"],
                        "additionalProperties": false,
                    },
                    "projectors": {
                        "type": "object",
                        "properties": { "auto_rebuild": { "type": "boolean" } },
                        "required": ["auto_rebuild"],
                        "additionalProperties": false,
                    },
                    "verify": {
                        "type": "object",
                        "properties": { "enabled": { "type": "boolean" } },
                        "required": ["enabled"],
                        "additionalProperties": false,
                    },
                },
                "required": ["effects", "retention", "projectors", "verify"],
                "additionalProperties": false,
            },
        },
        "required": [
            "version", "uptime_seconds", "log_head", "data_dir",
            "opdb_schema_version", "verify", "keystore", "config"
        ],
        "additionalProperties": false,
    })
}

fn subject_entry_schema() -> Value {
    json!({
        "type": "object",
        "description": "One live subject key, without any key material.",
        "properties": {
            "subject_field": { "type": "string" },
            "subject_value": { "type": "string" },
            "master_key_id": { "type": "string" },
            "created_at": { "type": "string" },
        },
        "required": ["subject_field", "subject_value", "master_key_id", "created_at"],
        "additionalProperties": false,
    })
}

// ---------------------------------------------------------------------------
// Small builders
// ---------------------------------------------------------------------------

fn schema_ref(name: &str) -> Value {
    json!({ "$ref": format!("#/components/schemas/{name}") })
}

fn response(description: &str, schema: Value) -> Value {
    json!({
        "description": description,
        "content": { "application/json": { "schema": schema } },
    })
}

fn path_param(name: &str, description: &str, schema: Value) -> Value {
    json!({
        "name": name,
        "in": "path",
        "required": true,
        "description": description,
        "schema": schema,
    })
}

fn query_param(name: &str, description: &str, schema: Value) -> Value {
    json!({
        "name": name,
        "in": "query",
        "required": false,
        "description": description,
        "schema": schema,
    })
}

fn header_param(name: &str, description: &str) -> Value {
    json!({
        "name": name,
        "in": "header",
        "required": false,
        "description": description,
        "schema": { "type": "string" },
    })
}

fn position_schema(description: &str) -> Value {
    json!({
        "type": "integer",
        "minimum": 0,
        "format": "int64",
        "description": description,
    })
}

/// A log position that may be absent, for a cursor that has run out.
fn position_nullable(description: &str) -> Value {
    json!({
        "type": ["integer", "null"],
        "minimum": 0,
        "format": "int64",
        "description": description,
    })
}

/// A string constrained to a known set of names. An empty set means the project
/// declares none, and an empty `enum` would match nothing at all, so the constraint
/// is dropped rather than made unsatisfiable.
fn name_schema(names: &[&str]) -> Value {
    if names.is_empty() {
        json!({ "type": "string" })
    } else {
        json!({ "type": "string", "enum": names })
    }
}

/// A component key: OpenAPI restricts these to `[A-Za-z0-9._-]`, and an event type
/// or module name is author-supplied.
fn component_name(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// An `operationId` fragment: unique per operation and safe for a code generator to
/// use as a function name.
fn identifier(raw: &str) -> String {
    raw.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn operation_id(prefix: &str, projector: &str, entity: &str) -> String {
    format!("{prefix}_{}_{}", identifier(projector), identifier(entity))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::schema::IndexDef;

    fn schema() -> InputSchema {
        InputSchema {
            fields: vec![
                ("id".to_owned(), FieldKind::Uuid),
                ("amount".to_owned(), FieldKind::Money { scale: 2 }),
                (
                    "kind".to_owned(),
                    FieldKind::OneOf(vec!["a".to_owned(), "b".to_owned()]),
                ),
                (
                    "note".to_owned(),
                    FieldKind::Optional(Box::new(FieldKind::Text {
                        max_length: Some(10),
                    })),
                ),
            ],
        }
    }

    fn subject_meta(kind: FieldKind, subject: &str) -> FieldMeta {
        FieldMeta {
            kind,
            indexed: true,
            subject: Some(subject.to_owned()),
        }
    }

    /// A projector whose entity exercises every branch the generator has: a key, an
    /// indexed column, an unindexed one, a subject-encrypted one, and an optional.
    fn entity() -> EntityDef {
        EntityDef {
            name: "user_summary".to_owned(),
            key: "user_id".to_owned(),
            fields: vec![
                ("user_id".to_owned(), FieldMeta::plain(FieldKind::Uuid)),
                ("shop_id".to_owned(), FieldMeta::plain(FieldKind::I64)),
                (
                    "display_name".to_owned(),
                    FieldMeta::plain(FieldKind::Text { max_length: None }),
                ),
                (
                    "email".to_owned(),
                    subject_meta(FieldKind::Text { max_length: None }, "user_id"),
                ),
                (
                    "note".to_owned(),
                    FieldMeta::plain(FieldKind::Optional(Box::new(FieldKind::Text {
                        max_length: None,
                    }))),
                ),
            ],
            indexes: vec![IndexDef {
                name: "by_shop".to_owned(),
                columns: vec!["shop_id".to_owned(), "user_id".to_owned()],
            }],
        }
    }

    fn event_def() -> EventDef {
        EventDef {
            event_type: "user.registered".to_owned(),
            fields: vec![
                ("user_id".to_owned(), FieldMeta::plain(FieldKind::Uuid)),
                (
                    "email".to_owned(),
                    subject_meta(FieldKind::Text { max_length: None }, "user_id"),
                ),
            ],
        }
    }

    /// A surface covering all four module kinds, so one document exercises every
    /// generator branch.
    fn surface<'a>(
        input: &'a InputSchema,
        entities: &'a [EntityDef],
        events: &'a [(String, EventDef)],
    ) -> Surface<'a> {
        Surface {
            commands: vec![("do-thing", input)],
            projectors: vec![ProjectorSurface {
                name: "users",
                entities,
                sources: vec!["user.registered"],
            }],
            effects: vec![EffectSurface {
                name: "notify",
                sources: Vec::new(),
            }],
            events: events
                .iter()
                .map(|(event_type, def)| (event_type.as_str(), def))
                .collect(),
        }
    }

    fn full_doc() -> Value {
        let input = schema();
        let entities = vec![entity()];
        let events = vec![("user.registered".to_owned(), event_def())];
        build(&surface(&input, &entities, &events))
    }

    fn command_body_schema(doc: &Value) -> &Value {
        &doc["paths"]["/commands/do-thing"]["post"]["requestBody"]["content"]["application/json"]["schema"]
    }

    #[test]
    fn maps_field_kinds_and_marks_optionals() {
        let doc = full_doc();
        let props = &command_body_schema(&doc)["properties"];
        assert_eq!(props["amount"]["type"], "string");
        assert!(
            props["amount"].get("format").is_none(),
            "money is a bare string"
        );
        assert_eq!(props["id"]["format"], "uuid");
        assert_eq!(props["kind"]["enum"], json!(["a", "b"]));
        assert_eq!(props["note"]["maxLength"], json!(10));

        let required = command_body_schema(&doc)["required"]
            .as_array()
            .unwrap()
            .clone();
        assert!(required.contains(&json!("id")));
        assert!(
            !required.contains(&json!("note")),
            "optional is not required"
        );
    }

    #[test]
    fn a_body_is_required_only_when_a_field_is() {
        let doc = full_doc();
        assert_eq!(
            doc["paths"]["/commands/do-thing"]["post"]["requestBody"]["required"],
            json!(true)
        );

        let all_optional = InputSchema {
            fields: vec![(
                "note".to_owned(),
                FieldKind::Optional(Box::new(FieldKind::Text { max_length: None })),
            )],
        };
        let entities: Vec<EntityDef> = Vec::new();
        let events: Vec<(String, EventDef)> = Vec::new();
        let mut surface = surface(&all_optional, &entities, &events);
        surface.commands = vec![("ping", &all_optional)];
        let doc = build(&surface);
        assert_eq!(
            doc["paths"]["/commands/ping"]["post"]["requestBody"]["required"],
            json!(false),
            "an empty body parses as an empty object, so an all-optional command needs none"
        );
    }

    /// `field_schema` is the only place the wire form of `Money` is stated, so a field
    /// annotation that replaced its description rather than appending would leave every
    /// money column looking like any other string.
    ///
    /// The Starlark version also pinned `uint`'s `2^64-1` ceiling here, which was the
    /// only place it appeared. heklang has no unsigned type, so an integer column is an
    /// `Int` with nothing about its range worth saying.
    #[test]
    fn field_annotations_keep_the_kind_description() {
        let entity = EntityDef {
            name: "ledger".to_owned(),
            key: "entry_id".to_owned(),
            fields: vec![
                ("entry_id".to_owned(), FieldMeta::plain(FieldKind::Uuid)),
                (
                    "amount".to_owned(),
                    FieldMeta::plain(FieldKind::Money { scale: 2 }),
                ),
                ("count".to_owned(), FieldMeta::plain(FieldKind::I64)),
            ],
            indexes: Vec::new(),
        };
        let input = schema();
        let entities = vec![entity];
        let events: Vec<(String, EventDef)> = Vec::new();
        let mut surface = surface(&input, &entities, &events);
        surface.projectors = vec![ProjectorSurface {
            name: "books",
            entities: &entities,
            sources: Vec::new(),
        }];
        let doc = build(&surface);
        let props = &doc["components"]["schemas"]["entity.books.ledger"]["properties"];

        let amount = props["amount"]["description"].as_str().unwrap();
        assert!(
            amount.starts_with("decimal amount"),
            "`money` lost its wire form: {amount}"
        );
        assert!(
            amount.contains("Not filterable"),
            "the annotation is still there too: {amount}"
        );
        // A plain integer has no wire form to state, so its description is the
        // annotation alone: appending to nothing must still produce it.
        let count = props["count"]["description"].as_str().unwrap();
        assert!(count.contains("Not filterable"), "{count}");
    }

    /// An optional command input may be omitted *or* sent as an explicit null, so
    /// `required` alone under-describes it and a validating client would refuse a body
    /// the server accepts.
    #[test]
    fn an_optional_command_input_admits_an_explicit_null() {
        let input = InputSchema {
            fields: vec![
                (
                    "note".to_owned(),
                    FieldKind::Optional(Box::new(FieldKind::Text { max_length: None })),
                ),
                (
                    "kind".to_owned(),
                    FieldKind::Optional(Box::new(FieldKind::OneOf(vec![
                        "a".to_owned(),
                        "b".to_owned(),
                    ]))),
                ),
                ("id".to_owned(), FieldKind::Uuid),
            ],
        };
        let entities: Vec<EntityDef> = Vec::new();
        let events: Vec<(String, EventDef)> = Vec::new();
        let mut surface = surface(&input, &entities, &events);
        surface.commands = vec![("do-thing", &input)];
        let doc = build(&surface);
        let props = &command_body_schema(&doc)["properties"];

        assert_eq!(props["note"]["type"], json!(["string", "null"]));
        assert_eq!(
            props["kind"]["enum"],
            json!(["a", "b", null]),
            "widening `type` alone leaves the enum rejecting the null it now permits"
        );
        assert_eq!(
            props["id"]["type"], "string",
            "a required field is not widened"
        );
    }

    #[test]
    fn documents_every_status_the_command_route_returns() {
        let doc = full_doc();
        let responses = &doc["paths"]["/commands/do-thing"]["post"]["responses"];
        for status in ["200", "400", "409", "422", "500", "503"] {
            assert!(
                responses.get(status).is_some(),
                "the spec omits the {status} response"
            );
        }
    }

    /// A response that only carries a `description` is what this whole change exists
    /// to remove, so no operation may ship one.
    #[test]
    fn every_response_carries_a_content_schema() {
        let doc = full_doc();
        for (path, item) in doc["paths"].as_object().unwrap() {
            for (method, operation) in item.as_object().unwrap() {
                for (status, response) in operation["responses"].as_object().unwrap() {
                    let content = response
                        .get("content")
                        .unwrap_or_else(|| panic!("{method} {path} {status} has no content"));
                    let media = content.as_object().unwrap().values().next().unwrap();
                    assert!(
                        media.get("schema").is_some(),
                        "{method} {path} {status} has content but no schema"
                    );
                }
            }
        }
    }

    /// An operation tagged with a name the top-level `tags` array does not declare
    /// renders as an unnamed section, which is the failure this grouping work is
    /// meant to fix.
    #[test]
    fn every_operation_is_tagged_and_every_tag_is_declared() {
        let doc = full_doc();
        let declared: Vec<&str> = doc["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tag| tag["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            declared,
            vec!["commands", "read: users", "operations", "introspection"],
            "tags render in declaration order, so the order is part of the contract"
        );

        for (path, item) in doc["paths"].as_object().unwrap() {
            for (method, operation) in item.as_object().unwrap() {
                let tags = operation["tags"]
                    .as_array()
                    .unwrap_or_else(|| panic!("{method} {path} has no tags"));
                assert!(!tags.is_empty(), "{method} {path} has an empty tag list");
                for tag in tags {
                    let tag = tag.as_str().unwrap();
                    assert!(
                        declared.contains(&tag),
                        "{method} {path} uses undeclared tag `{tag}`"
                    );
                }
            }
        }
    }

    fn collect_refs(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    if key == "$ref" {
                        out.push(child.as_str().unwrap().to_owned());
                    } else {
                        collect_refs(child, out);
                    }
                }
            }
            Value::Array(items) => {
                for item in items {
                    collect_refs(item, out);
                }
            }
            _ => {}
        }
    }

    /// The structural guard: a renamed or dropped schema leaves a dangling `$ref`,
    /// which renders as an empty box rather than an error.
    #[test]
    fn every_ref_resolves() {
        let doc = full_doc();
        let mut refs = Vec::new();
        collect_refs(&doc, &mut refs);
        assert!(!refs.is_empty(), "a document with no refs proves nothing");
        let schemas = doc["components"]["schemas"].as_object().unwrap();
        for reference in refs {
            let name = reference
                .strip_prefix("#/components/schemas/")
                .unwrap_or_else(|| panic!("unexpected ref target `{reference}`"));
            assert!(
                schemas.contains_key(name),
                "dangling ref `{reference}`; schemas are {:?}",
                schemas.keys().collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn a_read_path_is_generated_per_entity_with_a_typed_key() {
        let doc = full_doc();
        let one = &doc["paths"]["/read/users/user_summary/{key}"]["get"];
        let key = one["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .find(|param| param["name"] == "key")
            .expect("the by-key read has a key param");
        assert_eq!(key["in"], "path");
        assert_eq!(key["required"], json!(true));
        assert_eq!(
            key["schema"]["format"], "uuid",
            "the key param is typed from the key column, not left a bare string"
        );
        assert_eq!(one["tags"], json!(["read: users"]));

        let page = &doc["paths"]["/read/users/user_summary"]["get"]["responses"]["200"]["content"]
            ["application/json"]["schema"];
        assert_eq!(
            page["properties"]["items"]["items"]["$ref"],
            "#/components/schemas/entity.users.user_summary"
        );
    }

    /// Both read handlers can return 500 (`read_failed`, `task_panicked`), and both
    /// 503s carry a `Retry-After` for the two codes that resolve on their own. A client
    /// generated from a document missing either has no branch for it.
    #[test]
    fn the_read_paths_document_their_500_and_their_retry_after() {
        let doc = full_doc();
        for path in ["/read/users/user_summary", "/read/users/user_summary/{key}"] {
            let responses = &doc["paths"][path]["get"]["responses"];
            assert!(
                responses.get("500").is_some(),
                "{path} omits the 500 `read_failed` returns"
            );
            assert_eq!(
                responses["503"]["headers"]["Retry-After"]["schema"]["type"], "integer",
                "{path} does not declare the Retry-After it sends"
            );
        }
    }

    /// `limit` and `timeout_ms` are clamped by the handler, not rejected, so declaring
    /// a `maximum` would make a validating client refuse a request the server serves.
    #[test]
    fn clamped_params_declare_no_maximum() {
        let doc = full_doc();
        let params = doc["paths"]["/read/users/user_summary"]["get"]["parameters"]
            .as_array()
            .unwrap()
            .clone();
        for name in ["limit", "timeout_ms"] {
            let param = params
                .iter()
                .find(|param| param["name"] == name)
                .unwrap_or_else(|| panic!("no `{name}` param"));
            assert!(
                param["schema"].get("maximum").is_none(),
                "`{name}` is clamped, not rejected, so a `maximum` would be a lie"
            );
            assert!(
                param["description"]
                    .as_str()
                    .unwrap()
                    .contains("rather than rejected")
                    || param["description"].as_str().unwrap().contains("clamped"),
                "`{name}` must say what actually happens out of range"
            );
        }
    }

    #[test]
    fn only_filterable_fields_become_query_params() {
        let doc = full_doc();
        let params: Vec<&str> = doc["paths"]["/read/users/user_summary"]["get"]["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .map(|param| param["name"].as_str().unwrap())
            .collect();
        for control in ["limit", "cursor", "after", "timeout_ms"] {
            assert!(params.contains(&control), "missing control param {control}");
        }
        assert!(params.contains(&"user_id"), "the key is filterable");
        assert!(
            params.contains(&"shop_id"),
            "an index's leftmost column is filterable"
        );
        assert!(
            !params.contains(&"display_name"),
            "an unindexed field must not be documented as filterable"
        );
        assert!(
            !params.contains(&"note"),
            "a non-leftmost / unindexed column must not be documented as filterable"
        );
    }

    /// A subject column is removed from a row when its key was erased, so declaring
    /// it required would describe a body the server does not always send.
    #[test]
    fn an_entity_does_not_require_a_subject_column() {
        let doc = full_doc();
        let entity = &doc["components"]["schemas"]["entity.users.user_summary"];
        let required = entity["required"].as_array().unwrap();
        assert!(required.contains(&json!("user_id")));
        assert!(required.contains(&json!("display_name")));
        assert!(
            !required.contains(&json!("email")),
            "an erased subject column comes back absent, so it cannot be required"
        );
        assert!(
            !required.contains(&json!("note")),
            "an optional column is not required"
        );
        // One extension name, one meaning: the schema-level `x-hekla-key` names the key
        // column, and the property-level flag has its own name rather than reusing this
        // one with a different type.
        assert_eq!(entity["x-hekla-key"], "user_id");
        assert_eq!(entity["properties"]["user_id"]["x-hekla-is-key"], true);
        assert!(entity["properties"]["user_id"].get("x-hekla-key").is_none());
        assert_eq!(
            entity["properties"]["email"]["x-hekla-subject"], "user_id",
            "the subject is machine-readable, not only prose"
        );
    }

    #[test]
    fn events_are_documented_and_reachable_from_the_command_response() {
        let doc = full_doc();
        let schemas = doc["components"]["schemas"].as_object().unwrap();
        assert!(
            schemas.contains_key("event.user.registered"),
            "each declared event gets a schema"
        );
        assert_eq!(
            doc["components"]["schemas"]["EmittedEvent"]["properties"]["type"]["enum"],
            json!(["user.registered"]),
            "the declared event set is the enum a command response reports"
        );
        assert_eq!(
            schemas["event.user.registered"]["required"],
            json!(["user_id", "email"]),
            "an event's own schema does require its subject field: it is in the log"
        );
    }

    #[test]
    fn operator_paths_enumerate_the_declared_module_names() {
        let doc = full_doc();
        let replay = &doc["paths"]["/projectors/{name}/replay"]["post"]["parameters"][0];
        assert_eq!(replay["schema"]["enum"], json!(["users"]));
        let skip = &doc["paths"]["/effects/{name}/skip/{position}"]["post"]["parameters"][0];
        assert_eq!(skip["schema"]["enum"], json!(["notify"]));
    }

    /// An empty `enum` matches nothing, so a project with no effects must not
    /// document a path no request could ever satisfy.
    #[test]
    fn an_empty_module_set_drops_the_enum_rather_than_emitting_an_empty_one() {
        let input = schema();
        let entities: Vec<EntityDef> = Vec::new();
        let events: Vec<(String, EventDef)> = Vec::new();
        let mut surface = surface(&input, &entities, &events);
        surface.effects = Vec::new();
        surface.projectors = Vec::new();
        let doc = build(&surface);
        let skip = &doc["paths"]["/effects/{name}/skip/{position}"]["post"]["parameters"][0];
        assert_eq!(skip["schema"], json!({ "type": "string" }));
        assert!(
            doc["paths"]["/projectors/{name}/replay"].is_object(),
            "the route exists whether or not the project declares a projector"
        );
    }

    /// The CLI dump exists to be committed and diffed, which needs the document to be
    /// a pure function of the project.
    #[test]
    fn every_introspection_route_is_described_and_grouped() {
        let doc = full_doc();
        let paths = doc["paths"].as_object().unwrap();
        let admin: Vec<&String> = paths.keys().filter(|p| p.starts_with("/admin")).collect();
        // Counted from the router rather than written down. A literal here is a second
        // hand-maintained copy of the route table, which is the exact failure mode the
        // table's own doc comment argues against: it catches a typo but not the thing
        // that matters, someone adding a route and updating only the number.
        let registered = server::routes()
            .iter()
            .filter(|route| route.starts_with(server::ADMIN_ROUTE))
            .count();
        assert_eq!(
            admin.len(),
            registered,
            "every /admin route the router registers needs a path: {admin:?}"
        );
        for path in admin {
            let tags = paths[path]["get"]["tags"].as_array().unwrap();
            assert_eq!(tags, &vec![json!("introspection")], "{path} is ungrouped");
        }
    }

    #[test]
    fn the_introspection_module_params_enumerate_the_declared_names() {
        let doc = full_doc();
        let effect_param = |path: &str| -> Value {
            doc["paths"][path]["get"]["parameters"]
                .as_array()
                .unwrap()
                .iter()
                .find(|param| param["name"] == json!("name"))
                .unwrap()["schema"]
                .clone()
        };
        // The fixture declares one effect and one projector, so a client generator
        // gets a closed set rather than a bare string.
        assert_eq!(
            effect_param("/admin/effects/{name}")["enum"],
            json!(["notify"])
        );
        assert_eq!(
            effect_param("/admin/effects/{name}/invocations")["enum"],
            json!(["notify"])
        );
        assert_eq!(
            effect_param("/admin/projectors/{name}")["enum"],
            json!(["users"])
        );
    }

    #[test]
    fn a_decrypting_endpoint_documents_the_opt_out_and_a_paged_one_documents_the_clamp() {
        let doc = full_doc();
        let params = |path: &str| -> Vec<(String, Value)> {
            doc["paths"][path]["get"]["parameters"]
                .as_array()
                .unwrap()
                .iter()
                .map(|param| (param["name"].as_str().unwrap().to_owned(), param.clone()))
                .collect()
        };
        for path in [
            "/admin/events",
            "/admin/events/{position}",
            "/admin/traces/{correlation_id}",
        ] {
            let found = params(path);
            let (_, decrypt) = found.iter().find(|(name, _)| name == "decrypt").unwrap();
            assert_eq!(decrypt["schema"]["default"], json!(true));
        }
        // The handler clamps, so declaring a maximum would make a validating client
        // refuse locally what the server would happily answer.
        let (_, limit) = params("/admin/events")
            .into_iter()
            .find(|(name, _)| name == "limit")
            .unwrap();
        assert!(limit["schema"].get("maximum").is_none());
        assert!(
            limit["description"].as_str().unwrap().contains("clamped"),
            "the clamp is the behaviour a caller has to know about"
        );
    }

    /// Walks every object schema in the document and checks that each name in
    /// `required` is also in `properties`.
    ///
    /// A schema requiring a property it does not declare, under
    /// `additionalProperties: false`, is unsatisfiable: one clause demands the key and
    /// the other forbids it, so every real response fails validation. `oas3` parses
    /// such a document happily, which is exactly why this is checked here.
    fn check_required_are_declared(value: &Value, path: &str, problems: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                if let (Some(Value::Array(required)), Some(Value::Object(properties))) =
                    (map.get("required"), map.get("properties"))
                {
                    for name in required.iter().filter_map(Value::as_str) {
                        if !properties.contains_key(name) {
                            problems.push(format!("{path}: required `{name}` is not a property"));
                        }
                    }
                }
                for (key, child) in map {
                    check_required_are_declared(child, &format!("{path}/{key}"), problems);
                }
            }
            Value::Array(items) => {
                for (index, child) in items.iter().enumerate() {
                    check_required_are_declared(child, &format!("{path}/{index}"), problems);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn every_required_property_is_a_declared_property() {
        let mut problems = Vec::new();
        check_required_are_declared(&full_doc(), "", &mut problems);
        assert!(problems.is_empty(), "unsatisfiable schemas: {problems:#?}");
    }

    #[test]
    fn the_subject_state_enum_covers_every_state_the_renderer_emits() {
        let doc = full_doc();
        let declared: Vec<&str> =
            doc["components"]["schemas"]["SubjectState"]["properties"]["state"]["enum"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| value.as_str().unwrap())
                .collect();
        // The handler picks one of these five by name, and a strict generated client
        // rejects a response carrying one the document never declared.
        assert_eq!(
            declared,
            vec!["decrypted", "encrypted", "erased", "stale", "unreadable"]
        );
    }

    #[test]
    fn the_declaration_kind_enum_is_spelled_the_way_heklang_spells_it() {
        use heklang::Kind;
        // Tied to `Kind::name` rather than to string literals, so a rename upstream
        // fails here instead of silently making the document describe kinds no row can
        // hold. `Test` is the one kind deliberately left out: `Digest::entries` holds
        // tests back, so a declaration row can never carry it.
        let expected: Vec<&str> = [
            Kind::Event,
            Kind::Enum,
            Kind::Record,
            Kind::Function,
            Kind::Command,
            Kind::Projector,
            Kind::Effect,
        ]
        .iter()
        .map(|kind| kind.name())
        .collect();
        assert_eq!(DECLARATION_KINDS.to_vec(), expected);
        assert!(!DECLARATION_KINDS.contains(&Kind::Test.name()));

        let doc = full_doc();
        let declared: Vec<&str> = doc["components"]["schemas"]["DeclarationSummary"]["properties"]
            ["kind"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert_eq!(declared, expected);
    }

    #[test]
    fn both_effect_schemas_declare_the_same_state_vocabulary() {
        let doc = full_doc();
        let enum_of = |schema: &str| -> Vec<String> {
            doc["components"]["schemas"][schema]["properties"]["state"]["enum"]
                .as_array()
                .unwrap_or_else(|| panic!("{schema} declares a state enum"))
                .iter()
                .map(|value| value.as_str().unwrap().to_owned())
                .collect()
        };
        let detail = enum_of("EffectDetail");
        assert_eq!(detail, ["healthy", "lagging", "quarantined", "wedged"]);
        // `/status` and `/admin/effects` read one function on the shared handle, so a
        // client that learned the vocabulary from either must not meet a word from the
        // other. Both schemas carry `additionalProperties: false`, which makes a
        // one-sided edit a document that describes a response the server violates.
        assert_eq!(
            enum_of("EffectStatus"),
            detail,
            "the two schemas describe the same value from the same function"
        );
        for schema in ["EffectDetail", "EffectStatus"] {
            let required = doc["components"]["schemas"][schema]["required"]
                .as_array()
                .unwrap();
            assert!(
                required.iter().any(|name| name == "state"),
                "{schema} always carries a state, so it is required"
            );
        }
    }

    #[test]
    fn the_event_type_filter_is_not_closed_to_the_declared_set() {
        let doc = full_doc();
        let param = doc["paths"]["/admin/events"]["get"]["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .find(|param| param["name"] == json!("type"))
            .unwrap();
        // The log outlives any one deployment, and `LogEvent.declared` exists to report
        // exactly the types this project no longer knows. An enum of the declared set
        // would make those unaddressable from a generated client.
        assert!(
            param["schema"]["items"].get("enum").is_none(),
            "a historical event type has to stay filterable"
        );
        assert!(
            param["description"]
                .as_str()
                .unwrap()
                .contains("no longer declares")
        );
    }

    #[test]
    fn the_document_is_deterministic() {
        assert_eq!(full_doc().to_string(), full_doc().to_string());
    }

    fn operation_ids(doc: &Value) -> Vec<&str> {
        let mut ids = Vec::new();
        for item in doc["paths"].as_object().unwrap().values() {
            for operation in item.as_object().unwrap().values() {
                if let Some(id) = operation["operationId"].as_str() {
                    ids.push(id);
                }
            }
        }
        ids
    }

    /// A duplicate `operationId` is a spec violation, and a client generator resolves
    /// it by dropping an operation or by overwriting a function. `identifier` folds
    /// `-` to `_`, so two projectors differing only there would otherwise collide.
    #[test]
    fn operation_ids_are_unique_even_when_names_fold_together() {
        let input = schema();
        let entities = vec![entity()];
        let events: Vec<(String, EventDef)> = Vec::new();
        let mut surface = surface(&input, &entities, &events);
        surface.projectors = vec![
            ProjectorSurface {
                name: "read-side",
                entities: &entities,
                sources: Vec::new(),
            },
            ProjectorSurface {
                name: "read_side",
                entities: &entities,
                sources: Vec::new(),
            },
        ];
        let doc = build(&surface);

        let ids = operation_ids(&doc);
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "duplicate operationId in {ids:?}");
        assert!(
            ids.iter().any(|id| id.ends_with("_2")),
            "the two projectors really did fold together, so one was renumbered: {ids:?}"
        );

        // Component names are a separate namespace with a wider charset, so they stay
        // distinct without renumbering.
        let schemas = doc["components"]["schemas"].as_object().unwrap();
        assert!(schemas.contains_key("entity.read-side.user_summary"));
        assert!(schemas.contains_key("entity.read_side.user_summary"));
    }

    /// An event type is an unvalidated author string, and `component_name` folds every
    /// character outside `[A-Za-z0-9._-]` to `_`. Two types that fold together must not
    /// collapse into one schema: the second insert would win, leaving one schema
    /// describing the wrong event's fields while `EmittedEvent.type` still listed both.
    #[test]
    fn colliding_event_types_get_separate_schemas() {
        let input = schema();
        let entities: Vec<EntityDef> = Vec::new();
        let spaced = EventDef {
            event_type: "order placed".to_owned(),
            fields: vec![("shop".to_owned(), FieldMeta::plain(FieldKind::I64))],
        };
        let underscored = EventDef {
            event_type: "order_placed".to_owned(),
            fields: vec![("customer".to_owned(), FieldMeta::plain(FieldKind::Uuid))],
        };
        let events = vec![
            ("order placed".to_owned(), spaced),
            ("order_placed".to_owned(), underscored),
        ];
        let doc = build(&surface(&input, &entities, &events));

        let schemas = doc["components"]["schemas"].as_object().unwrap();
        let event_keys: Vec<&String> = schemas
            .keys()
            .filter(|key| key.starts_with("event."))
            .collect();
        assert_eq!(
            event_keys.len(),
            2,
            "two declared events, two schemas: {event_keys:?}"
        );
        assert_eq!(
            doc["components"]["schemas"]["EmittedEvent"]["properties"]["type"]["enum"]
                .as_array()
                .unwrap()
                .len(),
            2,
        );
        // The two schemas describe different events, so one did not overwrite the other.
        let fields: Vec<Vec<&str>> = event_keys
            .iter()
            .map(|key| {
                schemas[*key]["properties"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect()
            })
            .collect();
        assert!(
            fields.contains(&vec!["shop"]) && fields.contains(&vec!["customer"]),
            "one schema replaced the other: {fields:?}"
        );
    }

    /// The ordinary case must not be renumbered, or the guard above would pass on a
    /// generator that suffixed everything.
    #[test]
    fn unique_names_keep_their_bare_operation_ids() {
        let doc = full_doc();
        let ids = operation_ids(&doc);
        assert!(
            ids.contains(&"execute_do_thing"),
            "a command keeps its plain id: {ids:?}"
        );
        assert!(
            ids.contains(&"scan_users_user_summary"),
            "a read keeps its plain id: {ids:?}"
        );
        assert!(
            !ids.iter().any(|id| id.ends_with("_2")),
            "nothing collides here, so nothing may be renumbered: {ids:?}"
        );
    }

    /// A structurally malformed document renders as a blank page in a reference UI
    /// rather than an error, so it is parsed against a real OpenAPI 3.1 model here.
    ///
    /// It checks structure, not semantics: it catches a misplaced parameter, a
    /// response that is not a response object, and a missing required top-level
    /// field, but it does not check that a `$ref` resolves (that is
    /// `every_ref_resolves`) and it accepts any `openapi` version string. The
    /// control below is what makes "it parsed" mean something.
    #[test]
    fn the_document_parses_as_openapi_3_1() {
        let doc = full_doc();
        let spec: oas3::Spec =
            serde_json::from_value(doc).expect("the generated document is valid OpenAPI 3.1");
        assert!(
            spec.paths.is_some_and(|paths| !paths.is_empty()),
            "the parsed document has paths"
        );

        let malformed = json!({
            "openapi": "3.1.0",
            "info": { "title": "t", "version": "1" },
            "paths": { "/x": { "get": {
                "parameters": [{ "name": "a", "in": "nowhere" }],
                "responses": {},
            }}},
        });
        assert!(
            serde_json::from_value::<oas3::Spec>(malformed).is_err(),
            "the parse would accept anything, so passing it proves nothing"
        );
    }
}
