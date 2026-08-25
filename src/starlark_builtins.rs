//! Starlark builtins for the runtime.
//!
//! Verified against starlark 0.14.2.
//!
//! Module layout: each `.star` file is one command, projector or effect,
//! identified by its filename (slug-validated). Handlers (`query`, `fold`,
//! `handle`) and schema globals (`input`, `initial`, entities) are named top-level
//! values; there are no registration calls. An event-driven handler is a dict keyed
//! by query clauses (see
//! [`EventDispatch`]), and a command's `fold` returns the new state rather than
//! mutating the one it was handed. Events are declared with `event(...)` in
//! `events/` and constructed by calling the definition (`user_registered(...)`),
//! which validates the payload and derives tags; a command's `handle` returns an
//! event, a list of events, or `reject(...)`.

use std::fmt;
use std::hash::Hash;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use allocative::Allocative;
use anyhow::Context;
use serde::Serializer;
use starlark::any::{AnyLifetime, ProvidesStaticType};
use starlark::collections::{SmallMap, StarlarkHasher};
use starlark::environment::{FrozenModule, Globals, GlobalsBuilder, Module};
use starlark::eval::{Arguments, Evaluator, FileLoader};
use starlark::syntax::{AstModule, Dialect};
use starlark::values::dict::{AllocDict, DictRef};
use starlark::values::function::FUNCTION_TYPE;
use starlark::values::list::{ListRef, UnpackList};
use starlark::values::none::NoneType;
use starlark::values::structs::AllocStruct;
use starlark::values::tuple::TupleRef;
use starlark::values::{
    Heap, NoSerialize, OwnedFrozenValue, StarlarkValue, Value, ValueLike, starlark_value,
};
use starlark::{starlark_module, starlark_simple_value};

use crate::context::{EffectCtx, EffectHost, HandleCtx, ProjectorCtx, QueryCtx};
use crate::dispatch::{EventDefs, RESERVED_TAG_PREFIX};
use crate::read_api::RESERVED_QUERY_PARAMS;
use crate::read_model::quote_ident;

// ---------------------------------------------------------------------------
// Field types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Allocative)]
pub enum FieldKind {
    Text {
        max_length: Option<u32>,
    },
    I64,
    U64,
    Bool,
    Uuid,
    Timestamp,
    /// Fixed-scale decimal. Do not use floats for money.
    Money,
    Json,
    OneOf(Vec<String>),
    Optional(Box<FieldKind>),
}

impl FieldKind {
    /// SQLite column type. The runtime generates DDL from this.
    pub fn sql_type(&self) -> &'static str {
        match self {
            FieldKind::Text { .. } | FieldKind::Uuid | FieldKind::OneOf(_) => "TEXT",
            FieldKind::I64 | FieldKind::U64 => "INTEGER",
            // Money is a decimal string on the wire; store it verbatim so a value like
            // "10.50" round-trips and reads back the same JSON type whether or not the
            // field is subject-encrypted.
            FieldKind::Money => "TEXT",
            FieldKind::Bool => "INTEGER",
            FieldKind::Timestamp => "TEXT", // ISO-8601, sorts lexicographically
            FieldKind::Json => "TEXT",
            FieldKind::Optional(inner) => inner.sql_type(),
        }
    }

    pub fn is_nullable(&self) -> bool {
        matches!(self, FieldKind::Optional(_))
    }

    /// Strip an `Optional(..)` wrapper to reach the underlying kind.
    pub fn base(&self) -> &FieldKind {
        match self {
            FieldKind::Optional(inner) => inner,
            other => other,
        }
    }
}

/// A declared field: its type plus the per-field policy that governs tagging and
/// subject-scoped encryption. `indexed` decides whether the field becomes a store
/// tag; `subject` names a sibling field whose per-subject key encrypts this field's
/// value (in the tag, the payload, and any read-model column); `unique` additionally
/// emits a global-key tag so a global uniqueness check survives erasure.
#[derive(Debug, Clone, PartialEq, Allocative)]
pub struct FieldMeta {
    pub kind: FieldKind,
    pub indexed: bool,
    pub subject: Option<String>,
    pub unique: bool,
}

impl FieldMeta {
    /// A plain field: indexed, no subject, not unique. The default for every field
    /// that opts into nothing.
    pub fn plain(kind: FieldKind) -> FieldMeta {
        FieldMeta {
            kind,
            indexed: true,
            subject: None,
            unique: false,
        }
    }

    pub fn is_nullable(&self) -> bool {
        self.kind.is_nullable()
    }

    /// The SQLite column type for this field in a read model. A subject-scoped
    /// field stores its opaque ciphertext, so its column is always `TEXT`
    /// regardless of the underlying kind; the read API decrypts and re-types it on
    /// the way out.
    pub fn sql_type(&self) -> &'static str {
        if self.subject.is_some() {
            "TEXT"
        } else {
            self.kind.sql_type()
        }
    }
}

#[derive(Debug, Clone, ProvidesStaticType, NoSerialize, Allocative)]
pub struct FieldType(pub FieldMeta);

impl fmt::Display for FieldType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

#[starlark_value(type = "field_type")]
impl<'v> StarlarkValue<'v> for FieldType {}
starlark_simple_value!(FieldType);

/// The standard `str`, `int` and `bool` are shadowed by kiln's field types, so the
/// conversion half of `int` has to come from somewhere. Keep a private copy of the
/// standard globals to call into: base prefixes and arbitrary-precision parsing live
/// behind `StarlarkInt`, which starlark does not export, and reimplementing them
/// would silently drop bignum support. `str` and `bool` need no such help, since
/// `Value::to_str` and `Value::to_bool` are public.
///
/// The `Globals` is `'static`, so the frozen function outlives every evaluator that
/// borrows it.
fn stdlib_int<'v>() -> Value<'v> {
    static STANDARD: OnceLock<Globals> = OnceLock::new();
    STANDARD
        .get_or_init(Globals::standard)
        .iter()
        .find(|(name, _)| *name == "int")
        .expect("starlark always defines int")
        .1
        .to_value()
}

/// Reject field options on a builtin that was called to convert a value. Passing
/// both means the caller confused the two halves of the overload, and silently
/// dropping the options would declare nothing while looking like it had.
fn no_field_options(name: &str, options: &[(&str, bool)]) -> anyhow::Result<()> {
    let given: Vec<&str> = options
        .iter()
        .filter(|(_, present)| *present)
        .map(|(option, _)| *option)
        .collect();
    if given.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "{name}() got a value to convert as well as the field option(s) {given:?}; pass a value to convert it, or only field options to declare a field"
    )
}

/// Assemble a [`FieldType`] from a base kind and the shared `indexed`/`subject`/
/// `unique` policy arguments, applying the kind-independent rules: `unique` requires
/// `subject` and indexing, and neither `subject` nor `unique` is meaningful on an
/// opaque `json` blob or an unbounded `text` (whose ciphertext tag must stay
/// bounded). Sibling-reference rules (the subject naming a real, non-encrypted
/// field) need the whole field set and are checked where fields are assembled.
fn field_type(
    kind: FieldKind,
    indexed: Option<bool>,
    subject: Option<String>,
    unique: Option<bool>,
) -> anyhow::Result<FieldType> {
    let indexed = indexed.unwrap_or(true);
    let unique = unique.unwrap_or(false);
    if unique && subject.is_none() {
        anyhow::bail!(
            "unique = True requires subject = \"...\"; a global uniqueness index is opt-in on a subject-scoped field"
        );
    }
    if unique && !indexed {
        anyhow::bail!("unique = True cannot be combined with indexed = False");
    }
    if subject.is_some() || unique {
        match kind.base() {
            FieldKind::Json => {
                anyhow::bail!("a json field cannot be subject-encrypted or unique")
            }
            FieldKind::Text { max_length: None } => anyhow::bail!(
                "a subject-encrypted or unique text field needs max_length so its ciphertext tag stays bounded"
            ),
            _ => {}
        }
    }
    Ok(FieldType(FieldMeta {
        kind,
        indexed,
        subject,
        unique,
    }))
}

/// Check every `subject = "sibling"` reference against the sibling set: the named
/// field must exist, must not itself be subject-encrypted (subjects do not chain),
/// and must be a scalar id rather than an opaque `json` blob. `context` names the
/// event or entity in the error. Shared by the `event()` and `entity()` builtins,
/// which both know their complete field set.
fn validate_subject_refs(context: &str, fields: &[(String, FieldMeta)]) -> anyhow::Result<()> {
    for (name, meta) in fields {
        let Some(subject) = &meta.subject else {
            continue;
        };
        match fields.iter().find(|(n, _)| n == subject) {
            None => anyhow::bail!(
                "{context}: field `{name}` is scoped to subject `{subject}`, which is not a declared field"
            ),
            Some((_, sm)) if sm.subject.is_some() => anyhow::bail!(
                "{context}: subject `{subject}` for field `{name}` is itself subject-encrypted (subjects cannot chain)"
            ),
            Some((_, sm)) if matches!(sm.kind.base(), FieldKind::Json) => anyhow::bail!(
                "{context}: subject `{subject}` for field `{name}` must be a scalar id, not json"
            ),
            // The key is derived from the subject id, so it must always be present; an
            // optional (null-able) id would leave the value un-keyable.
            Some((_, sm)) if sm.is_nullable() => anyhow::bail!(
                "{context}: subject `{subject}` for field `{name}` must not be optional (it keys the encryption)"
            ),
            Some(_) => {}
        }
    }
    Ok(())
}

/// Whether `name` is a plain SQL identifier: ascii letters, digits and underscores,
/// starting with a letter or underscore.
fn is_sql_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// How deep [`contains_cipher_handle`] descends before giving up. Real payloads
/// nest a handful of levels; the cap exists so a self-referential value cannot
/// recurse forever.
const MAX_HANDLE_SCAN_DEPTH: u32 = 64;

/// Whether `value` is, or transitively contains, a [`CipherHandle`]. The re-emit
/// guard has to see through lists, tuples and dicts: `to_json_value` flattens a
/// nested handle to its bare ciphertext, which would then be stored (or
/// re-encrypted) as if it were plaintext.
///
/// Past [`MAX_HANDLE_SCAN_DEPTH`] this reports `false` rather than descending
/// further. Starlark values can be cyclic (`x = []; x.append(x)`), and recursing
/// into one would overflow the stack and abort the process. Returning `false` hands
/// the value to `to_json_value`, which detects the cycle and fails the command with
/// a normal error. A handle buried deeper than the cap is therefore not caught here,
/// which is the same position the guard was in before it looked inside containers at
/// all.
fn contains_cipher_handle(value: Value<'_>) -> bool {
    fn scan(value: Value<'_>, depth: u32) -> bool {
        if value.downcast_ref::<CipherHandle>().is_some() {
            return true;
        }
        if depth == MAX_HANDLE_SCAN_DEPTH {
            return false;
        }
        if let Some(list) = ListRef::from_value(value) {
            return list.iter().any(|item| scan(item, depth + 1));
        }
        if let Some(tuple) = TupleRef::from_value(value) {
            return tuple.iter().any(|item| scan(item, depth + 1));
        }
        if let Some(dict) = DictRef::from_value(value) {
            return dict
                .iter()
                .any(|(key, val)| scan(key, depth + 1) || scan(val, depth + 1));
        }
        false
    }
    scan(value, 0)
}

/// Downcast an entity builtin's first argument to its [`EntityDef`], naming the
/// builtin in the error.
fn entity_arg<'v>(builtin: &str, entity: Value<'v>) -> anyhow::Result<&'v EntityDef> {
    entity.downcast_ref::<EntityDef>().ok_or_else(|| {
        anyhow::anyhow!(
            "{builtin}() first argument must be an entity(...), got {}",
            entity.get_type()
        )
    })
}

// ---------------------------------------------------------------------------
// Input schema (commands)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, ProvidesStaticType, NoSerialize, Allocative)]
pub struct InputSchema {
    pub fields: Vec<(String, FieldKind)>,
}

impl fmt::Display for InputSchema {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "schema({} fields)", self.fields.len())
    }
}

#[starlark_value(type = "input_schema")]
impl<'v> StarlarkValue<'v> for InputSchema {}
starlark_simple_value!(InputSchema);

// ---------------------------------------------------------------------------
// Entity schema (projectors)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, ProvidesStaticType, NoSerialize, Allocative)]
pub struct IndexDef {
    pub name: String,
    /// Ordered. Left-to-right sort precedence, exactly like a SQL composite index.
    pub columns: Vec<String>,
}

impl fmt::Display for IndexDef {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "index({}, {:?})", self.name, self.columns)
    }
}

#[starlark_value(type = "index")]
impl<'v> StarlarkValue<'v> for IndexDef {}
starlark_simple_value!(IndexDef);

/// Process-unique handles for entity values. A projector's `handle` refers to
/// an entity by value (`put(users, ...)`); this id is baked into the frozen
/// entity so the host can map that reference back to the definition (and its
/// load-resolved name) without a magic string.
static ENTITY_ID: AtomicU64 = AtomicU64::new(1);

fn next_entity_id() -> u64 {
    ENTITY_ID.fetch_add(1, Ordering::Relaxed)
}

/// Process-unique handles for event definitions. Every `event(...)` call mints a
/// fresh one and the registry keeps the id of the definition it registered, so the
/// host can tell a declared definition from one a handler built at runtime under the
/// same type name. Cloning preserves it, so loading a definition, re-binding it under
/// a second name, and registering it all yield the same id.
static EVENT_DEF_ID: AtomicU64 = AtomicU64::new(1);

fn next_event_def_id() -> u64 {
    EVENT_DEF_ID.fetch_add(1, Ordering::Relaxed)
}

#[derive(Debug, Clone, ProvidesStaticType, NoSerialize, Allocative)]
pub struct EntityDef {
    pub id: u64,
    /// The table name. An explicit `name=` override, otherwise empty until the
    /// host fills it from the global binding at load.
    pub name: String,
    pub key: String,
    pub fields: Vec<(String, FieldMeta)>,
    pub indexes: Vec<IndexDef>,
}

impl fmt::Display for EntityDef {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let name = if self.name.is_empty() {
            "<unbound>"
        } else {
            &self.name
        };
        write!(f, "entity({name})")
    }
}

#[starlark_value(type = "entity")]
impl<'v> StarlarkValue<'v> for EntityDef {}
starlark_simple_value!(EntityDef);

impl EntityDef {
    /// DDL generation. This is the payoff of the declared schema: users never
    /// write SQL or migrations.
    pub fn create_table_sql(&self) -> String {
        let cols: Vec<String> = self
            .fields
            .iter()
            .map(|(name, meta)| {
                let null = if meta.is_nullable() { "" } else { " NOT NULL" };
                let pk = if *name == self.key {
                    " PRIMARY KEY"
                } else {
                    ""
                };
                format!("  {} {}{pk}{null}", quote_ident(name), meta.sql_type())
            })
            .collect();
        format!(
            "CREATE TABLE IF NOT EXISTS {} (\n{}\n)",
            quote_ident(&self.name),
            cols.join(",\n")
        )
    }

    pub fn create_index_sql(&self) -> Vec<String> {
        self.indexes
            .iter()
            .map(|ix| {
                let columns: Vec<String> = ix.columns.iter().map(|col| quote_ident(col)).collect();
                format!(
                    "CREATE INDEX IF NOT EXISTS {} ON {} ({})",
                    quote_ident(&format!("{}_{}", self.name, ix.name)),
                    quote_ident(&self.name),
                    columns.join(", ")
                )
            })
            .collect()
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        // Generated SQL quotes every identifier, so this is not what keeps the SQL
        // well-formed; it keeps a `name =` override (or an `index("...")` name) to
        // something that reads as a table name in a schema dump, a log line or an
        // ad-hoc query, which a name carrying quotes or spaces would not.
        if !is_sql_identifier(&self.name) {
            anyhow::bail!(
                "entity `{}`: table name must be ascii letters, digits and underscores, starting with a letter or underscore",
                self.name
            );
        }
        for ix in &self.indexes {
            if !is_sql_identifier(&ix.name) {
                anyhow::bail!(
                    "entity `{}`: index name `{}` must be ascii letters, digits and underscores, starting with a letter or underscore",
                    self.name,
                    ix.name
                );
            }
        }
        let Some((_, key_meta)) = self.fields.iter().find(|(n, _)| *n == self.key) else {
            anyhow::bail!(
                "entity `{}`: key `{}` is not a declared field",
                self.name,
                self.key
            );
        };
        // The read API paginates by the key as an opaque cursor and binds it as a
        // typed filter, so the key must be a present, orderable scalar. An optional
        // key could be null; a bool (two values) or json (unordered) key would
        // silently truncate cursor pagination; money is stored as its decimal string
        // (so `ORDER BY` and the `key > ?` cursor would compare lexicographically:
        // `"2" > "10"`), so it cannot key the ordered scan either.
        if key_meta.is_nullable() {
            anyhow::bail!(
                "entity `{}`: key `{}` may not be optional",
                self.name,
                self.key
            );
        }
        if matches!(
            key_meta.kind.base(),
            FieldKind::Bool | FieldKind::Json | FieldKind::Money
        ) {
            anyhow::bail!(
                "entity `{}`: key `{}` must be an orderable scalar, not {:?}",
                self.name,
                self.key,
                key_meta.kind.base()
            );
        }
        if key_meta.subject.is_some() {
            anyhow::bail!(
                "entity `{}`: key `{}` may not be subject-encrypted (the key is a plaintext cursor)",
                self.name,
                self.key
            );
        }
        // A subject-scoped column needs its sibling subject-id column present so the
        // read API can find the key to decrypt it; the `entity()` builtin's
        // `validate_subject_refs` already enforces that (and rejects a chained or
        // json subject), so it holds by the time we get here.
        for ix in &self.indexes {
            for col in &ix.columns {
                match self.fields.iter().find(|(n, _)| n == col) {
                    None => anyhow::bail!(
                        "entity `{}`: index `{}` references unknown field `{}`",
                        self.name,
                        ix.name,
                        col
                    ),
                    // A subject column holds ciphertext, so a filter (which arrives
                    // as plaintext, and without the subject cannot derive the key)
                    // could never match it. Reject the index rather than surprise the
                    // author with a silent no-op. Filter by the plaintext subject id.
                    Some((_, meta)) if meta.subject.is_some() => anyhow::bail!(
                        "entity `{}`: index `{}` covers subject-encrypted column `{}`; filter by the plaintext subject id instead",
                        self.name,
                        ix.name,
                        col
                    ),
                    Some(_) => {}
                }
            }
        }
        // A read filter targets the key or an index-leading column, so a field named
        // like a reserved read query param could never be filtered. Reject at load
        // rather than surprising the author with a silent no-op at request time.
        let mut filterable = vec![self.key.as_str()];
        filterable.extend(
            self.indexes
                .iter()
                .filter_map(|ix| ix.columns.first())
                .map(String::as_str),
        );
        for field in filterable {
            if RESERVED_QUERY_PARAMS.contains(&field) {
                anyhow::bail!(
                    "entity `{}`: filterable field `{}` collides with a reserved read query param (one of: {})",
                    self.name,
                    field,
                    RESERVED_QUERY_PARAMS.join(", ")
                );
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Event definition: declares fields and which fields become store tags
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, ProvidesStaticType, NoSerialize, Allocative)]
pub struct EventDef {
    /// This definition's identity, from [`next_event_def_id`]. Tells the registered
    /// definition apart from a same-named one built inside a function body.
    pub id: u64,
    pub event_type: String,
    /// Every field, with its per-field tagging and encryption policy. Under
    /// automatic tagging each `indexed` field becomes a store tag; there is no
    /// separate tag list to keep in sync.
    pub fields: Vec<(String, FieldMeta)>,
}

impl EventDef {
    /// The declared field metadata for `name`, if any.
    pub fn field(&self, name: &str) -> Option<&FieldMeta> {
        self.fields.iter().find(|(n, _)| n == name).map(|(_, m)| m)
    }

    /// Whether `name` is a subject-scoped (encrypted) field. The single authority both
    /// command-response paths use to drop subject tags, so they cannot drift.
    pub fn is_subject(&self, name: &str) -> bool {
        self.field(name).is_some_and(|meta| meta.subject.is_some())
    }
}

impl fmt::Display for EventDef {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "event({})", self.event_type)
    }
}

#[starlark_value(type = "event_def")]
impl<'v> StarlarkValue<'v> for EventDef {
    /// An event definition is callable: `user_registered(user_id = ..., email = ...)`
    /// builds one concrete event, validating the payload against the declared
    /// fields and deriving tags from the declared tag fields. A command's `handle`
    /// returns the result directly, or a list of them. Named arguments only, so the
    /// call reads like the schema it checks against.
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let heap = eval.heap();
        // A `QueryCtx` on the evaluator means this call is a query/source clause (a
        // subset match), not an event to emit (which needs every field).
        let query_mode = eval
            .extra
            .and_then(|extra| extra.downcast_ref::<QueryCtx>())
            .is_some();
        args.no_positional_args(heap)?;
        let named = args.names_map()?;

        let mut payload = serde_json::Map::with_capacity(named.len());
        for (name, value) in &named {
            // A subject-encrypted value read from an event (a handle) must never be
            // fed back into a constructor: it would serialise to its ciphertext and be
            // re-encrypted, storing ciphertext-of-ciphertext. A derivation must be
            // built from the plaintext the command already holds.
            if contains_cipher_handle(*value) {
                return Err(anyhow::anyhow!(
                    "event `{}` field `{}`: a subject-encrypted value from an event cannot be re-emitted; supply the plaintext",
                    self.event_type,
                    name.as_str()
                )
                .into());
            }
            let json = value.to_json_value().map_err(|err| {
                anyhow::anyhow!(
                    "event `{}` field `{}` must be JSON-serialisable: {err}",
                    self.event_type,
                    name.as_str()
                )
            })?;
            payload.insert(name.as_str().to_owned(), json);
        }

        if query_mode {
            let constraints = build_query_constraints(&self.event_type, &payload)?;
            return Ok(heap.alloc(EventSpec::Filter {
                event_type: self.event_type.clone(),
                def_id: self.id,
                constraints,
            }));
        }

        validate_event_payload(&self.event_type, &self.fields, &payload)?;
        let tags = derive_tags(&self.event_type, &self.fields, &payload)?;
        Ok(heap.alloc(ConstructedEvent {
            def_id: self.id,
            event_type: self.event_type.clone(),
            data_json: serde_json::Value::Object(payload).to_string(),
            tags,
        }))
    }

    /// Compare by identity, not by type name, so equality agrees with the rule the
    /// append seam enforces: a definition built inside a function body is a different
    /// definition even when it declares the same type.
    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(other
            .downcast_ref::<EventDef>()
            .is_some_and(|o| o.id == self.id))
    }

    /// Hashable so a definition can key a per-type dispatch map (a command's `fold`,
    /// a projector or effect `handle`). Hash the process-unique id rather than the
    /// pointer: freezing a dict carries each key's pre-freeze hash through verbatim,
    /// so a hash that moved on freeze would silently break lookup.
    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        self.id.hash(hasher);
        Ok(())
    }
}
starlark_simple_value!(EventDef);

// ---------------------------------------------------------------------------
// Constructed event
// ---------------------------------------------------------------------------

/// One concrete event produced by calling an event definition. The payload is
/// held as its JSON wire form (a validated, serialised object) and the tags are
/// already derived, so the dispatch layer appends it without re-validating.
#[derive(Debug, Clone, ProvidesStaticType, NoSerialize, Allocative)]
pub struct ConstructedEvent {
    /// The [`EventDef::id`] of the definition that built this event, checked against
    /// the registry before it is emitted.
    pub def_id: u64,
    pub event_type: String,
    pub data_json: String,
    pub tags: Vec<(String, Option<String>)>,
}

impl fmt::Display for ConstructedEvent {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "event({})", self.event_type)
    }
}

#[starlark_value(type = "event")]
impl<'v> StarlarkValue<'v> for ConstructedEvent {}
starlark_simple_value!(ConstructedEvent);

// ---------------------------------------------------------------------------
// Opaque handle for a subject-encrypted value read from event data
// ---------------------------------------------------------------------------

/// A subject-scoped field, as a `fold` or projector `handle` sees it: an opaque
/// wrapper around the ciphertext, never the plaintext. It can be compared for
/// equality (deterministic encryption makes ciphertext equality mean plaintext
/// equality) and used as a dict key, and it can be stored with `put`/`patch` (which
/// persist the ciphertext). It cannot be concatenated, sliced, case-changed, or
/// otherwise turned into an inspectable string: those operations are simply not
/// defined on it, so they error. Its `str()` form is a fixed token, so even
/// interpolating it into a log line or error leaks nothing. Plaintext never enters a
/// handler; a derivation must be computed by the command and emitted as its own
/// subject-scoped field.
#[derive(Debug, Clone, ProvidesStaticType, Allocative)]
pub struct CipherHandle {
    /// The base64url ciphertext, the only thing this yields (to `put`, and to a tag).
    pub ciphertext: String,
    /// The originating event field name, bound into the ciphertext as its associated
    /// data. A handle may only be stored into an identically-named subject column.
    pub field: String,
    /// The subject field this value is scoped to (its key's identity), for the
    /// `put`/`patch` consistency check.
    pub subject_field: String,
    /// The plaintext subject id (not secret), for the `put`/`patch` consistency check.
    pub subject_value: String,
}

impl fmt::Display for CipherHandle {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "<encrypted:{}>", self.field)
    }
}

impl serde::Serialize for CipherHandle {
    /// Serialises to its ciphertext, so `put`'s `row.to_json_value()` stores the
    /// opaque ciphertext in the read-model column. This is the one place a handle
    /// yields its bytes, and they are ciphertext, never plaintext.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.ciphertext)
    }
}

#[starlark_value(type = "encrypted")]
impl<'v> StarlarkValue<'v> for CipherHandle {
    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(other
            .downcast_ref::<CipherHandle>()
            .is_some_and(|o| o.ciphertext == self.ciphertext))
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        self.ciphertext.hash(hasher);
        Ok(())
    }
}
starlark_simple_value!(CipherHandle);

// ---------------------------------------------------------------------------
// Query spec (commands): the DCB consistency boundary
// ---------------------------------------------------------------------------

/// The consistency boundary a command's `query` reads over, or one arm of a
/// projector's or effect's subscription, lowered to a tephra `QueryItem`.
#[derive(Debug, Clone, ProvidesStaticType, NoSerialize, Allocative)]
pub enum EventSpec {
    /// Every event. Lowers to `Query::All`, a full scan that bypasses the index.
    All,
    /// One typed query clause: events of `event_type` that satisfy every field
    /// `constraint` (ANDed). An empty `constraints` matches all events of that type.
    /// Produced by calling an event definition in query position, e.g.
    /// `OrderPlaced(shop_id = 42)`.
    Filter {
        event_type: String,
        /// The identity of the definition this clause was built from, so a clause used
        /// as a dispatch key can be told apart from one built by calling `event(...)`
        /// inline. Deliberately outside [`EventSpec`]'s hash and equality: two
        /// references to one loaded definition already share an id, and the
        /// registration check is separate.
        def_id: u64,
        /// Field name to its constrained value, as a scalar string (type-checked
        /// against the field's kind when the clause was built), **sorted by field
        /// name**. Sorted so one predicate is one key: `f(a = 1, b = 2)` and
        /// `f(b = 2, a = 1)` must not become two dispatch arms that both fire. It also
        /// matches tephra's `Tags`, whose containment check is a merge over two sorted
        /// sequences. The lowering to a tephra query encrypts a subject-scoped field's
        /// value; plaintext fields match verbatim.
        constraints: Vec<(String, String)>,
    },
}

impl EventSpec {
    /// Whether two clauses select the same event type. Used to reject two bare keys
    /// of one type, which would be the same predicate written twice.
    pub fn same_type(&self, other: &EventSpec) -> bool {
        match (self, other) {
            (EventSpec::All, EventSpec::All) => true,
            (
                EventSpec::Filter { event_type, .. },
                EventSpec::Filter {
                    event_type: other, ..
                },
            ) => event_type == other,
            _ => false,
        }
    }

    /// The type this clause selects, or `None` for `all_events()`.
    pub fn event_type(&self) -> Option<&str> {
        match self {
            EventSpec::All => None,
            EventSpec::Filter { event_type, .. } => Some(event_type),
        }
    }

    /// The identity of the definition this clause was built from, or `None` for
    /// `all_events()`, which is a builtin rather than a definition.
    pub fn def_id(&self) -> Option<u64> {
        match self {
            EventSpec::All => None,
            EventSpec::Filter { def_id, .. } => Some(*def_id),
        }
    }
}
impl fmt::Display for EventSpec {
    /// Rendered the way it was written, constraints included, so an error over a map
    /// with several clauses of one type names the arm rather than just its type.
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            EventSpec::All => write!(f, "all_events()"),
            EventSpec::Filter {
                event_type,
                constraints,
                ..
            } => {
                write!(f, "{event_type}(")?;
                for (index, (field, value)) in constraints.iter().enumerate() {
                    if index > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{field} = {value:?}")?;
                }
                write!(f, ")")
            }
        }
    }
}

#[starlark_value(type = "event_spec")]
impl<'v> StarlarkValue<'v> for EventSpec {
    /// Compare by predicate, not by identity: a clause is a value, and two clauses
    /// that select the same events are the same dispatch key.
    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        let Some(other) = other.downcast_ref::<EventSpec>() else {
            return Ok(false);
        };
        Ok(match (self, other) {
            (EventSpec::All, EventSpec::All) => true,
            (
                EventSpec::Filter {
                    event_type: left,
                    constraints: left_constraints,
                    ..
                },
                EventSpec::Filter {
                    event_type: right,
                    constraints: right_constraints,
                    ..
                },
            ) => left == right && left_constraints == right_constraints,
            _ => false,
        })
    }

    /// Hashable so a clause can key a dispatch map. Constraints are sorted at
    /// construction, so the hash is stable across argument order.
    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        match self {
            EventSpec::All => 0u8.hash(hasher),
            EventSpec::Filter {
                event_type,
                constraints,
                ..
            } => {
                1u8.hash(hasher);
                event_type.hash(hasher);
                constraints.hash(hasher);
            }
        }
        Ok(())
    }
}
starlark_simple_value!(EventSpec);

// ---------------------------------------------------------------------------
// Rejection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, ProvidesStaticType, NoSerialize, Allocative)]
pub struct Rejection {
    pub code: String,
    pub message: String,
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "reject({}: {})", self.code, self.message)
    }
}

#[starlark_value(type = "rejection")]
impl<'v> StarlarkValue<'v> for Rejection {}
starlark_simple_value!(Rejection);

// ---------------------------------------------------------------------------
// Invalid input
// ---------------------------------------------------------------------------

/// A command's third terminal outcome: the input is malformed regardless of
/// state (a shape or parse-level problem, distinct from a state-dependent
/// [`Rejection`]). Maps to HTTP 400.
#[derive(Debug, Clone, ProvidesStaticType, NoSerialize, Allocative)]
pub struct InvalidInput {
    pub message: String,
}

impl fmt::Display for InvalidInput {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "invalid_input({})", self.message)
    }
}

#[starlark_value(type = "invalid_input")]
impl<'v> StarlarkValue<'v> for InvalidInput {}
starlark_simple_value!(InvalidInput);

// ---------------------------------------------------------------------------
// Entity operations (projectors): what a projector's `handle` emits per event
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Allocative)]
pub enum EntityOpKind {
    /// Replace a whole row. Stored as a JSON object string, keyed on apply by the
    /// entity's declared key field.
    Put(String),
    /// Merge `changes` into the row with `key`: fields present are set, fields
    /// set to null are cleared, and columns not mentioned are left untouched.
    /// A no-op if the row doesn't exist.
    Patch { key: String, changes: String },
    /// Delete the row with this key.
    Delete(String),
}

#[derive(Debug, Clone, ProvidesStaticType, NoSerialize, Allocative)]
pub struct EntityOp {
    /// The [`EntityDef::id`] of the entity this op targets. The host resolves it
    /// back to the entity (and its table name) when applying the op.
    pub entity_id: u64,
    pub kind: EntityOpKind,
}

impl fmt::Display for EntityOp {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match &self.kind {
            EntityOpKind::Put(_) => write!(f, "put(#{})", self.entity_id),
            EntityOpKind::Patch { key, .. } => write!(f, "patch(#{}, {key})", self.entity_id),
            EntityOpKind::Delete(key) => write!(f, "delete(#{}, {key})", self.entity_id),
        }
    }
}

#[starlark_value(type = "entity_op")]
impl<'v> StarlarkValue<'v> for EntityOp {}
starlark_simple_value!(EntityOp);

// ---------------------------------------------------------------------------
// Module kind + definition
// ---------------------------------------------------------------------------

/// Which directory convention a loaded file falls under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleKind {
    Command,
    Projector,
    Effect,
}

impl ModuleKind {
    /// The word used in diagnostics.
    pub fn label(self) -> &'static str {
        match self {
            ModuleKind::Command => "command",
            ModuleKind::Projector => "projector",
            ModuleKind::Effect => "effect",
        }
    }
}

#[derive(Debug, Clone)]
pub enum ModuleDef {
    Command {
        name: String,
        input: InputSchema,
    },
    Projector {
        name: String,
        entities: Vec<EntityDef>,
        /// The subscription: one or more specs, OR'd together into the read query.
        sources: Vec<EventSpec>,
    },
    Effect {
        name: String,
        /// The subscription: one or more specs, OR'd together into the read query.
        sources: Vec<EventSpec>,
    },
}

impl ModuleDef {
    pub fn name(&self) -> &str {
        match self {
            ModuleDef::Command { name, .. }
            | ModuleDef::Projector { name, .. }
            | ModuleDef::Effect { name, .. } => name,
        }
    }
}

// ---------------------------------------------------------------------------
// Builtins
// ---------------------------------------------------------------------------

#[starlark_module]
pub fn runtime_builtins(builder: &mut GlobalsBuilder) {
    // --- field types -------------------------------------------------------
    //
    // The scalar types reuse Starlark's builtin names (`str`, `int`, `bool`),
    // shadowing the standard globals. One rule keeps both meanings reachable: a
    // positional argument means Starlark's conversion, and no positional argument
    // means a field declaration. That works because every stdlib conversion is
    // positional-only and every field option (`indexed`, `subject`, `unique`,
    // `max_length`) is named-only.
    //
    // The one thing this costs is the zero-value idiom: `int()` and `bool()` no
    // longer produce `0` and `False`. Write the literals instead. (`str()` costs
    // nothing at all, since the standard `str` requires its argument.)

    fn str<'v>(
        #[starlark(require = pos)] a: Option<Value<'v>>,
        #[starlark(require = named)] max_length: Option<u32>,
        #[starlark(require = named)] indexed: Option<bool>,
        #[starlark(require = named)] subject: Option<String>,
        #[starlark(require = named)] unique: Option<bool>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        if let Some(a) = a {
            no_field_options(
                "str",
                &[
                    ("max_length", max_length.is_some()),
                    ("indexed", indexed.is_some()),
                    ("subject", subject.is_some()),
                    ("unique", unique.is_some()),
                ],
            )?;
            // Already a string: hand it back rather than copying, as the standard
            // `str` does.
            return Ok(match a.unpack_str() {
                Some(_) => a,
                None => eval.heap().alloc(a.to_str()),
            });
        }
        let ft = field_type(FieldKind::Text { max_length }, indexed, subject, unique)?;
        Ok(eval.heap().alloc(ft))
    }

    fn int<'v>(
        #[starlark(require = pos)] a: Option<Value<'v>>,
        base: Option<Value<'v>>,
        #[starlark(require = named)] indexed: Option<bool>,
        #[starlark(require = named)] subject: Option<String>,
        #[starlark(require = named)] unique: Option<bool>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        if let Some(a) = a {
            no_field_options(
                "int",
                &[
                    ("indexed", indexed.is_some()),
                    ("subject", subject.is_some()),
                    ("unique", unique.is_some()),
                ],
            )?;
            // Delegate rather than reimplement: base prefixes and arbitrary-precision
            // parsing live behind starlark's `StarlarkInt`, which is not public.
            let named: Vec<(&str, Value<'v>)> = match base {
                Some(base) => vec![("base", base)],
                None => Vec::new(),
            };
            return eval.eval_function(stdlib_int(), &[a], &named);
        }
        if let Some(base) = base {
            return Err(anyhow::anyhow!(
                "int(base = {}) has no value to convert; int() with no positional argument declares an i64 field",
                base.to_repr()
            )
            .into());
        }
        let ft = field_type(FieldKind::I64, indexed, subject, unique)?;
        Ok(eval.heap().alloc(ft))
    }

    /// No standard global to shadow, so this one is a plain field type.
    fn uint(
        #[starlark(require = named)] indexed: Option<bool>,
        #[starlark(require = named)] subject: Option<String>,
        #[starlark(require = named)] unique: Option<bool>,
    ) -> anyhow::Result<FieldType> {
        field_type(FieldKind::U64, indexed, subject, unique)
    }

    fn bool<'v>(
        #[starlark(require = pos)] a: Option<Value<'v>>,
        #[starlark(require = named)] indexed: Option<bool>,
        #[starlark(require = named)] subject: Option<String>,
        #[starlark(require = named)] unique: Option<bool>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        if let Some(a) = a {
            no_field_options(
                "bool",
                &[
                    ("indexed", indexed.is_some()),
                    ("subject", subject.is_some()),
                    ("unique", unique.is_some()),
                ],
            )?;
            return Ok(Value::new_bool(a.to_bool()));
        }
        let ft = field_type(FieldKind::Bool, indexed, subject, unique)?;
        Ok(eval.heap().alloc(ft))
    }

    fn uuid(
        #[starlark(require = named)] indexed: Option<bool>,
        #[starlark(require = named)] subject: Option<String>,
        #[starlark(require = named)] unique: Option<bool>,
    ) -> anyhow::Result<FieldType> {
        field_type(FieldKind::Uuid, indexed, subject, unique)
    }

    fn timestamp(
        #[starlark(require = named)] indexed: Option<bool>,
        #[starlark(require = named)] subject: Option<String>,
        #[starlark(require = named)] unique: Option<bool>,
    ) -> anyhow::Result<FieldType> {
        field_type(FieldKind::Timestamp, indexed, subject, unique)
    }

    fn money(
        #[starlark(require = named)] indexed: Option<bool>,
        #[starlark(require = named)] subject: Option<String>,
        #[starlark(require = named)] unique: Option<bool>,
    ) -> anyhow::Result<FieldType> {
        field_type(FieldKind::Money, indexed, subject, unique)
    }

    fn json(
        #[starlark(require = named)] indexed: Option<bool>,
        #[starlark(require = named)] subject: Option<String>,
        #[starlark(require = named)] unique: Option<bool>,
    ) -> anyhow::Result<FieldType> {
        field_type(FieldKind::Json, indexed, subject, unique)
    }

    /// Named `one_of` rather than `enum` because the overload rule the scalar
    /// types use does not reach it: `enum(["a", "b"])` and a variant list are both
    /// positional, so there would be nothing to tell them apart. (`enum` itself is
    /// free, being a starlark-rust extension rather than standard Starlark, and
    /// kiln builds standard globals.)
    fn one_of(
        #[starlark(require = pos)] variants: UnpackList<String>,
        #[starlark(require = named)] indexed: Option<bool>,
        #[starlark(require = named)] subject: Option<String>,
        #[starlark(require = named)] unique: Option<bool>,
    ) -> anyhow::Result<FieldType> {
        if variants.items.is_empty() {
            anyhow::bail!("one_of() needs at least one variant");
        }
        field_type(FieldKind::OneOf(variants.items), indexed, subject, unique)
    }

    /// A nullable field. Inherits the inner field's `indexed`/`subject`/`unique`
    /// policy, so `optional(str(subject = "customer_id", max_length = 200))` is an
    /// optional subject-scoped field.
    fn optional(#[starlark(require = pos)] inner: &FieldType) -> anyhow::Result<FieldType> {
        if matches!(inner.0.kind, FieldKind::Optional(_)) {
            anyhow::bail!("optional(optional(...)) is not meaningful");
        }
        Ok(FieldType(FieldMeta {
            kind: FieldKind::Optional(Box::new(inner.0.kind.clone())),
            indexed: inner.0.indexed,
            subject: inner.0.subject.clone(),
            unique: inner.0.unique,
        }))
    }

    // --- schema ------------------------------------------------------------

    fn schema<'v>(
        #[starlark(kwargs)] fields: SmallMap<String, Value<'v>>,
    ) -> anyhow::Result<InputSchema> {
        let mut out = Vec::with_capacity(fields.len());
        for (name, value) in fields {
            let ft = value.downcast_ref::<FieldType>().ok_or_else(|| {
                anyhow::anyhow!(
                    "schema field `{name}` must be a field type, got {}",
                    value.get_type()
                )
            })?;
            // Command input is plaintext at the boundary; subject/unique are event
            // and entity concerns, not input ones.
            if ft.0.subject.is_some() || ft.0.unique {
                anyhow::bail!(
                    "schema field `{name}`: subject/unique are not valid on command input (input is plaintext)"
                );
            }
            out.push((name, ft.0.kind.clone()));
        }
        Ok(InputSchema { fields: out })
    }

    // --- entities ----------------------------------------------------------

    fn index(
        #[starlark(require = pos)] name: String,
        #[starlark(require = pos)] columns: UnpackList<String>,
    ) -> anyhow::Result<IndexDef> {
        if columns.items.is_empty() {
            anyhow::bail!("index `{name}` needs at least one column");
        }
        Ok(IndexDef {
            name,
            columns: columns.items,
        })
    }

    /// Declare a read-model table. `name` is optional; by default the table is
    /// named after the global it's bound to (`users = entity(...)` → `users`);
    /// pass `name` only to override that (e.g. to match a legacy table). The
    /// entity is included in its projector automatically; there is no list to
    /// keep in sync. Validation is deferred to load, once the name is known.
    fn entity<'v>(
        #[starlark(require = named)] key: String,
        #[starlark(require = named)] fields: SmallMap<String, Value<'v>>,
        #[starlark(require = named)] indexes: Option<UnpackList<Value<'v>>>,
        #[starlark(require = named)] name: Option<String>,
    ) -> anyhow::Result<EntityDef> {
        let mut field_defs = Vec::with_capacity(fields.len());
        for (fname, value) in fields {
            let ft = value
                .downcast_ref::<FieldType>()
                .ok_or_else(|| anyhow::anyhow!("entity field `{fname}` must be a field type"))?;
            field_defs.push((fname, ft.0.clone()));
        }
        validate_subject_refs("entity", &field_defs)?;

        let mut index_defs = Vec::new();
        for value in indexes.unwrap_or_default() {
            let ix = value
                .downcast_ref::<IndexDef>()
                .ok_or_else(|| anyhow::anyhow!("entity indexes must be index(...)"))?;
            index_defs.push(ix.clone());
        }

        Ok(EntityDef {
            id: next_entity_id(),
            name: name.unwrap_or_default(),
            key,
            fields: field_defs,
            indexes: index_defs,
        })
    }

    // --- event definition --------------------------------------------------

    /// Declare an event type. Every field is automatically indexed as a store tag
    /// unless it opts out with `indexed = False`; there is no `tags = [...]` list.
    fn event<'v>(
        #[starlark(require = named)] r#type: String,
        #[starlark(require = named)] fields: SmallMap<String, Value<'v>>,
    ) -> anyhow::Result<EventDef> {
        let mut field_defs = Vec::with_capacity(fields.len());
        for (name, value) in fields {
            let ft = value.downcast_ref::<FieldType>().ok_or_else(|| {
                anyhow::anyhow!("event `{}` field `{name}` must be a field type", r#type)
            })?;
            // The host stamps its own tags in the `_kiln_` namespace (the
            // idempotency tag, and the global uniqueness tag). Reserving the prefix
            // keeps a handler from forging a host tag, and so an append condition.
            if name.starts_with(RESERVED_TAG_PREFIX) {
                anyhow::bail!(
                    "event `{}`: field `{name}` uses the reserved `{RESERVED_TAG_PREFIX}` prefix",
                    r#type
                );
            }
            field_defs.push((name, ft.0.clone()));
        }
        validate_subject_refs(&format!("event `{}`", r#type), &field_defs)?;
        Ok(EventDef {
            id: next_event_def_id(),
            event_type: r#type,
            fields: field_defs,
        })
    }

    // --- DCB query ---------------------------------------------------------

    /// The catch-all query: every event, regardless of type. Lowers to tephra's
    /// `Query::All` (a full scan that bypasses the index). Projectors that build a
    /// global read model use this; commands rarely do. Typed queries and sources
    /// otherwise name event types by calling their definitions, e.g.
    /// `OrderPlaced(shop_id = 42)` (a subset match), or `OrderPlaced()` for every
    /// event of that type.
    fn all_events() -> anyhow::Result<EventSpec> {
        Ok(EventSpec::All)
    }

    // --- control flow ------------------------------------------------------

    fn reject(
        #[starlark(require = pos)] code: String,
        #[starlark(require = pos)] message: String,
    ) -> anyhow::Result<Rejection> {
        Ok(Rejection { code, message })
    }

    /// Refuse a command because the input is malformed regardless of state (a
    /// shape or parse-level problem). Distinct from `reject`, which refuses
    /// well-formed input the current state forbids. Maps to HTTP 400.
    fn invalid_input(#[starlark(require = pos)] message: String) -> anyhow::Result<InvalidInput> {
        Ok(InvalidInput { message })
    }

    // --- projector entity ops ----------------------------------------------

    /// Replace a whole row in an entity's read model. `entity` is the entity
    /// value itself (`put(users, ...)`), not its name; `row` is a dict carrying
    /// every non-`optional` field (including the key). Columns absent from `row`
    /// are dropped.
    fn put<'v>(
        #[starlark(require = pos)] entity: Value<'v>,
        #[starlark(require = pos)] row: Value<'v>,
    ) -> anyhow::Result<EntityOp> {
        let def = entity_arg("put", entity)?;
        // Handle provenance only survives on the Starlark value, so the row has to be
        // checked before `to_json_value` flattens it.
        let dict = DictRef::from_value(row)
            .ok_or_else(|| anyhow::anyhow!("put() row must be a dict, got {}", row.get_type()))?;
        enforce_subject_columns(def, &dict, None).map_err(|err| anyhow::anyhow!("put(): {err}"))?;
        let json = row
            .to_json_value()
            .map_err(|err| anyhow::anyhow!("put() row must be JSON-serialisable: {err}"))?;
        let obj = json
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("put() row must be a dict, got {}", row.get_type()))?;
        validate_row(&def.fields, obj, true).map_err(|err| anyhow::anyhow!("put(): {err}"))?;
        Ok(EntityOp {
            entity_id: def.id,
            kind: EntityOpKind::Put(json.to_string()),
        })
    }

    /// Partially update the row keyed by `key`: fields in `changes` are set,
    /// fields set to `None` are cleared (must be `optional`), and columns not
    /// mentioned keep their current values. A no-op if the row doesn't exist.
    fn patch<'v>(
        #[starlark(require = pos)] entity: Value<'v>,
        #[starlark(require = pos)] key: String,
        #[starlark(require = pos)] changes: Value<'v>,
    ) -> anyhow::Result<EntityOp> {
        let def = entity_arg("patch", entity)?;
        let dict = DictRef::from_value(changes).ok_or_else(|| {
            anyhow::anyhow!("patch() changes must be a dict, got {}", changes.get_type())
        })?;
        enforce_subject_columns(def, &dict, Some(&key))
            .map_err(|err| anyhow::anyhow!("patch(): {err}"))?;
        let json = changes
            .to_json_value()
            .map_err(|err| anyhow::anyhow!("patch() changes must be JSON-serialisable: {err}"))?;
        let obj = json.as_object().ok_or_else(|| {
            anyhow::anyhow!("patch() changes must be a dict, got {}", changes.get_type())
        })?;
        if obj.is_empty() {
            anyhow::bail!("patch() changes is empty");
        }
        if obj.contains_key(&def.key) {
            anyhow::bail!("patch() cannot change the key field `{}`", def.key);
        }
        validate_row(&def.fields, obj, false).map_err(|err| anyhow::anyhow!("patch(): {err}"))?;
        Ok(EntityOp {
            entity_id: def.id,
            kind: EntityOpKind::Patch {
                key,
                changes: json.to_string(),
            },
        })
    }

    /// Delete the row with `key` from an entity's read model. `entity` is the
    /// entity value itself (`delete(users, ...)`), not its name.
    fn delete<'v>(
        #[starlark(require = pos)] entity: Value<'v>,
        #[starlark(require = pos)] key: String,
    ) -> anyhow::Result<EntityOp> {
        let def = entity_arg("delete", entity)?;
        Ok(EntityOp {
            entity_id: def.id,
            kind: EntityOpKind::Delete(key),
        })
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

pub struct LoadedModule {
    pub def: ModuleDef,
    pub module: FrozenModule,
    /// The source hash: the module's deployed identity, and an effect's script
    /// hash on each invocation.
    pub source_hash: String,
}

/// A module's name is its file stem, validated as a slug so it maps cleanly onto
/// a table or topic name: lowercase ASCII letters, digits and single interior
/// hyphens.
pub fn module_name_from_path(filename: &str) -> anyhow::Result<String> {
    let stem = Path::new(filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("cannot derive a name from the filename"))?;
    validate_slug(stem)?;
    Ok(stem.to_owned())
}

fn validate_slug(s: &str) -> anyhow::Result<()> {
    if s.is_empty() {
        anyhow::bail!("name is empty");
    }
    if s.starts_with('-') || s.ends_with('-') {
        anyhow::bail!("name `{s}` must not start or end with a hyphen");
    }
    let mut prev_hyphen = false;
    for &b in s.as_bytes() {
        if !(b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
            anyhow::bail!("name `{s}` must be lowercase letters, digits and single hyphens");
        }
        if b == b'-' && prev_hyphen {
            anyhow::bail!("name `{s}` must not contain a double hyphen");
        }
        prev_hyphen = b == b'-';
    }
    Ok(())
}

/// The load-time instruction budget. Bounds evaluation of a module's top-level
/// (schema declarations, `def` bindings). Per-event limits are set separately on
/// the dispatch evaluator.
const LOAD_MAX_TICKS: u64 = 10_000_000;

/// Parse a `.star` file into an AST under the standard dialect. `load()` is
/// enabled by the dialect; the caller wires the resolver at evaluation time.
pub fn parse_module(filename: &str, src: String) -> starlark::Result<AstModule> {
    AstModule::parse(filename, src, &Dialect::Standard)
}

/// Evaluate and freeze a parsed module. `loader` resolves the module's `load()`
/// imports; pass `None` for a standalone file with no imports.
///
/// `FrozenModule` is Send + Sync, so this runs once at load and the result is
/// shared across worker threads. Only the per-event `Evaluator` is cheap and
/// thread-local.
pub fn eval_frozen(
    ast: AstModule,
    globals: &Globals,
    loader: Option<&dyn FileLoader>,
    query_mode: bool,
) -> starlark::Result<FrozenModule> {
    Module::with_temp_heap(|module| {
        {
            let query_ctx = QueryCtx;
            let mut eval = Evaluator::new(&module);
            if let Some(loader) = loader {
                eval.set_loader(loader);
            }
            // A module body's dispatch keys call event definitions as query clauses, so
            // those calls must see query mode. Events are constructed inside `handle`,
            // which runs in its own evaluator, so this never reaches one.
            if query_mode {
                eval.extra = Some(&query_ctx);
            }
            eval.set_max_tick_count(LOAD_MAX_TICKS)?;
            eval.eval_module(ast, globals)?;
        }
        starlark::Result::Ok(module.freeze()?)
    })
}

/// Read a frozen module's declarations into a `ModuleDef` for the given kind.
///
/// Handlers (`query`, `fold`, `handle`) and schema globals (a command's `input` and
/// `initial`; a projector's entities) are named
/// top-level values read off the frozen module; there are no registration calls.
/// `handle` is always required; the rest depends on the kind. Failing here means
/// failing at load rather than on the first request. Messages name the global, not the
/// file: the [`Finding`](crate::loader::Finding) the loader builds already carries the
/// path as its location, and repeating it reads as a stutter.
pub fn module_def_from_frozen(
    kind: ModuleKind,
    name: String,
    module: &FrozenModule,
) -> anyhow::Result<ModuleDef> {
    // `query`, `initial` and `fold` are optional (a command with no invariants
    // omits them and the host calls `handle(input, None)` directly), but
    // `handle` is always required.
    if module.get_option("handle")?.is_none() {
        anyhow::bail!("missing required `handle` function");
    }
    Ok(match kind {
        ModuleKind::Command => {
            check_command_handlers(module)?;
            ModuleDef::Command {
                name,
                input: read_schema(module)?,
            }
        }
        ModuleKind::Projector => {
            let sources = read_event_handler(module)?;
            ModuleDef::Projector {
                name,
                entities: read_entities(module)?,
                sources,
            }
        }
        ModuleKind::Effect => ModuleDef::Effect {
            name,
            sources: read_event_handler(module)?,
        },
    })
}

/// Check a command's `initial`, `fold` and `handle` shapes.
///
/// `initial` is a plain value and never a function (see [`initial_state`]); `fold` is
/// a clause-keyed map; `handle` decides from input and folded state rather than from
/// one event, so per-clause dispatch belongs on `fold`, not on it.
fn check_command_handlers(module: &FrozenModule) -> anyhow::Result<()> {
    if let Some(owned) = module.get_option("initial")? {
        let val = owned.value();
        if val.get_type() == FUNCTION_TYPE {
            anyhow::bail!(
                "`initial` must be a value, not a function; write `initial = {{...}}` (fold returns the new state, so it never needs a fresh copy)"
            );
        }
        // Not a function and not data would reach `handle` as state and read as
        // nonsense there instead of here.
        if val.to_json_value().is_err() {
            anyhow::bail!(
                "`initial` must be a plain value (a dict, list, string, number, bool or None), got {}",
                val.get_type()
            );
        }
    }
    if let Some(owned) = module.get_option("fold")? {
        parse_event_dispatch(owned.value()).map_err(|err| anyhow::anyhow!("`fold` {err}"))?;
    }
    if let Some(owned) = module.get_option("handle")?
        && owned.value().get_type() != FUNCTION_TYPE
    {
        anyhow::bail!(
            "a command's `handle` takes (input, state) and must be a single function, got {}; per-clause dispatch belongs on `fold`",
            owned.value().get_type()
        );
    }
    Ok(())
}

/// Check a projector's or effect's `handle`, and read the subscription it implies.
///
/// The map *is* the subscription: its keys say which events to read and what to do
/// with each, so there is no `source` to keep in step with them. A leftover `source`
/// is rejected rather than ignored, because a silently ignored subscription reads as
/// a working one.
fn read_event_handler(module: &FrozenModule) -> anyhow::Result<Vec<EventSpec>> {
    let Some(owned) = module.get_option("handle")? else {
        anyhow::bail!("missing required `handle` map");
    };
    let dispatch =
        parse_event_dispatch(owned.value()).map_err(|err| anyhow::anyhow!("`handle` {err}"))?;
    if module.get_option("source")?.is_some() {
        anyhow::bail!(
            "`source` is no longer declared separately; `handle`'s keys are the subscription, so move each clause into the key that handles it"
        );
    }
    Ok(dispatch.specs())
}

/// Read the required `input = schema(...)` global off a command module.
fn read_schema(module: &FrozenModule) -> anyhow::Result<InputSchema> {
    let Some(owned) = module.get_option("input")? else {
        anyhow::bail!("command must define `input = schema(...)`");
    };
    let val = owned.value();
    let schema = val
        .downcast_ref::<InputSchema>()
        .ok_or_else(|| anyhow::anyhow!("`input` must be a schema(...), got {}", val.get_type()))?;
    Ok(schema.clone())
}

/// Collect a projector's entities.
///
/// Entities are gathered implicitly: every global bound to an `entity(...)` is a
/// table, named after its binding unless it carries an explicit `name=`. There
/// is no `entities = [...]` list to keep in sync.
fn read_entities(module: &FrozenModule) -> anyhow::Result<Vec<EntityDef>> {
    let bindings: Vec<String> = module
        .names()
        .filter_map(|n| n.to_value().unpack_str().map(str::to_owned))
        .collect();
    let mut entities = Vec::new();
    for binding in &bindings {
        let Some(owned) = module.get_option(binding)? else {
            continue;
        };
        let Some(def) = owned.value().downcast_ref::<EntityDef>() else {
            continue;
        };
        let mut resolved = def.clone();
        if resolved.name.is_empty() {
            resolved.name = binding.clone();
        }
        resolved.validate()?;
        entities.push(resolved);
    }
    // Deterministic DDL/output order regardless of how `names()` iterates.
    entities.sort_by(|a, b| a.name.cmp(&b.name));
    if entities.is_empty() {
        anyhow::bail!("projector defines no entities; assign one with `name = entity(...)`");
    }
    Ok(entities)
}

// ---------------------------------------------------------------------------
// Handler invocation
// ---------------------------------------------------------------------------

/// The shared body of every `call_handler*`: the only thing that varies between
/// them is the context put on the evaluator, which is what decides which
/// context-gated builtins resolve.
fn eval_with_extra<'v, 'a, 'e>(
    module: &Module<'v>,
    func: Value<'v>,
    args: &[Value<'v>],
    max_instructions: u64,
    extra: Option<&'a dyn AnyLifetime<'e>>,
) -> starlark::Result<Value<'v>> {
    let mut eval = Evaluator::new(module);
    eval.set_max_tick_count(max_instructions)?;
    eval.extra = extra;
    eval.eval_function(func, args, &[])
}

/// Call a handler. Synchronous and non-yielding, so this belongs on
/// `spawn_blocking`, never on a Tokio worker.
pub fn call_handler<'v>(
    module: &Module<'v>,
    func: Value<'v>,
    args: &[Value<'v>],
    max_instructions: u64,
) -> starlark::Result<Value<'v>> {
    eval_with_extra(module, func, args, max_instructions, None)
}

/// Call a handler with a [`HandleCtx`] in scope, so `now()` resolves. Used only
/// for a command's `handle`; `query` and `fold` go through [`call_handler`] with
/// no context, which is why `now()` is unavailable there.
pub fn call_handler_with_ctx<'v>(
    module: &Module<'v>,
    func: Value<'v>,
    args: &[Value<'v>],
    max_instructions: u64,
    ctx: &HandleCtx,
) -> starlark::Result<Value<'v>> {
    eval_with_extra(module, func, args, max_instructions, Some(ctx))
}

/// Call a command's `query` with a [`QueryCtx`] in scope, so an event-definition
/// call inside it builds a query clause (a subset match) rather than an event to
/// emit. `now()` still errors, because it needs a [`HandleCtx`], not this.
pub fn call_handler_with_query_ctx<'v>(
    module: &Module<'v>,
    func: Value<'v>,
    args: &[Value<'v>],
    max_instructions: u64,
) -> starlark::Result<Value<'v>> {
    let query_ctx = QueryCtx;
    eval_with_extra(module, func, args, max_instructions, Some(&query_ctx))
}

/// Call a projector's `handle` with a [`ProjectorCtx`] in scope, so `get()` can
/// read the read model. Used only for a projector's `handle`.
pub fn call_handler_with_projector_ctx<'v>(
    module: &Module<'v>,
    func: Value<'v>,
    args: &[Value<'v>],
    max_instructions: u64,
    ctx: &ProjectorCtx,
) -> starlark::Result<Value<'v>> {
    eval_with_extra(module, func, args, max_instructions, Some(ctx))
}

/// Call an effect's `handle` with an [`EffectCtx`] in scope, so the impure
/// builtins (`http.*`, `invoke_command`, `read`, `now`, `log`) resolve and journal
/// through the host. Used only for an effect's `handle`.
pub fn call_handler_with_effect_ctx<'v>(
    module: &Module<'v>,
    func: Value<'v>,
    args: &[Value<'v>],
    max_instructions: u64,
    ctx: &EffectCtx,
) -> starlark::Result<Value<'v>> {
    eval_with_extra(module, func, args, max_instructions, Some(ctx))
}

/// The globals for pure modules (projectors, and the `events/` and `lib/` files
/// every kind imports). No clock, no randomness, no I/O.
pub fn globals() -> Globals {
    GlobalsBuilder::standard().with(runtime_builtins).build()
}

/// Builtins available only to commands. `now()` is the request's pinned append
/// time, in scope during `handle` (where a [`HandleCtx`] is set on the evaluator)
/// and an error elsewhere. It exists as a global so a `handle` naming it resolves
/// at load; the `handle`-only guard is enforced at call time by the presence of
/// the context, so `query` and `fold` (evaluated without one) cannot read a clock.
#[starlark_module]
pub(crate) fn command_builtins(builder: &mut GlobalsBuilder) {
    fn now(eval: &mut Evaluator) -> anyhow::Result<String> {
        eval.extra
            .and_then(|extra| extra.downcast_ref::<HandleCtx>())
            .map(|ctx| ctx.now.clone())
            .ok_or_else(|| anyhow::anyhow!("now() is only available in handle()"))
    }
}

/// Globals for commands: the base builtins plus `now()`.
pub fn command_globals() -> Globals {
    GlobalsBuilder::standard()
        .with(runtime_builtins)
        .with(command_builtins)
        .build()
}

/// Builtins available only to projectors. `get(entity, key)` reads the current
/// row from the projector's own read model, through the current batch's
/// uncommitted writes (a [`ProjectorCtx`] on the evaluator carries the reader).
/// It exists as a global so a `handle` naming it resolves at load; the guard is
/// the presence of the context, so it errors anywhere but a projector `handle`.
#[starlark_module]
pub(crate) fn projector_builtins(builder: &mut GlobalsBuilder) {
    fn get<'v>(
        #[starlark(require = pos)] entity: Value<'v>,
        #[starlark(require = pos)] key: String,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let def = entity_arg("get", entity)?;
        let ctx = eval
            .extra
            .and_then(|extra| extra.downcast_ref::<ProjectorCtx>())
            .ok_or_else(|| anyhow::anyhow!("get() is only available in a projector's handle()"))?;
        let Some(row) = ctx.reader.get(def.id, &key)? else {
            return Ok(Value::new_none());
        };
        // Wrap subject columns as handles, exactly as an event is materialised, so a
        // read-modify-write (`get()` then `put()`) round-trips: the ciphertext stays
        // opaque and re-storing it is a valid handle, not a raw string.
        Ok(match row.as_object() {
            Some(obj) if def.fields.iter().any(|(_, m)| m.subject.is_some()) => {
                alloc_row_with_handles(eval.heap(), &def.fields, obj)
            }
            _ => eval.heap().alloc(row),
        })
    }
}

/// Globals for projectors: the base builtins plus `get()`. Still no clock, no
/// randomness, and no I/O beyond reading the projector's own read model.
pub fn projector_globals() -> Globals {
    GlobalsBuilder::standard()
        .with(runtime_builtins)
        .with(projector_builtins)
        .build()
}

/// The [`EffectHost`] in scope on the evaluator, or an error naming `what`. The
/// host is present only during an effect's `handle`, so the impure builtins error
/// anywhere else, keeping commands and projectors structurally pure.
fn effect_host<'e>(eval: &'e Evaluator, what: &str) -> anyhow::Result<&'e dyn EffectHost> {
    eval.extra
        .and_then(|extra| extra.downcast_ref::<EffectCtx>())
        .map(|ctx| ctx.host)
        .ok_or_else(|| anyhow::anyhow!("{what} is only available in an effect's handle()"))
}

/// Read a Starlark dict of `str: str` into header pairs.
fn header_pairs(value: Value<'_>) -> anyhow::Result<Vec<(String, String)>> {
    let dict = DictRef::from_value(value)
        .ok_or_else(|| anyhow::anyhow!("http headers must be a dict, got {}", value.get_type()))?;
    let mut out = Vec::with_capacity(dict.len());
    for (key, val) in dict.iter() {
        let key = key
            .unpack_str()
            .ok_or_else(|| anyhow::anyhow!("http header name must be a string"))?;
        let val = val
            .unpack_str()
            .ok_or_else(|| anyhow::anyhow!("http header `{key}` value must be a string"))?;
        out.push((key.to_owned(), val.to_owned()));
    }
    Ok(out)
}

type HttpArgs = (String, Vec<(String, String)>, Option<serde_json::Value>);

/// Pull `url` (required), `headers` (dict), and `body` (any JSON) out of an
/// `http.*` call's keyword arguments.
fn parse_http_args(method: &str, kwargs: SmallMap<String, Value<'_>>) -> anyhow::Result<HttpArgs> {
    let verb = method.to_ascii_lowercase();
    let mut url = None;
    let mut headers = Vec::new();
    let mut body = None;
    for (key, value) in kwargs {
        match key.as_str() {
            "url" => {
                url = Some(
                    value
                        .unpack_str()
                        .ok_or_else(|| anyhow::anyhow!("http.{verb}() url must be a string"))?
                        .to_owned(),
                );
            }
            "headers" => headers = header_pairs(value)?,
            "body" => {
                if !value.is_none() {
                    // The bodyless verbs never send a body, so accepting one would
                    // silently drop it (and skew the journaled call). Reject it.
                    if matches!(method, "GET" | "DELETE") {
                        anyhow::bail!("http.{verb}() does not take a body");
                    }
                    body = Some(value.to_json_value().map_err(|err| {
                        anyhow::anyhow!("http.{verb}() body must be JSON-serialisable: {err}")
                    })?);
                }
            }
            other => anyhow::bail!("http.{verb}() got unexpected argument `{other}`"),
        }
    }
    let url = url.ok_or_else(|| anyhow::anyhow!("http.{verb}() requires url="))?;
    Ok((url, headers, body))
}

/// The shared body of every `http.*` builtin: parse the arguments, then journal
/// the call through the effect host.
fn http_dispatch<'v>(
    method: &str,
    kwargs: SmallMap<String, Value<'v>>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> anyhow::Result<Value<'v>> {
    let (url, headers, body) = parse_http_args(method, kwargs)?;
    let host = effect_host(eval, &format!("http.{}()", method.to_ascii_lowercase()))?;
    let result = host.http(method, &url, headers, body)?;
    Ok(eval.heap().alloc(result))
}

/// Builtins available only to effects: the impure, journaled capabilities. Each
/// call is recorded in the effect journal, so a replay after a crash returns the
/// recorded result instead of performing the side effect again. They exist as
/// globals so an effect naming them resolves at load; the guard is the presence of
/// an [`EffectCtx`], so they error anywhere but an effect's `handle`. Commands and
/// projectors never see these globals, so purity stays structural.
#[starlark_module]
pub(crate) fn effect_builtins(builder: &mut GlobalsBuilder) {
    fn now(eval: &mut Evaluator) -> anyhow::Result<String> {
        effect_host(eval, "now()")?.now()
    }

    fn log(
        #[starlark(require = pos)] message: String,
        eval: &mut Evaluator,
    ) -> anyhow::Result<NoneType> {
        effect_host(eval, "log()")?.log(&message);
        Ok(NoneType)
    }

    /// Decrypt a subject-encrypted value read from an event to its plaintext. The
    /// explicit boundary an effect crosses to act on personal data; only effects have
    /// it. Fails (terminally) if the subject has been erased.
    fn reveal<'v>(
        #[starlark(require = pos)] handle: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<String> {
        let handle = handle.downcast_ref::<CipherHandle>().ok_or_else(|| {
            anyhow::anyhow!(
                "reveal() expects a subject-encrypted value from an event, got {}",
                handle.get_type()
            )
        })?;
        effect_host(eval, "reveal()")?.reveal(
            &handle.subject_field,
            &handle.subject_value,
            &handle.field,
            &handle.ciphertext,
        )
    }

    fn invoke_command<'v>(
        #[starlark(require = pos)] name: String,
        #[starlark(require = pos)] input: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let input_json = input.to_json_value().map_err(|err| {
            anyhow::anyhow!("invoke_command() input must be JSON-serialisable: {err}")
        })?;
        let host = effect_host(eval, "invoke_command()")?;
        let result = host.invoke_command(&name, input_json)?;
        Ok(eval.heap().alloc(result))
    }

    fn read<'v>(
        #[starlark(require = pos)] projector: String,
        #[starlark(require = pos)] entity: String,
        #[starlark(require = pos)] key: String,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let host = effect_host(eval, "read()")?;
        let result = host.read(&projector, &entity, &key)?;
        Ok(eval.heap().alloc(result))
    }

    fn scan<'v>(
        #[starlark(require = pos)] projector: String,
        #[starlark(require = pos)] entity: String,
        #[starlark(require = named)] field: Option<String>,
        #[starlark(require = named)] value: Option<String>,
        #[starlark(require = named)] cursor: Option<String>,
        #[starlark(require = named)] limit: Option<i32>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let filter = match (field, value) {
            (Some(field), Some(value)) => Some((field, value)),
            (None, None) => None,
            _ => anyhow::bail!("scan() `field` and `value` must be given together"),
        };
        // A negative limit would clamp to a one-row page, silently truncating the scan.
        let limit = match limit {
            Some(n) if n < 0 => anyhow::bail!("scan() limit must not be negative"),
            other => other.map(|n| n as usize),
        };
        let host = effect_host(eval, "scan()")?;
        let result = host.scan(&projector, &entity, filter, cursor, limit)?;
        Ok(eval.heap().alloc(result))
    }
}

/// The `http.*` namespace for effects: journaled HTTP calls. Each takes `url=`,
/// optional `headers=` (a dict), and (for the body-bearing verbs) `body=` (any
/// JSON), and returns `{status, body, headers}`. Transport failures and 5xx never
/// reach here (the runtime retries them); a `status >= 400` is a real result the
/// handler decides on.
#[starlark_module]
pub(crate) fn http_builtins(builder: &mut GlobalsBuilder) {
    fn get<'v>(
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        http_dispatch("GET", kwargs, eval)
    }

    fn post<'v>(
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        http_dispatch("POST", kwargs, eval)
    }

    fn put<'v>(
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        http_dispatch("PUT", kwargs, eval)
    }

    fn delete<'v>(
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        http_dispatch("DELETE", kwargs, eval)
    }

    fn patch<'v>(
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        http_dispatch("PATCH", kwargs, eval)
    }
}

/// Globals for effects: the base builtins plus the journaled impure capabilities
/// and the `http` namespace.
pub fn effect_globals() -> Globals {
    GlobalsBuilder::standard()
        .with(runtime_builtins)
        .with(effect_builtins)
        .with_namespace("http", http_builtins)
        .build()
}

/// The initial state for a command, lifted onto the given heap.
///
/// `initial` is a plain value, never a function: it sees no input, no clock and no
/// randomness, so it can only ever be a constant, and a module-level expression
/// already covers everything a `def initial()` could compute. It is handed to `fold`
/// as the frozen module global it is, with no copy, because `fold` returns the new
/// state rather than mutating this one. Absent `initial` yields `None`.
///
/// The shape is checked once at load by [`module_def_from_frozen`].
pub fn initial_state<'v>(
    frozen: &FrozenModule,
    module: &Module<'v>,
) -> starlark::Result<Value<'v>> {
    let Some(owned) = frozen.get_option("initial")? else {
        return Ok(Value::new_none());
    };
    Ok(thaw(&owned, module))
}

/// `fold` must return the new state. Falling off the end gives `None`, which is
/// almost always a mutate-and-forget-to-return.
///
/// `what` is [`EventDispatch::label`]'s naming, built only on the failing path, so a
/// map points at the entry rather than at `fold` as a whole.
pub fn check_fold_result(val: Value<'_>, what: &str) -> anyhow::Result<()> {
    if val.is_none() {
        anyhow::bail!(
            "{what} must return the updated state, not None; return the new state (e.g. `return dict(state, taken = True)`)"
        );
    }
    Ok(())
}

/// Lift a frozen module global into an evaluation heap, so it can be passed to
/// `call_handler` or handed to a handler as a value.
///
/// `add_reference` keeps the frozen data alive for the evaluator's lifetime.
/// `FrozenValue::to_value<'v>` is valid for any `'v`: frozen data is
/// permanent, the lifetime just scopes the view.
pub fn thaw<'v>(value: &OwnedFrozenValue, module: &Module<'v>) -> Value<'v> {
    // SAFETY: add_reference ensures the source heap outlives `module`; the
    // returned Value<'v> is a valid view of permanently-allocated frozen data.
    unsafe { value.owned_frozen_value(module.frozen_heap()).to_value() }
}

// ---------------------------------------------------------------------------
// Materialising inputs and events for the handlers
// ---------------------------------------------------------------------------

/// Build the `input` struct a command's handlers see: one field per declared
/// schema field, so handlers read `input.email`. An absent `optional` field is
/// `None`; an absent required field is an error (the runtime validates the body
/// against the schema first, so this only guards direct callers). Allocated on
/// `module`'s heap.
pub fn alloc_input<'v>(
    module: &Module<'v>,
    schema: &InputSchema,
    payload: &serde_json::Value,
) -> anyhow::Result<Value<'v>> {
    let obj = payload
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("command input must be a JSON object"))?;
    let mut fields: Vec<(&str, Value<'v>)> = Vec::with_capacity(schema.fields.len());
    for (name, kind) in &schema.fields {
        let value = match obj.get(name) {
            Some(value) => module.heap().alloc(value.clone()),
            None if kind.is_nullable() => Value::new_none(),
            None => anyhow::bail!("input is missing declared field `{name}`"),
        };
        fields.push((name.as_str(), value));
    }
    Ok(module.heap().alloc(AllocStruct(fields)))
}

/// Build the `event` struct passed to `fold` and to a projector/effect `handle`:
/// `event.type` and `event.data`.
///
/// `event.data` is a struct, read with dot access (`event.data.email`) exactly as a
/// command reads `input.email`. Both are host-built from a declared field schema, which
/// is what earns the dot: a field the definition does not declare is a load-time typo
/// rather than a `None` at runtime. Handler-built values (a command's folded state, a
/// `put()` row) stay dicts, because they have no declared shape to check against.
///
/// It is built from the definition's fields rather than from the stored payload, so a
/// field the payload omits reads as `None` instead of raising, matching how
/// [`alloc_input`] treats an absent optional. An unregistered event type has no field
/// list, so it falls back to whatever the payload carries.
///
/// When the definition declares any subject-scoped field, those values are wrapped as
/// opaque [`CipherHandle`]s (the stored value is ciphertext) rather than exposed as
/// strings, so plaintext never enters a handler.
pub fn alloc_event<'v>(
    module: &Module<'v>,
    event_type: &str,
    data: &serde_json::Value,
    event_def: Option<&EventDef>,
) -> Value<'v> {
    let heap = module.heap();
    let empty = serde_json::Map::new();
    let obj = data.as_object().unwrap_or(&empty);
    let pairs = match event_def {
        Some(def) => {
            let has_subject = def.fields.iter().any(|(_, m)| m.subject.is_some());
            let mut pairs = Vec::with_capacity(def.fields.len());
            for (name, _) in &def.fields {
                let value = match obj.get(name) {
                    Some(value) if has_subject => {
                        wrap_subject_value(heap, &def.fields, obj, name, value)
                    }
                    Some(value) => heap.alloc(value.clone()),
                    None => Value::new_none(),
                };
                pairs.push((name.clone(), value));
            }
            pairs
        }
        // No registered definition, so there is no shape to fill out: expose the
        // payload as it was stored.
        None => obj
            .iter()
            .map(|(key, value)| (key.clone(), heap.alloc(value.clone())))
            .collect(),
    };
    let fields: Vec<(&str, Value<'v>)> = vec![
        ("type", heap.alloc(event_type)),
        ("data", heap.alloc(AllocStruct(pairs))),
    ];
    heap.alloc(AllocStruct(fields))
}

/// Materialise a stored projector read-model row as a Starlark dict, wrapping every
/// subject-scoped field's ciphertext as an opaque [`CipherHandle`] so a handler sees a
/// handle, never plaintext or raw ciphertext.
///
/// A row stays a dict (unlike `event.data`, which is a struct) because `put()` takes a
/// dict, so `get()` then `put()` round-trips without a conversion in between. A null
/// (unset optional) subject field stays absent.
pub(crate) fn alloc_row_with_handles<'v>(
    heap: Heap<'v>,
    fields: &[(String, FieldMeta)],
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Value<'v> {
    let pairs: Vec<(String, Value<'v>)> = obj
        .iter()
        .map(|(key, value)| {
            (
                key.clone(),
                wrap_subject_value(heap, fields, obj, key, value),
            )
        })
        .collect();
    heap.alloc(AllocDict(pairs))
}

/// One stored field as a handler sees it: a subject-scoped field's ciphertext wrapped
/// as an opaque [`CipherHandle`], anything else as itself.
///
/// A ciphertext is always wrapped so it stays opaque and is preserved across a
/// read-modify-write. When the subject id is present the handle is fully scoped; when
/// it is absent or non-scalar (a corrupt or legacy row the write path could not
/// produce) the handle carries an empty subject id, so a `put` that rewrites the row
/// fails loudly in `enforce_subject_columns` (the id cannot be reconciled) rather than
/// silently nulling the stored ciphertext.
pub(crate) fn wrap_subject_value<'v>(
    heap: Heap<'v>,
    fields: &[(String, FieldMeta)],
    obj: &serde_json::Map<String, serde_json::Value>,
    name: &str,
    value: &serde_json::Value,
) -> Value<'v> {
    let subject_field = fields
        .iter()
        .find(|(n, _)| n == name)
        .and_then(|(_, meta)| meta.subject.as_ref());
    let (Some(subject_field), Some(ciphertext)) = (subject_field, value.as_str()) else {
        return heap.alloc(value.clone());
    };
    let subject_value = obj
        .get(subject_field)
        .and_then(scalar_to_string)
        .unwrap_or_default();
    heap.alloc(CipherHandle {
        ciphertext: ciphertext.to_owned(),
        field: name.to_owned(),
        subject_field: subject_field.clone(),
        subject_value,
    })
}

/// The scalar string form of a JSON value for a tag or a subject id: strings as-is,
/// numbers and bools by their canonical text. `None` for null or a composite.
pub(crate) fn scalar_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Number(number) => Some(number.to_string()),
        serde_json::Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Interpreting the result of `handle`
// ---------------------------------------------------------------------------

/// One event emitted by `handle`, lowered to plain data for the store.
pub struct EmittedEvent {
    pub event_type: String,
    pub data: serde_json::Value,
    pub tags: Vec<(String, Option<String>)>,
}

/// What `handle` decided.
pub enum HandleOutcome {
    /// `reject(...)`: state-dependent refusal, nothing is written.
    Reject(Rejection),
    /// `invalid_input(...)`: the input is malformed, nothing is written.
    InvalidInput(InvalidInput),
    /// A list of events to append (possibly empty = "nothing to do").
    Emit(Vec<EmittedEvent>),
}

/// Collect the events a command's `handle` (or a test `expect`) yields: one
/// constructed event, or a list of them (an empty list means "nothing to do",
/// valid for an idempotent command). Each item must come from calling an event
/// definition, so it is already validated and tagged. Returns `None` when `value`
/// is neither an event nor a list, so the caller can report its own error naming
/// the other valid outcomes.
pub fn events_from_value(value: Value<'_>) -> anyhow::Result<Option<Vec<ConstructedEvent>>> {
    if let Some(event) = value.downcast_ref::<ConstructedEvent>() {
        return Ok(Some(vec![event.clone()]));
    }
    let Some(list) = ListRef::from_value(value) else {
        return Ok(None);
    };
    let mut collected = Vec::with_capacity(list.len());
    for item in list.iter() {
        let event = item.downcast_ref::<ConstructedEvent>().ok_or_else(|| {
            anyhow::anyhow!(
                "a returned list must contain only events from an event definition, got {}",
                item.get_type()
            )
        })?;
        collected.push(event.clone());
    }
    Ok(Some(collected))
}

/// Check that `event` came from the definition the project registered for its type,
/// and not from a second `event(...)` built under the same name.
///
/// A definition constructed inside a function body never reaches the loader, so it
/// carries whatever schema the handler chose: fields the declared event does not
/// have (stored verbatim, and so never encrypted or erasable), a different set of
/// indexed fields, or forged store tags. The registry's definition is the one the
/// deploy-time checks ran against, so only events built from it may be emitted.
pub fn check_registered_definition(
    event: &ConstructedEvent,
    events: &EventDefs,
) -> anyhow::Result<()> {
    match events.get(&event.event_type) {
        Some(def) if def.id == event.def_id => Ok(()),
        Some(_) => anyhow::bail!(
            "event `{}` was built from a definition declared outside events/; load() the declared definition instead of calling event(type = \"{}\", ...) again",
            event.event_type,
            event.event_type
        ),
        None => anyhow::bail!(
            "event type `{}` is not declared in events/; define it there and load() it, so its schema (and any `subject` encryption) is applied",
            event.event_type
        ),
    }
}

/// Interpret the value `handle` returned: `reject(...)`, `invalid_input(...)`, or
/// the event(s) it returned, lowered to plain data for the store.
pub fn parse_handle_result(val: Value<'_>, events: &EventDefs) -> anyhow::Result<HandleOutcome> {
    if let Some(rejection) = val.downcast_ref::<Rejection>() {
        return Ok(HandleOutcome::Reject(rejection.clone()));
    }
    if let Some(invalid) = val.downcast_ref::<InvalidInput>() {
        return Ok(HandleOutcome::InvalidInput(invalid.clone()));
    }
    if let Some(constructed) = events_from_value(val)? {
        let lowered = constructed
            .iter()
            .map(|event| {
                check_registered_definition(event, events)?;
                Ok(EmittedEvent {
                    event_type: event.event_type.clone(),
                    data: serde_json::from_str(&event.data_json)
                        .with_context(|| format!("event `{}` payload", event.event_type))?,
                    tags: event.tags.clone(),
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        return Ok(HandleOutcome::Emit(lowered));
    }
    anyhow::bail!(
        "handle() must return an event, a list of events, reject(...) or invalid_input(...), got {}",
        val.get_type()
    );
}

/// Interpret the value a projector's `handle` returned: a list of `put(...)` /
/// `patch(...)` / `delete(...)` ops (possibly empty).
pub fn parse_entity_ops(val: Value<'_>) -> anyhow::Result<Vec<EntityOp>> {
    let list = ListRef::from_value(val).ok_or_else(|| {
        anyhow::anyhow!(
            "projector handle() must return a list of put(...)/patch(...)/delete(...) ops, got {}",
            val.get_type()
        )
    })?;
    let mut ops = Vec::with_capacity(list.len());
    for item in list.iter() {
        let op = item.downcast_ref::<EntityOp>().ok_or_else(|| {
            anyhow::anyhow!(
                "projector ops must be put(...), patch(...) or delete(...), got {}",
                item.get_type()
            )
        })?;
        ops.push(op.clone());
    }
    Ok(ops)
}

/// Check a row (or a set of patch changes) against an entity's declared fields.
///
/// Rejects unknown fields and a null on a non-`optional` field. When `full`
/// (a `put`, which replaces the whole row) every non-`optional` field must also
/// be present; a `patch` only touches the fields it names, so `full` is false.
fn validate_row(
    fields: &[(String, FieldMeta)],
    obj: &serde_json::Map<String, serde_json::Value>,
    full: bool,
) -> anyhow::Result<()> {
    for key in obj.keys() {
        if !fields.iter().any(|(name, _)| name == key) {
            anyhow::bail!("unknown field `{key}`");
        }
    }
    for (name, meta) in fields {
        match obj.get(name) {
            Some(value) if value.is_null() && !meta.is_nullable() => {
                anyhow::bail!("field `{name}` is not optional and cannot be null");
            }
            None if full && !meta.is_nullable() => {
                anyhow::bail!("required field `{name}` is missing");
            }
            _ => {}
        }
    }
    Ok(())
}

/// Enforce that every subject-scoped entity column receives a matching
/// [`CipherHandle`] (the ciphertext read from an event), not a plaintext value a
/// handler fabricated. A handle must carry the column's own name (its associated
/// data), the column's declared subject field, and a subject value that agrees with
/// the row's subject-id column. This is what keeps a read model from ever holding
/// plaintext, and keeps one subject's data from being filed under another's id. Runs
/// on the Starlark row before it is flattened to JSON, where handle provenance is
/// lost. `patch_key` is the patched row's key, used when the subject id is the entity
/// key and so not present in the changes.
fn enforce_subject_columns<'v>(
    def: &EntityDef,
    dict: &DictRef<'v>,
    patch_key: Option<&str>,
) -> anyhow::Result<()> {
    let entry = |name: &str| -> Option<Value<'v>> {
        dict.iter()
            .find_map(|(k, v)| (k.unpack_str() == Some(name)).then_some(v))
    };
    let string_of = |value: Value<'v>| -> Option<String> {
        value
            .to_json_value()
            .ok()
            .as_ref()
            .and_then(scalar_to_string)
    };
    // A handle may only be stored into a subject-scoped column. Anywhere else it would
    // be filed as opaque ciphertext the read API never decrypts and `reveal()` cannot
    // reach: a silent, permanent loss. Reject it.
    for (key, value) in dict.iter() {
        if value.downcast_ref::<CipherHandle>().is_some() {
            let name = key.unpack_str().unwrap_or_default();
            let subject_column = def
                .fields
                .iter()
                .any(|(field, meta)| field == name && meta.subject.is_some());
            if !subject_column {
                anyhow::bail!(
                    "column `{name}` is not subject-encrypted, so it cannot store an encrypted value read from an event"
                );
            }
        }
    }
    for (col, meta) in &def.fields {
        let Some(subject_field) = &meta.subject else {
            continue;
        };
        let Some(value) = entry(col) else {
            continue; // absent: `validate_row` enforces presence for a `put`
        };
        if value.is_none() {
            continue; // explicit null on an optional column
        }
        let handle = value.downcast_ref::<CipherHandle>().ok_or_else(|| {
            anyhow::anyhow!(
                "column `{col}` is subject-encrypted; store the value read from the event (an encrypted handle), not a {}",
                value.get_type()
            )
        })?;
        if handle.field != *col {
            anyhow::bail!(
                "column `{col}` received a value encrypted for field `{}`; a handle may only be stored into its own column",
                handle.field
            );
        }
        if handle.subject_field != *subject_field {
            anyhow::bail!(
                "column `{col}` is scoped to subject `{subject_field}`, but the value is scoped to `{}`",
                handle.subject_field
            );
        }
        let expected = match entry(subject_field).and_then(string_of) {
            Some(id) => id,
            None => match patch_key {
                Some(key) if def.key == *subject_field => key.to_owned(),
                _ => anyhow::bail!(
                    "column `{col}` needs its subject id `{subject_field}` present in the same row to store"
                ),
            },
        };
        if handle.subject_value != expected {
            anyhow::bail!(
                "column `{col}` holds data for `{subject_field}` = `{}`, but the row's `{subject_field}` is `{expected}`",
                handle.subject_value
            );
        }
    }
    Ok(())
}

/// Validate a JSON object against a set of declared fields: no unknown fields,
/// every non-`optional` field present and non-null, and each value well-typed for
/// its `FieldKind`. `what` names the subject in error messages (e.g. an event
/// type, or "input").
fn check_declared<'a, I>(
    what: &str,
    fields: I,
    obj: &serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<()>
where
    I: Iterator<Item = (&'a str, &'a FieldKind)> + Clone,
{
    for key in obj.keys() {
        if !fields.clone().any(|(name, _)| name == key) {
            anyhow::bail!("{what}: unknown field `{key}`");
        }
    }
    for (name, kind) in fields {
        match obj.get(name) {
            Some(value) => check_value(kind, value)
                .map_err(|err| anyhow::anyhow!("{what} field `{name}`: {err}"))?,
            None if kind.is_nullable() => {}
            None => anyhow::bail!("{what}: missing required field `{name}`"),
        }
    }
    Ok(())
}

/// Validate a command's input against its declared schema fields.
pub(crate) fn check_fields(
    what: &str,
    fields: &[(String, FieldKind)],
    obj: &serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<()> {
    check_declared(what, fields.iter().map(|(n, k)| (n.as_str(), k)), obj)
}

/// Validate a constructed event's payload against its declared fields. This is the
/// check the event constructor runs at emit time, so a malformed event fails where
/// it is built.
pub(crate) fn validate_event_payload(
    event_type: &str,
    fields: &[(String, FieldMeta)],
    obj: &serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<()> {
    check_declared(
        &format!("event `{event_type}`"),
        fields.iter().map(|(n, m)| (n.as_str(), &m.kind)),
        obj,
    )
}

/// Validate a command's request body against its input schema before the decision
/// cycle runs. A failure is the host-side equivalent of `invalid_input(...)`: the
/// body is malformed regardless of state, so it maps to HTTP 400.
pub fn validate_command_input(
    schema: &InputSchema,
    input: &serde_json::Value,
) -> anyhow::Result<()> {
    let obj = input
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("input: must be a JSON object"))?;
    check_fields("input", &schema.fields, obj)
}

/// Type-check one JSON value against a field kind. `optional` allows null and
/// otherwise checks the inner kind.
fn check_value(kind: &FieldKind, value: &serde_json::Value) -> anyhow::Result<()> {
    if value.is_null() {
        if kind.is_nullable() {
            return Ok(());
        }
        anyhow::bail!("must not be null");
    }
    match kind.base() {
        FieldKind::Text { max_length } => {
            let text = value.as_str().context("expected a string")?;
            if let Some(max) = max_length {
                let len = text.chars().count();
                if len > *max as usize {
                    anyhow::bail!("length {len} exceeds max_length {max}");
                }
            }
        }
        FieldKind::Uuid | FieldKind::Timestamp => {
            value.as_str().context("expected a string")?;
        }
        FieldKind::Money => {
            let text = value.as_str().context("expected a decimal string")?;
            if !is_decimal_string(text) {
                anyhow::bail!("`{text}` is not a decimal amount");
            }
        }
        FieldKind::OneOf(variants) => {
            let text = value.as_str().context("expected a string")?;
            if !variants.iter().any(|variant| variant == text) {
                anyhow::bail!("`{text}` is not one of {variants:?}");
            }
        }
        // Both integer kinds are stored in a SQLite INTEGER column, which is signed
        // 64-bit, so a value above `i64::MAX` is refused here rather than at the
        // projector. Reinterpreting the bits would round-trip but sort below zero,
        // silently breaking `ORDER BY` and the `key > ?` cursor for those rows; the
        // same reason `money` cannot key an ordered scan.
        FieldKind::I64 => {
            if !value.is_i64() {
                anyhow::bail!("expected an integer between {} and {}", i64::MIN, i64::MAX);
            }
        }
        FieldKind::U64 => {
            if !value.is_u64() {
                anyhow::bail!("expected a non-negative integer");
            }
            if !value.is_i64() {
                anyhow::bail!(
                    "{value} exceeds the largest storable integer ({})",
                    i64::MAX
                );
            }
        }
        FieldKind::Bool => {
            value.as_bool().context("expected a boolean")?;
        }
        FieldKind::Json => {}
        FieldKind::Optional(_) => unreachable!("base() strips Optional"),
    }
    Ok(())
}

/// A decimal money literal on the wire: an optional leading `-`, at least one
/// integer digit, and an optional fractional part. Exponents and thousands
/// separators are rejected so the wire form stays unambiguous.
fn is_decimal_string(text: &str) -> bool {
    let unsigned = text.strip_prefix('-').unwrap_or(text);
    let mut parts = unsigned.splitn(2, '.');
    let whole = parts.next().unwrap_or("");
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    digits(whole) && parts.next().is_none_or(digits)
}

/// Build the tags for a constructed event under automatic tagging: every `indexed`
/// field becomes a keyed tag whose value is its scalar stringified. A null (an
/// `optional` field left unset) and an `indexed = False` field contribute no tag.
///
/// Subject-scoped fields are left as their plaintext value here; the runtime's emit
/// lowering ([`crate::dispatch::build_event`]) replaces them with ciphertext (and
/// adds the global-key tag for a `unique` field), because only the runtime holds the
/// key store. In pure contexts (a projector fold, `kiln test`) that never reach the
/// store, the plaintext form is what is compared.
fn derive_tags(
    event_type: &str,
    fields: &[(String, FieldMeta)],
    obj: &serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<Vec<(String, Option<String>)>> {
    let mut tags = Vec::with_capacity(fields.len());
    for (name, meta) in fields {
        if !meta.indexed {
            continue;
        }
        let text = match obj.get(name) {
            None | Some(serde_json::Value::Null) => continue,
            Some(other) => scalar_to_string(other).ok_or_else(|| {
                anyhow::anyhow!(
                    "event `{event_type}`: indexed field `{name}` must be a scalar, got a {}",
                    json_kind(other)
                )
            })?,
        };
        tags.push((name.clone(), Some(text)));
    }
    Ok(tags)
}

/// Build a typed query clause's constraints from the fields a query-position event
/// call provided: `(field, value-as-string)` pairs, in the order given. A constraint
/// value must be a scalar; the deeper checks (the field exists, is indexed, is
/// well-typed, and a subject field's key is derivable) are deploy-time concerns in
/// [`crate::validate`], which reports them as errors rather than as an evaluation
/// failure. Fields not named are simply unconstrained (a subset match).
fn build_query_constraints(
    event_type: &str,
    provided: &serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<Vec<(String, String)>> {
    let mut constraints = Vec::with_capacity(provided.len());
    for (name, value) in provided {
        let text = scalar_to_string(value).ok_or_else(|| {
            anyhow::anyhow!("event `{event_type}`: filter `{name}` must be a scalar")
        })?;
        constraints.push((name.clone(), text));
    }
    // One predicate is one dispatch key: sorted so `f(a = 1, b = 2)` and
    // `f(b = 2, a = 1)` hash and compare alike rather than becoming two arms that
    // both fire for the same event. `serde_json::Map` is already ordered, but that
    // depends on a feature a transitive dependency could flip.
    constraints.sort();
    Ok(constraints)
}

fn json_kind(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Interpret a `query` result: a single event-definition call or `all_events()`, or a
/// list of them OR'd together. The specs lower to a
/// tephra `Query` (OR across items, AND within an item's tags).
pub fn parse_event_specs(val: Value<'_>) -> anyhow::Result<Vec<EventSpec>> {
    if let Some(spec) = val.downcast_ref::<EventSpec>() {
        return Ok(vec![spec.clone()]);
    }
    let list = ListRef::from_value(val).ok_or_else(|| {
        anyhow::anyhow!(
            "must be an event definition call (e.g. `order_placed(...)`), all_events(), or a list of them, got {}",
            val.get_type()
        )
    })?;
    if list.is_empty() {
        anyhow::bail!("empty list matches nothing; use all_events() to match every event");
    }
    let mut specs = Vec::with_capacity(list.len());
    for item in list.iter() {
        let spec = item.downcast_ref::<EventSpec>().ok_or_else(|| {
            anyhow::anyhow!(
                "list items must be event definition calls or all_events(), got {}",
                item.get_type()
            )
        })?;
        specs.push(spec.clone());
    }
    // `all_events()` subsumes everything, so combining it with other filters is
    // a mistake rather than a wider union.
    if specs.len() > 1 && specs.iter().any(|s| matches!(s, EventSpec::All)) {
        anyhow::bail!("all_events() can't be combined with other filters in a list");
    }
    Ok(specs)
}

/// One arm of a dispatch: the clause that selects its events, and the function to run
/// for them.
pub struct DispatchArm<'v> {
    /// What this arm subscribes to, or `None` for `all_events()`, which selects every
    /// event. Lowered to a tephra
    /// `QueryItem` once per run and then used as the match predicate, so the store's
    /// filter and the arm's filter are the same code on the same item.
    pub spec: Option<EventSpec>,
    pub func: Value<'v>,
}

/// How a handler that sees an event stream is dispatched: a dict of query clauses,
/// each with its own function. There is one form, for a command's `fold` and for a
/// projector's or effect's `handle` alike.
///
/// ```starlark
/// handle = {
///     order_placed(): on_placed,
///     order_placed(shop_id = 1): also_notify_shop_one,
///     shop_suspended(): on_suspended,
/// }
/// ```
///
/// **Every arm whose clause matches runs, in declaration order.** No arm can be
/// shadowed by an earlier one, so order fixes only the sequence of ops or journaled
/// calls (which determinism needs), never which arms run at all.
///
/// For a projector or effect the keys are also the subscription, so there is no second
/// list beside them to keep in step. `all_events()` is the clause that selects
/// everything, which is how one arm handles every event.
pub struct EventDispatch<'v> {
    arms: Vec<DispatchArm<'v>>,
}

impl<'v> EventDispatch<'v> {
    /// In declaration order, which a Starlark dict preserves.
    pub fn arms(&self) -> &[DispatchArm<'v>] {
        &self.arms
    }

    /// The clauses this subscribes to.
    pub fn specs(&self) -> Vec<EventSpec> {
        self.arms
            .iter()
            .map(|arm| arm.spec.clone().unwrap_or(EventSpec::All))
            .collect()
    }

    /// How to name a failure: the clause of the arm that failed, so an error points at
    /// the line that has to change.
    pub fn label(&self, global: &str, spec: Option<&EventSpec>) -> String {
        match spec {
            Some(spec) => format!("{global} entry for `{spec}`"),
            None => format!("{global} entry for `all_events()`"),
        }
    }
}

/// Interpret a `fold` or an event-driven `handle`: a dict mapping query clauses to
/// functions, one arm each.
///
/// Every arm is copied out of the dict so the borrow is released before the caller
/// runs any of them; holding a `DictRef` across a handler call would keep a `RefCell`
/// borrow alive through arbitrary Starlark.
///
/// Errors read as predicates so callers can prefix them with the global's name, the
/// way [`parse_event_specs`] is consumed.
pub fn parse_event_dispatch<'v>(val: Value<'v>) -> anyhow::Result<EventDispatch<'v>> {
    if val.get_type() == FUNCTION_TYPE {
        anyhow::bail!(
            "must be a dict mapping query clauses to functions, not a single function; write `{{order_placed(): on_placed}}`, or `{{all_events(): on_any}}` for one arm that runs for every event"
        );
    }
    let dict = DictRef::from_value(val).ok_or_else(|| {
        anyhow::anyhow!(
            "must be a dict mapping query clauses to functions, got {}",
            val.get_type()
        )
    })?;
    if dict.is_empty() {
        anyhow::bail!(
            "maps no clauses, so it would never run; give each event a clause key, or use `{{all_events(): ...}}`"
        );
    }
    let mut arms = Vec::with_capacity(dict.len());
    for (key, func) in dict.iter() {
        let spec = key_spec(key)?;
        if func.get_type() != FUNCTION_TYPE {
            let named = spec.clone().unwrap_or(EventSpec::All);
            anyhow::bail!(
                "entry for `{named}` must be a function, got {}",
                func.get_type()
            );
        }
        arms.push(DispatchArm { spec, func });
    }
    drop(dict);
    Ok(EventDispatch { arms })
}

/// The clause a dispatch key names, or `None` for `all_events()`, which selects every
/// event.
///
/// A key is always a call, never a bare definition, so one spelling covers the
/// unconstrained and the constrained arm and agrees with `query`, which has only ever
/// taken clauses. Rejecting the bare form here rather than dropping [`EventDef`]'s
/// hashability is deliberate: an unhashable key would fail while the dict was still
/// being built, with starlark's message instead of this one.
fn key_spec(key: Value<'_>) -> anyhow::Result<Option<EventSpec>> {
    if let Some(def) = key.downcast_ref::<EventDef>() {
        // Naming the event type rather than the binding, which a Starlark value does
        // not carry: the type locates the line, the example shows the fix.
        anyhow::bail!(
            "maps event `{}` through a bare definition; keys must be query clauses, so call it: `order_placed()`, or `order_placed(shop_id = 1)` to filter",
            def.event_type
        );
    }
    let Some(spec) = key.downcast_ref::<EventSpec>() else {
        anyhow::bail!(
            "keys must be query clauses from an events/ definition (e.g. `order_placed()`), got {}",
            key.get_type()
        );
    };
    Ok(match spec {
        // `all_events()` names no type, so it selects everything. In a map that is a
        // meaningful arm (one that runs for every event), unlike in a `query` list
        // where combining it with filters is a mistake.
        EventSpec::All => None,
        spec => Some(spec.clone()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields() -> Vec<(String, FieldMeta)> {
        vec![
            ("id".to_owned(), FieldMeta::plain(FieldKind::Uuid)),
            ("amount".to_owned(), FieldMeta::plain(FieldKind::Money)),
            (
                "kind".to_owned(),
                FieldMeta::plain(FieldKind::OneOf(vec!["a".to_owned(), "b".to_owned()])),
            ),
            (
                "note".to_owned(),
                FieldMeta::plain(FieldKind::Optional(Box::new(FieldKind::Text {
                    max_length: None,
                }))),
            ),
        ]
    }

    /// Evaluate `expr` against the base globals and return its repr, or the first
    /// `error:` line if it failed.
    fn eval_expr(expr: &str) -> String {
        let ast = parse_module("types.star", expr.to_owned()).unwrap();
        Module::with_temp_heap(|module| {
            let mut eval = Evaluator::new(&module);
            match eval.eval_module(ast, &globals()) {
                Ok(value) => value.to_repr(),
                Err(err) => format!("{err}")
                    .lines()
                    .find(|line| line.starts_with("error:"))
                    .unwrap_or("error: <none>")
                    .to_owned(),
            }
        })
    }

    fn field_kind(expr: &str) -> FieldKind {
        let ast = parse_module("types.star", format!("ft = {expr}")).unwrap();
        let frozen = eval_frozen(ast, &globals(), None, false).unwrap();
        let value = frozen.get("ft").unwrap();
        value
            .value()
            .downcast_ref::<FieldType>()
            .unwrap()
            .0
            .kind
            .clone()
    }

    /// The scalar field types shadow standard Starlark globals, which
    /// `GlobalsBuilder` currently permits silently. Upstream carries a
    /// "do not quietly ignore redefinitions" TODO, so pin the behaviour here: if a
    /// starlark upgrade ever starts rejecting or ignoring a redefinition, this
    /// fails in CI rather than at someone's deploy.
    #[test]
    fn the_shadowed_globals_are_kilns_own() {
        for name in ["str", "int", "bool"] {
            assert_eq!(
                globals().names().filter(|n| n.as_str() == name).count(),
                1,
                "`{name}` should be bound exactly once"
            );
        }
        // Kiln's meaning won, not the standard one: these are all field types.
        assert_eq!(field_kind("str()"), FieldKind::Text { max_length: None });
        assert_eq!(field_kind("int()"), FieldKind::I64);
        assert_eq!(field_kind("uint()"), FieldKind::U64);
        assert_eq!(field_kind("bool()"), FieldKind::Bool);
    }

    /// The rule that lets one name carry both meanings: a positional argument is
    /// Starlark's conversion, no positional argument is a field declaration.
    #[test]
    fn a_positional_argument_still_gets_the_standard_conversion() {
        assert_eq!(eval_expr("str(7)"), r#""7""#);
        assert_eq!(eval_expr("str([1, 'x'])"), r#""[1, \"x\"]""#);
        assert_eq!(eval_expr("bool(0)"), "False");
        assert_eq!(eval_expr("bool('x')"), "True");
        assert_eq!(eval_expr("int('16')"), "16");
        assert_eq!(eval_expr("int(3.9)"), "3");
        // Delegated to the standard `int`, so base prefixes and bignums survive.
        assert_eq!(eval_expr("int('16', 16)"), "22");
        assert_eq!(eval_expr("int('0x1f', 0)"), "31");
        assert_eq!(
            eval_expr("int(2 * 1208925819614629174706176)"),
            "2417851639229258349412352"
        );
    }

    #[test]
    fn field_options_reach_the_field_type() {
        assert_eq!(
            field_kind("str(max_length = 200)"),
            FieldKind::Text {
                max_length: Some(200)
            }
        );
        let ast = parse_module("types.star", "ft = int(indexed = False)".to_owned()).unwrap();
        let frozen = eval_frozen(ast, &globals(), None, false).unwrap();
        let value = frozen.get("ft").unwrap();
        assert!(!value.value().downcast_ref::<FieldType>().unwrap().0.indexed);
    }

    /// Mixing the two halves is a confusion, not a shorthand: the options would be
    /// dropped on the floor while the call looked like a declaration.
    #[test]
    fn a_conversion_may_not_also_carry_field_options() {
        for expr in [
            "str('a', max_length = 10)",
            "int(1, indexed = False)",
            "bool(1, subject = 'customer_id')",
        ] {
            let err = eval_expr(expr);
            assert!(
                err.contains("pass a value to convert it, or only field options"),
                "{expr} gave {err}"
            );
        }
        assert!(
            eval_expr("int(base = 16)").contains("has no value to convert"),
            "int(base = 16) should not look like a declaration"
        );
    }

    fn object(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        value.as_object().unwrap().clone()
    }

    /// The largest `i64`, plus one, as a JSON number. `serde_json` keeps it as a
    /// `u64`, which is exactly the shape that used to reach the read model.
    fn above_i64_max() -> serde_json::Value {
        serde_json::json!(i64::MAX as u64 + 1)
    }

    #[test]
    fn a_u64_within_the_storable_range_is_accepted() {
        assert!(check_value(&FieldKind::U64, &serde_json::json!(0u64)).is_ok());
        assert!(check_value(&FieldKind::U64, &serde_json::json!(i64::MAX as u64)).is_ok());
    }

    #[test]
    fn a_u64_above_i64_max_is_rejected_rather_than_wedging_the_projector() {
        let err = check_value(&FieldKind::U64, &above_i64_max()).unwrap_err();
        assert!(
            err.to_string().contains("exceeds the largest storable"),
            "{err}"
        );
    }

    #[test]
    fn a_negative_value_is_still_rejected_for_u64() {
        let err = check_value(&FieldKind::U64, &serde_json::json!(-1)).unwrap_err();
        assert!(err.to_string().contains("non-negative"), "{err}");
    }

    #[test]
    fn an_i64_field_rejects_a_positive_value_that_does_not_fit() {
        // The old check accepted anything `is_u64()`, so this value passed input
        // validation and then failed at the read model's `as_i64()`.
        let err = check_value(&FieldKind::I64, &above_i64_max()).unwrap_err();
        assert!(err.to_string().contains("expected an integer"), "{err}");
        assert!(check_value(&FieldKind::I64, &serde_json::json!(i64::MIN)).is_ok());
        assert!(check_value(&FieldKind::I64, &serde_json::json!(i64::MAX)).is_ok());
    }

    #[test]
    fn an_out_of_range_integer_is_rejected_through_command_input_validation() {
        let schema = InputSchema {
            fields: vec![("count".to_owned(), FieldKind::U64)],
        };
        let err = validate_command_input(&schema, &serde_json::json!({"count": above_i64_max()}))
            .unwrap_err();
        assert!(err.to_string().contains("count"), "{err}");
    }

    #[test]
    fn accepts_a_well_typed_payload() {
        let obj = object(serde_json::json!({"id": "u1", "amount": "10.50", "kind": "a"}));
        validate_event_payload("t", &fields(), &obj).unwrap();
    }

    #[test]
    fn rejects_a_missing_required_field() {
        let obj = object(serde_json::json!({"id": "u1", "amount": "1"}));
        let err = validate_event_payload("t", &fields(), &obj).unwrap_err();
        assert!(err.to_string().contains("kind"), "{err}");
    }

    #[test]
    fn rejects_a_non_decimal_money_value() {
        let obj = object(serde_json::json!({"id": "u1", "amount": "ten", "kind": "a"}));
        assert!(validate_event_payload("t", &fields(), &obj).is_err());
    }

    #[test]
    fn rejects_a_value_outside_one_of() {
        let obj = object(serde_json::json!({"id": "u1", "amount": "1", "kind": "z"}));
        assert!(validate_event_payload("t", &fields(), &obj).is_err());
    }

    #[test]
    fn rejects_an_unknown_field() {
        let obj = object(serde_json::json!({"id": "u1", "amount": "1", "kind": "a", "extra": 1}));
        let err = validate_event_payload("t", &fields(), &obj).unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
    }

    #[test]
    fn auto_tags_indexed_fields_and_skips_null_optionals() {
        let obj = object(serde_json::json!({"id": "u1", "amount": "1", "kind": "a"}));
        let tags = derive_tags("t", &fields(), &obj).unwrap();
        assert_eq!(
            tags,
            vec![
                ("id".to_owned(), Some("u1".to_owned())),
                ("amount".to_owned(), Some("1".to_owned())),
                ("kind".to_owned(), Some("a".to_owned())),
            ]
        );
    }

    #[test]
    fn indexed_false_field_produces_no_tag() {
        let fields = vec![
            ("id".to_owned(), FieldMeta::plain(FieldKind::Uuid)),
            (
                "note".to_owned(),
                FieldMeta {
                    kind: FieldKind::Text { max_length: None },
                    indexed: false,
                    subject: None,
                    unique: false,
                },
            ),
        ];
        let obj = object(serde_json::json!({"id": "u1", "note": "secret"}));
        let tags = derive_tags("t", &fields, &obj).unwrap();
        assert_eq!(tags, vec![("id".to_owned(), Some("u1".to_owned()))]);
    }

    fn entity(name: &str, indexes: Vec<IndexDef>) -> EntityDef {
        EntityDef {
            id: 1,
            name: name.to_owned(),
            key: "id".to_owned(),
            fields: vec![
                ("id".to_owned(), FieldMeta::plain(FieldKind::Uuid)),
                (
                    "email".to_owned(),
                    FieldMeta::plain(FieldKind::Text {
                        max_length: Some(200),
                    }),
                ),
            ],
            indexes,
        }
    }

    #[test]
    fn accepts_an_identifier_table_and_index_name() {
        let indexes = vec![IndexDef {
            name: "by_email".to_owned(),
            columns: vec!["email".to_owned()],
        }];
        entity("user_accounts2", indexes).validate().unwrap();
    }

    #[test]
    fn rejects_a_table_name_that_is_not_an_identifier() {
        for name in ["user accounts", "users; drop table users", "2users", ""] {
            let err = entity(name, Vec::new()).validate().unwrap_err();
            assert!(err.to_string().contains("table name"), "{name}: {err}");
        }
    }

    #[test]
    fn rejects_an_index_name_that_is_not_an_identifier() {
        let indexes = vec![IndexDef {
            name: "by email) ; drop table users --".to_owned(),
            columns: vec!["email".to_owned()],
        }];
        let err = entity("users", indexes).validate().unwrap_err();
        assert!(err.to_string().contains("index name"), "{err}");
    }

    #[test]
    fn rejects_a_cipher_handle_nested_in_an_emitted_payload() {
        // A handle serialises to its bare ciphertext, so one smuggled through a list
        // or dict would be stored as if it were plaintext, outside the key's reach.
        let src = r#"
ev = event(type = "t.derived", fields = {"blob": json(indexed = False)})

def in_list(handle):
    return ev(blob = [handle])

def in_dict(handle):
    return ev(blob = {"inner": handle})
"#;
        let ast = parse_module("t.star", src.to_owned()).unwrap();
        let frozen = eval_frozen(ast, &command_globals(), None, false).unwrap();
        Module::with_temp_heap(|module| {
            let handle = module.heap().alloc(CipherHandle {
                ciphertext: "Y2lwaGVydGV4dA".to_owned(),
                field: "email".to_owned(),
                subject_field: "user_id".to_owned(),
                subject_value: "u1".to_owned(),
            });
            for name in ["in_list", "in_dict"] {
                let func = frozen.get_option(name).unwrap().unwrap();
                let err = call_handler(&module, thaw(&func, &module), &[handle], 1_000_000)
                    .expect_err("a nested handle must not be re-emitted");
                assert!(err.to_string().contains("re-emitted"), "{name}: {err}");
            }
        });
    }

    #[test]
    fn put_and_patch_require_a_dict_before_flattening() {
        let src = r#"
users = entity(key = "id", fields = {"id": uuid()})

def bad_put():
    return put(users, bad_put)

def bad_patch():
    return patch(users, "u1", bad_patch)
"#;
        let ast = parse_module("t.star", src.to_owned()).unwrap();
        let frozen = eval_frozen(ast, &projector_globals(), None, false).unwrap();
        Module::with_temp_heap(|module| {
            for name in ["bad_put", "bad_patch"] {
                let func = frozen.get_option(name).unwrap().unwrap();
                let err = call_handler(&module, thaw(&func, &module), &[], 1_000_000)
                    .expect_err("a non-dict row must not reach the subject-column check");
                assert!(err.to_string().contains("must be a dict"), "{name}: {err}");
            }
        });
    }

    #[test]
    fn scan_rejects_a_negative_limit() {
        let src = "def f():\n    return scan(\"p\", \"e\", limit = -1)\n";
        let ast = parse_module("t.star", src.to_owned()).unwrap();
        let frozen = eval_frozen(ast, &effect_globals(), None, false).unwrap();
        Module::with_temp_heap(|module| {
            let func = frozen.get_option("f").unwrap().unwrap();
            let err = call_handler(&module, thaw(&func, &module), &[], 1_000_000)
                .expect_err("a negative limit must not be coerced to a one-row page");
            assert!(err.to_string().contains("negative"), "{err}");
        });
    }

    /// The registry a project would hold for the event definitions a module declares
    /// at top level, mirroring what `events/` modules feed the loader.
    fn registry(frozen: &FrozenModule) -> EventDefs {
        let names: Vec<String> = frozen
            .names()
            .filter_map(|name| name.to_value().unpack_str().map(str::to_owned))
            .collect();
        let mut defs = EventDefs::new();
        for name in names {
            let Ok(Some(owned)) = frozen.get_option(&name) else {
                continue;
            };
            if let Some(def) = owned.value().downcast_ref::<EventDef>() {
                defs.insert(def.event_type.clone(), def.clone());
            }
        }
        defs
    }

    #[test]
    fn a_malformed_event_payload_is_an_error_not_a_null() {
        let def = EventDef {
            id: next_event_def_id(),
            event_type: "t.broken".to_owned(),
            fields: Vec::new(),
        };
        let events = EventDefs::from([(def.event_type.clone(), def.clone())]);
        Module::with_temp_heap(|module| {
            let value = module.heap().alloc(ConstructedEvent {
                def_id: def.id,
                event_type: "t.broken".to_owned(),
                data_json: "{not json".to_owned(),
                tags: Vec::new(),
            });
            let err = match parse_handle_result(value, &events) {
                Ok(_) => panic!("a malformed payload must not be lowered to a null"),
                Err(err) => err,
            };
            assert!(err.to_string().contains("t.broken"), "{err}");
        });
    }

    /// A handler that builds its own `event(...)` under a declared type name would
    /// otherwise be lowered against the registry's schema, so any field the real
    /// definition does not declare rides into the log verbatim: never validated,
    /// never encrypted, never erasable.
    #[test]
    fn a_definition_built_inside_a_handler_cannot_be_emitted() {
        let src = r#"
ev = event(type = "t.happened", fields = {"id": uuid()})

def shadow(input, state):
    forged = event(type = "t.happened", fields = {"id": uuid(), "secret": str()})
    return forged(id = "u1", secret = "alice@example.com")

def unknown(input, state):
    forged = event(type = "t.undeclared", fields = {"id": uuid()})
    return forged(id = "u1")
"#;
        let ast = parse_module("t.star", src.to_owned()).unwrap();
        let frozen = eval_frozen(ast, &command_globals(), None, false).unwrap();
        let events = registry(&frozen);
        Module::with_temp_heap(|module| {
            let emit = |name: &str| {
                let func = frozen.get_option(name).unwrap().unwrap();
                let arg = module.heap().alloc(serde_json::Value::Null);
                let value =
                    call_handler(&module, thaw(&func, &module), &[arg, arg], 1_000_000).unwrap();
                parse_handle_result(value, &events)
            };

            let Err(err) = emit("shadow") else {
                panic!("a redeclared definition must not be emitted");
            };
            assert!(
                err.to_string().contains("declared outside events/"),
                "{err}"
            );

            let Err(err) = emit("unknown") else {
                panic!("an undeclared type must not be emitted");
            };
            assert!(err.to_string().contains("not declared in events/"), "{err}");
        });
    }

    /// The identity check must not catch a definition merely referred to by a second
    /// name: that is the same definition, and rejecting it would be a false positive.
    #[test]
    fn a_definition_referred_to_by_a_second_name_still_emits() {
        let src = r#"
ev = event(type = "t.happened", fields = {"id": uuid()})
alias = ev

def handle(input, state):
    return alias(id = "u1")
"#;
        let ast = parse_module("t.star", src.to_owned()).unwrap();
        let frozen = eval_frozen(ast, &command_globals(), None, false).unwrap();
        let events = registry(&frozen);
        Module::with_temp_heap(|module| {
            let func = frozen.get_option("handle").unwrap().unwrap();
            let arg = module.heap().alloc(serde_json::Value::Null);
            let value =
                call_handler(&module, thaw(&func, &module), &[arg, arg], 1_000_000).unwrap();
            let outcome = parse_handle_result(value, &events).unwrap();
            assert!(matches!(outcome, HandleOutcome::Emit(events) if events.len() == 1));
        });
    }

    #[test]
    fn money_decimal_forms() {
        assert!(is_decimal_string("0"));
        assert!(is_decimal_string("10.50"));
        assert!(is_decimal_string("-3"));
        assert!(!is_decimal_string("10."));
        assert!(!is_decimal_string(".5"));
        assert!(!is_decimal_string("1,000"));
        assert!(!is_decimal_string(""));
    }

    #[test]
    fn now_needs_a_handle_context() {
        let ast = parse_module("t.star", "def f():\n    return now()\n".to_owned()).unwrap();
        let frozen = eval_frozen(ast, &command_globals(), None, false).unwrap();
        Module::with_temp_heap(|module| {
            let func = frozen.get_option("f").unwrap().unwrap();
            // Without a context (as in `query`/`fold`), `now()` errors.
            assert!(call_handler(&module, thaw(&func, &module), &[], 1_000_000).is_err());
            // With one (as in `handle`), it returns the pinned instant.
            let ctx = HandleCtx {
                now: "2026-08-21T00:00:00Z".to_owned(),
            };
            let value =
                call_handler_with_ctx(&module, thaw(&func, &module), &[], 1_000_000, &ctx).unwrap();
            assert_eq!(value.unpack_str(), Some("2026-08-21T00:00:00Z"));
        });
    }

    #[test]
    fn handle_returns_events_directly_or_as_a_list() {
        let src = r#"
ev = event(type = "t.happened", fields = {"id": uuid()})

def one(input, state):
    return ev(id = "u1")

def many(input, state):
    return [ev(id = "u1"), ev(id = "u2")]

def nothing(input, state):
    return []

def bad(input, state):
    return 42
"#;
        let ast = parse_module("t.star", src.to_owned()).unwrap();
        let frozen = eval_frozen(ast, &command_globals(), None, false).unwrap();
        let defs = registry(&frozen);
        Module::with_temp_heap(|module| {
            let call = |name: &str| {
                let func = frozen.get_option(name).unwrap().unwrap();
                let arg = module.heap().alloc(serde_json::Value::Null);
                call_handler(&module, thaw(&func, &module), &[arg, arg], 1_000_000)
            };

            let one = parse_handle_result(call("one").unwrap(), &defs).unwrap();
            assert!(matches!(one, HandleOutcome::Emit(events) if events.len() == 1));

            let many = parse_handle_result(call("many").unwrap(), &defs).unwrap();
            assert!(matches!(many, HandleOutcome::Emit(events) if events.len() == 2));

            let nothing = parse_handle_result(call("nothing").unwrap(), &defs).unwrap();
            assert!(matches!(nothing, HandleOutcome::Emit(events) if events.is_empty()));

            let err = match parse_handle_result(call("bad").unwrap(), &defs) {
                Ok(_) => panic!("expected an error for a non-event return"),
                Err(err) => err,
            };
            assert!(err.to_string().contains("must return an event"), "{err}");
        });
    }

    // --- per-type dispatch -------------------------------------------------

    const TWO_EVENTS: &str = r#"
a = event(type = "t.a", fields = {"id": uuid()})
b = event(type = "t.b", fields = {"id": uuid()})
"#;

    /// Freeze a module and hand its globals to `f`, so a dispatch map is inspected the
    /// way the runtime sees it: after `Module::freeze`, not before. Every module kind
    /// evaluates in query mode, which is what turns a `a()` key into a clause.
    fn with_frozen<T>(src: &str, f: impl FnOnce(&FrozenModule) -> T) -> T {
        let ast = parse_module("d.star", src.to_owned()).unwrap();
        let frozen = eval_frozen(ast, &globals(), None, true).unwrap();
        f(&frozen)
    }

    /// The arms a dispatch declares, rendered as `type(constraint=value, ...)` in
    /// declaration order, which is the order they would run in.
    fn parse_global(src: &str, name: &str) -> anyhow::Result<Vec<String>> {
        with_frozen(src, |frozen| {
            let owned = frozen.get(name).unwrap();
            let dispatch = parse_event_dispatch(owned.value())?;
            Ok(dispatch.specs().iter().map(render_spec).collect())
        })
    }

    fn render_spec(spec: &EventSpec) -> String {
        match spec {
            EventSpec::All => "all_events()".to_owned(),
            EventSpec::Filter {
                event_type,
                constraints,
                ..
            } => {
                let inner: Vec<String> = constraints
                    .iter()
                    .map(|(field, value)| format!("{field}={value}"))
                    .collect();
                format!("{event_type}({})", inner.join(","))
            }
        }
    }

    /// A definition is not a dispatch key any more, but it stays hashable so that
    /// rejecting it is kiln's job: an unhashable key would fail while the dict was
    /// still being built, with starlark's `not hashable` instead of the message that
    /// says to call it.
    #[test]
    fn an_event_definition_can_key_a_dict() {
        let src = format!("{TWO_EVENTS}\nd = {{a: 1, b: 2}}\nd[b]");
        assert_eq!(eval_expr(&src), "2");
    }

    /// The one that catches a pointer-derived hash: freezing a dict carries each key's
    /// pre-freeze hash through unverified, so a hash that moved on freeze would leave
    /// the arm unreachable in the frozen module the runtime actually reads.
    #[test]
    fn a_clause_keyed_map_survives_the_module_freeze() {
        let src = format!("{TWO_EVENTS}\nfold = {{a(): lambda s, e: s, b(): lambda s, e: s}}");
        assert_eq!(parse_global(&src, "fold").unwrap(), ["t.a()", "t.b()"]);
    }

    /// A command's `fold` takes the same clause keys a `handle` does, constraints and
    /// all. Its module body evaluates in query mode for exactly this reason.
    #[test]
    fn a_fold_key_may_carry_a_constraint() {
        let src = r#"
a = event(type = "t.a", fields = {"id": uuid(), "shop": str()})
fold = {a(): lambda s, e: s, a(shop = "s"): lambda s, e: s}
"#;
        assert_eq!(parse_global(src, "fold").unwrap(), ["t.a()", "t.a(shop=s)"]);
    }

    /// Arms run in declaration order, so the parser must preserve it: a `HashMap` here
    /// would make a projector's op order and an effect's journal order depend on hash
    /// iteration.
    #[test]
    fn arms_keep_declaration_order() {
        let src = format!("{TWO_EVENTS}\nh = {{b(): lambda e: e, a(): lambda e: e}}");
        assert_eq!(parse_global(&src, "h").unwrap(), ["t.b()", "t.a()"]);
    }

    /// Several clauses may name one type: that is the fan-out the clause form exists
    /// for, and each is its own arm.
    #[test]
    fn one_type_may_carry_several_clauses() {
        let src = format!(
            "{TWO_EVENTS}\nh = {{a(): lambda e: e, a(id = \"x\"): lambda e: e, a(id = \"y\"): lambda e: e}}"
        );
        assert_eq!(
            parse_global(&src, "h").unwrap(),
            ["t.a()", "t.a(id=x)", "t.a(id=y)"]
        );
    }

    /// Constraints are sorted at construction, so one predicate is one key however the
    /// call was written. Starlark then rejects the repeat itself, which is a better
    /// answer than silently keeping one of two arms that would both have fired, and is
    /// why the parser needs no duplicate scan of its own.
    #[test]
    fn constraint_order_does_not_make_a_second_arm() {
        let src = r#"
a = event(type = "t.a", fields = {"id": uuid(), "shop": str()})
h = {a(id = "x", shop = "s"): lambda e: e, a(shop = "s", id = "x"): lambda e: e}
"#;
        let ast = parse_module("d.star", src.to_owned()).unwrap();
        let err = format!("{}", eval_frozen(ast, &globals(), None, true).unwrap_err());
        assert!(err.contains("Dictionary key repeated"), "got: {err}");
    }

    /// `all_events()` selects everything, so in a map it is an arm that runs for every
    /// event. It is what replaced the single-function form.
    #[test]
    fn all_events_is_a_catch_all_arm() {
        let src = format!("{TWO_EVENTS}\nh = {{all_events(): lambda e: e, a(): lambda e: e}}");
        assert_eq!(parse_global(&src, "h").unwrap(), ["all_events()", "t.a()"]);
    }

    /// Two definitions of the same type are distinct keys, matching the identity rule
    /// the append seam enforces.
    #[test]
    fn two_definitions_of_one_type_are_distinct_keys() {
        let src = r#"
a = event(type = "t.a", fields = {"id": uuid()})
b = event(type = "t.a", fields = {"id": uuid()})
d = {a: 1, b: 2}
len(d)
"#;
        assert_eq!(eval_expr(src), "2");
    }

    fn dispatch_err(src: &str) -> String {
        match parse_global(src, "fold") {
            Ok(arms) => panic!("expected a rejection, got {arms:?}"),
            Err(err) => format!("{err:#}"),
        }
    }

    /// The single-function form is gone, and the message has to say what replaced it,
    /// since "use a dict" alone does not tell an author how to keep handling everything.
    #[test]
    fn a_single_function_dispatch_is_rejected() {
        for src in [
            "def fold(state, event):\n    return state\n",
            "fold = lambda state, event: state\n",
        ] {
            let err = dispatch_err(src);
            assert!(err.contains("all_events()"), "{err}");
        }
    }

    #[test]
    fn a_dispatch_that_is_neither_function_nor_dict_is_rejected() {
        assert!(dispatch_err("fold = 7").contains("must be a dict mapping query clauses"));
    }

    #[test]
    fn an_empty_dispatch_map_is_rejected() {
        assert!(dispatch_err("fold = {}").contains("maps no clauses"));
    }

    #[test]
    fn a_non_clause_key_is_rejected() {
        let src = "fold = {\"t.a\": lambda s, e: s}";
        assert!(dispatch_err(src).contains("keys must be query clauses"));
    }

    /// The one spelling rule: a key is a call. A bare definition is the mistake this
    /// message exists to name, since it used to be the only accepted form in a `fold`.
    #[test]
    fn a_bare_definition_key_is_rejected() {
        let src = format!("{TWO_EVENTS}\nfold = {{a: lambda s, e: s}}");
        let err = dispatch_err(&src);
        assert!(err.contains("bare definition"), "{err}");
        assert!(err.contains("`t.a`"), "{err}");
    }

    #[test]
    fn a_non_function_arm_is_rejected() {
        let src = format!("{TWO_EVENTS}\nfold = {{a(): 7}}");
        let err = dispatch_err(&src);
        assert!(
            err.contains("entry for `t.a()` must be a function"),
            "{err}"
        );
    }

    /// The query-mode flip, from the other side: a command body that calls a definition
    /// gets a clause, so it can no longer build an event there. Nothing needs to, and
    /// pinning it keeps the flip from being reverted silently.
    #[test]
    fn a_module_body_call_yields_a_clause_not_an_event() {
        let src = format!("{TWO_EVENTS}\nx = a(id = \"11111111-1111-1111-1111-111111111111\")");
        let got = with_frozen(&src, |frozen| {
            frozen.get("x").unwrap().value().get_type().to_owned()
        });
        assert_eq!(got, "event_spec");
    }

    /// The literal-versus-function split `initial` rests on.
    #[test]
    fn a_literal_initial_is_data_and_a_function_is_not() {
        for (src, is_function) in [
            ("initial = {\"taken\": False}", false),
            ("initial = False", false),
            ("initial = lambda: 1", true),
            ("def initial():\n    return 1\n", true),
        ] {
            let got = with_frozen(src, |frozen| {
                frozen.get("initial").unwrap().value().get_type() == FUNCTION_TYPE
            });
            assert_eq!(got, is_function, "{src}");
        }
    }
}
