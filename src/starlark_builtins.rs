//! Starlark builtins for the runtime.
//!
//! Verified against starlark 0.14.2. The `pagable` feature gate note: `NoSerialize`
//! is behind `pagable`; drop it if you disable that feature.
//!
//! Module layout: each `.star` file is one command, projector or effect,
//! identified by its filename (slug-validated). Handlers (`query`, `initial`,
//! `fold`, `handle`) and schema globals (`input`, entities, `source`) are named
//! top-level values; there are no registration calls. Events are declared with
//! `event(...)` in `events/` and constructed by calling the definition
//! (`user_registered(...)`), which validates the payload and derives tags; a
//! command's `handle` returns `emit([...])` or `reject(...)`.

use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use allocative::Allocative;
use anyhow::Context;
use starlark::any::ProvidesStaticType;
use starlark::collections::SmallMap;
use starlark::environment::{FrozenModule, Globals, GlobalsBuilder, Module};
use starlark::eval::{Arguments, Evaluator, FileLoader};
use starlark::syntax::{AstModule, Dialect};
use starlark::values::dict::DictRef;
use starlark::values::list::{ListRef, UnpackList};
use starlark::values::none::NoneType;
use starlark::values::structs::AllocStruct;
use starlark::values::{
    NoSerialize, OwnedFrozenValue, StarlarkValue, Value, ValueLike, starlark_value,
};
use starlark::{starlark_module, starlark_simple_value};

use crate::context::{EffectCtx, EffectHost, HandleCtx, ProjectorCtx};

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
            FieldKind::I64 | FieldKind::U64 | FieldKind::Money => "INTEGER",
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

#[derive(Debug, Clone, ProvidesStaticType, NoSerialize, Allocative)]
pub struct FieldType(pub FieldKind);

impl fmt::Display for FieldType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

#[starlark_value(type = "field_type")]
impl<'v> StarlarkValue<'v> for FieldType {}
starlark_simple_value!(FieldType);

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

#[derive(Debug, Clone, ProvidesStaticType, NoSerialize, Allocative)]
pub struct EntityDef {
    pub id: u64,
    /// The table name. An explicit `name=` override, otherwise empty until the
    /// host fills it from the global binding at load.
    pub name: String,
    pub key: String,
    pub fields: Vec<(String, FieldKind)>,
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
            .map(|(name, kind)| {
                let null = if kind.is_nullable() { "" } else { " NOT NULL" };
                let pk = if *name == self.key {
                    " PRIMARY KEY"
                } else {
                    ""
                };
                format!("  {} {}{}{}", name, kind.sql_type(), pk, null)
            })
            .collect();
        format!(
            "CREATE TABLE IF NOT EXISTS {} (\n{}\n)",
            self.name,
            cols.join(",\n")
        )
    }

    pub fn create_index_sql(&self) -> Vec<String> {
        self.indexes
            .iter()
            .map(|ix| {
                format!(
                    "CREATE INDEX IF NOT EXISTS {}_{} ON {} ({})",
                    self.name,
                    ix.name,
                    self.name,
                    ix.columns.join(", ")
                )
            })
            .collect()
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if !self.fields.iter().any(|(n, _)| *n == self.key) {
            anyhow::bail!(
                "entity `{}`: key `{}` is not a declared field",
                self.name,
                self.key
            );
        }
        for ix in &self.indexes {
            for col in &ix.columns {
                if !self.fields.iter().any(|(n, _)| n == col) {
                    anyhow::bail!(
                        "entity `{}`: index `{}` references unknown field `{}`",
                        self.name,
                        ix.name,
                        col
                    );
                }
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
    pub event_type: String,
    pub fields: Vec<(String, FieldKind)>,
    /// Fields that are indexed as key-value tags in the event store.
    pub tags: Vec<String>,
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
    /// fields and deriving tags from the declared tag fields. Handlers pass the
    /// result to `emit([...])`. Named arguments only, so the call reads like the
    /// schema it checks against.
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let heap = eval.heap();
        args.no_positional_args(heap)?;
        let named = args.names_map()?;

        let mut payload = serde_json::Map::with_capacity(named.len());
        for (name, value) in &named {
            let json = value.to_json_value().map_err(|err| {
                anyhow::anyhow!(
                    "event `{}` field `{}` must be JSON-serialisable: {err}",
                    self.event_type,
                    name.as_str()
                )
            })?;
            payload.insert(name.as_str().to_owned(), json);
        }

        validate_event_payload(&self.event_type, &self.fields, &payload)?;
        let tags = derive_tags(&self.event_type, &self.tags, &payload)?;
        Ok(heap.alloc(ConstructedEvent {
            event_type: self.event_type.clone(),
            data_json: serde_json::Value::Object(payload).to_string(),
            tags,
        }))
    }
}
starlark_simple_value!(EventDef);

// ---------------------------------------------------------------------------
// Constructed event + emit outcome
// ---------------------------------------------------------------------------

/// One concrete event produced by calling an event definition. The payload is
/// held as its JSON wire form (a validated, serialised object) and the tags are
/// already derived, so the dispatch layer appends it without re-validating.
#[derive(Debug, Clone, ProvidesStaticType, NoSerialize, Allocative)]
pub struct ConstructedEvent {
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

/// What a command's `handle` returns to append events: the ordered events an
/// `emit(...)` call collected. A sibling of `reject(...)`, the other terminal
/// outcome.
#[derive(Debug, Clone, ProvidesStaticType, NoSerialize, Allocative)]
pub struct EmitOutcome {
    pub events: Vec<ConstructedEvent>,
}

impl fmt::Display for EmitOutcome {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "emit({} events)", self.events.len())
    }
}

#[starlark_value(type = "emit")]
impl<'v> StarlarkValue<'v> for EmitOutcome {}
starlark_simple_value!(EmitOutcome);

// ---------------------------------------------------------------------------
// Query spec (commands): the DCB consistency boundary
// ---------------------------------------------------------------------------

/// The consistency boundary a command's `query` (or a projector's subscription)
/// reads over, lowered to a tephra `Query`.
#[derive(Debug, Clone, ProvidesStaticType, NoSerialize, Allocative)]
pub enum EventSpec {
    /// Every event. Lowers to `Query::All`, a full scan that bypasses the index.
    All,
    /// A single query item: the event type is one of `types` (empty = any type)
    /// AND the event carries all of `tags`.
    Filter {
        types: Vec<String>,
        /// `(key, Some(value))` for keyed tags, `(key, None)` for bare tags.
        /// Bare `"premium"` and keyed `"premium:"` are distinct; they never collide.
        tags: Vec<(String, Option<String>)>,
    },
}

impl fmt::Display for EventSpec {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            EventSpec::All => write!(f, "all_events()"),
            EventSpec::Filter { types, .. } => write!(f, "events({types:?})"),
        }
    }
}

#[starlark_value(type = "event_spec")]
impl<'v> StarlarkValue<'v> for EventSpec {}
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
            EntityOpKind::Patch { key, .. } => write!(f, "patch(#{}, {})", self.entity_id, key),
            EntityOpKind::Delete(key) => write!(f, "delete(#{}, {})", self.entity_id, key),
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
    /// The directory (relative to the project root) this kind is authored under.
    pub fn dir(self) -> &'static str {
        match self {
            ModuleKind::Command => "commands",
            ModuleKind::Projector => "projectors",
            ModuleKind::Effect => "effects",
        }
    }

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

    fn text(#[starlark(require = named)] max_length: Option<u32>) -> anyhow::Result<FieldType> {
        Ok(FieldType(FieldKind::Text { max_length }))
    }

    fn i64_() -> anyhow::Result<FieldType> {
        Ok(FieldType(FieldKind::I64))
    }

    fn u64_() -> anyhow::Result<FieldType> {
        Ok(FieldType(FieldKind::U64))
    }

    fn boolean() -> anyhow::Result<FieldType> {
        Ok(FieldType(FieldKind::Bool))
    }

    fn uuid() -> anyhow::Result<FieldType> {
        Ok(FieldType(FieldKind::Uuid))
    }

    fn timestamp() -> anyhow::Result<FieldType> {
        Ok(FieldType(FieldKind::Timestamp))
    }

    fn money() -> anyhow::Result<FieldType> {
        Ok(FieldType(FieldKind::Money))
    }

    fn json() -> anyhow::Result<FieldType> {
        Ok(FieldType(FieldKind::Json))
    }

    /// Named `one_of` rather than `enum` because starlark-rust's extended
    /// dialect already defines `enum`.
    fn one_of(variants: UnpackList<String>) -> anyhow::Result<FieldType> {
        if variants.items.is_empty() {
            anyhow::bail!("one_of() needs at least one variant");
        }
        Ok(FieldType(FieldKind::OneOf(variants.items)))
    }

    fn optional(inner: &FieldType) -> anyhow::Result<FieldType> {
        if matches!(inner.0, FieldKind::Optional(_)) {
            anyhow::bail!("optional(optional(...)) is not meaningful");
        }
        Ok(FieldType(FieldKind::Optional(Box::new(inner.0.clone()))))
    }

    // --- schema ------------------------------------------------------------

    fn schema<'v>(
        #[starlark(kwargs)] fields: SmallMap<String, Value<'v>>,
    ) -> anyhow::Result<InputSchema> {
        let mut out = Vec::with_capacity(fields.len());
        for (name, value) in fields {
            let ft = value.downcast_ref::<FieldType>().ok_or_else(|| {
                anyhow::anyhow!(
                    "schema field `{}` must be a field type, got {}",
                    name,
                    value.get_type()
                )
            })?;
            out.push((name, ft.0.clone()));
        }
        Ok(InputSchema { fields: out })
    }

    // --- entities ----------------------------------------------------------

    fn index(
        #[starlark(require = pos)] name: String,
        #[starlark(require = pos)] columns: UnpackList<String>,
    ) -> anyhow::Result<IndexDef> {
        if columns.items.is_empty() {
            anyhow::bail!("index `{}` needs at least one column", name);
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

    fn event<'v>(
        #[starlark(require = named)] r#type: String,
        #[starlark(require = named)] fields: SmallMap<String, Value<'v>>,
        #[starlark(require = named)] tags: UnpackList<String>,
    ) -> anyhow::Result<EventDef> {
        let mut field_defs = Vec::with_capacity(fields.len());
        for (name, value) in fields {
            let ft = value.downcast_ref::<FieldType>().ok_or_else(|| {
                anyhow::anyhow!("event `{}` field `{}` must be a field type", r#type, name)
            })?;
            field_defs.push((name, ft.0.clone()));
        }
        for tag in &tags.items {
            if !field_defs.iter().any(|(n, _)| n == tag) {
                anyhow::bail!("event `{}`: tag `{}` is not a declared field", r#type, tag);
            }
        }
        Ok(EventDef {
            event_type: r#type,
            fields: field_defs,
            tags: tags.items,
        })
    }

    // --- DCB query ---------------------------------------------------------

    /// A query over specific event types and/or tags. Both are optional, but at
    /// least one must be given; use `all_events()` to match every event.
    ///
    /// `tags` accepts a dict or a list.
    ///
    /// Dict form, `None` value means bare tag:
    ///   `{"task_id": input.task_id, "premium": None}`
    ///
    /// List form, split on first `:`, no colon means bare:
    ///   `["task_id:abc-123", "premium"]`
    fn events<'v>(
        #[starlark(require = named)] types: Option<UnpackList<String>>,
        #[starlark(require = named)] tags: Option<Value<'v>>,
    ) -> anyhow::Result<EventSpec> {
        let types = types.map(|t| t.items).unwrap_or_default();
        let tags = match tags {
            Some(value) => parse_tags(value)?,
            None => Vec::new(),
        };
        if types.is_empty() && tags.is_empty() {
            anyhow::bail!(
                "events() needs at least a type or a tag; use all_events() to match every event"
            );
        }
        Ok(EventSpec::Filter { types, tags })
    }

    /// The catch-all query: every event, regardless of type or tags. Lowers to
    /// tephra's `Query::All` (a full scan that bypasses the index). Projectors
    /// that build a global read model use this; commands rarely do.
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

    /// Collect the events a command's `handle` appends. Accepts one constructed
    /// event or a list of them (`emit(user_registered(...))` or `emit([a, b])`);
    /// an empty list means "nothing to do", which is valid for idempotent
    /// commands. Each item must come from calling an event definition, so it is
    /// already validated and tagged.
    fn emit<'v>(#[starlark(require = pos)] events: Value<'v>) -> anyhow::Result<EmitOutcome> {
        let mut collected = Vec::new();
        if let Some(event) = events.downcast_ref::<ConstructedEvent>() {
            collected.push(event.clone());
        } else if let Some(list) = ListRef::from_value(events) {
            for item in list.iter() {
                let event = item.downcast_ref::<ConstructedEvent>().ok_or_else(|| {
                    anyhow::anyhow!(
                        "emit() items must be events from an event definition, got {}",
                        item.get_type()
                    )
                })?;
                collected.push(event.clone());
            }
        } else {
            anyhow::bail!(
                "emit() expects an event or a list of events, got {}",
                events.get_type()
            );
        }
        Ok(EmitOutcome { events: collected })
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
        let def = entity.downcast_ref::<EntityDef>().ok_or_else(|| {
            anyhow::anyhow!(
                "put() first argument must be an entity(...), got {}",
                entity.get_type()
            )
        })?;
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
        let def = entity.downcast_ref::<EntityDef>().ok_or_else(|| {
            anyhow::anyhow!(
                "patch() first argument must be an entity(...), got {}",
                entity.get_type()
            )
        })?;
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
        let def = entity.downcast_ref::<EntityDef>().ok_or_else(|| {
            anyhow::anyhow!(
                "delete() first argument must be an entity(...), got {}",
                entity.get_type()
            )
        })?;
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
        .ok_or_else(|| anyhow::anyhow!("{}: cannot derive a name from the filename", filename))?;
    validate_slug(stem).map_err(|err| anyhow::anyhow!("{}: {}", filename, err))?;
    Ok(stem.to_owned())
}

fn validate_slug(s: &str) -> anyhow::Result<()> {
    if s.is_empty() {
        anyhow::bail!("name is empty");
    }
    let bytes = s.as_bytes();
    if bytes[0] == b'-' || bytes[bytes.len() - 1] == b'-' {
        anyhow::bail!("name `{s}` must not start or end with a hyphen");
    }
    let mut prev_hyphen = false;
    for &b in bytes {
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
) -> starlark::Result<FrozenModule> {
    Module::with_temp_heap(|module| {
        {
            let mut eval = Evaluator::new(&module);
            if let Some(loader) = loader {
                eval.set_loader(loader);
            }
            eval.set_max_tick_count(LOAD_MAX_TICKS)?;
            eval.eval_module(ast, globals)?;
        }
        starlark::Result::Ok(module.freeze()?)
    })
}

/// Read a frozen module's declarations into a `ModuleDef` for the given kind.
///
/// Handlers (`query`, `initial`, `fold`, `handle`) and schema globals (a
/// command's `input`; a projector's entities and `source`; an effect's
/// `source`) are named top-level values read off the frozen module; there are no
/// registration calls. `handle` is always required; the rest depends on the
/// kind. Failing here means failing at load rather than on the first request.
pub fn module_def_from_frozen(
    kind: ModuleKind,
    name: String,
    filename: &str,
    module: &FrozenModule,
) -> anyhow::Result<ModuleDef> {
    // `query`, `initial` and `fold` are optional (a command with no invariants
    // omits them and the host calls `handle(input, None)` directly), but
    // `handle` is always required.
    if module.get_option("handle")?.is_none() {
        anyhow::bail!("{}: missing required `handle` function", filename);
    }
    Ok(match kind {
        ModuleKind::Command => ModuleDef::Command {
            name,
            input: read_schema(module, filename)?,
        },
        ModuleKind::Projector => {
            let (entities, sources) = read_projector(module, filename)?;
            ModuleDef::Projector {
                name,
                entities,
                sources,
            }
        }
        ModuleKind::Effect => ModuleDef::Effect {
            name,
            sources: read_effect(module, filename)?,
        },
    })
}

/// Parse, evaluate and read a single standalone file (no `load()` resolution).
/// The project loader wires imports itself; this is for one-off use and tests.
/// Whether the file is a command, projector or effect is decided by the caller
/// (directory convention) and passed in as `kind`.
pub fn load_script(
    filename: &str,
    src: String,
    kind: ModuleKind,
) -> starlark::Result<LoadedModule> {
    let name = module_name_from_path(filename)?;
    let source_hash = crate::hash::sha256_hex(src.as_bytes());
    let ast = parse_module(filename, src)?;
    let module = eval_frozen(ast, &globals_for(kind), None)?;
    let def = module_def_from_frozen(kind, name, filename, &module)?;
    Ok(LoadedModule {
        def,
        module,
        source_hash,
    })
}

/// Read the required `input = schema(...)` global off a command module.
fn read_schema(module: &FrozenModule, filename: &str) -> anyhow::Result<InputSchema> {
    let Some(owned) = module.get_option("input")? else {
        anyhow::bail!("{}: command must define `input = schema(...)`", filename);
    };
    let val = owned.value();
    let schema = val.downcast_ref::<InputSchema>().ok_or_else(|| {
        anyhow::anyhow!(
            "{}: `input` must be a schema(...), got {}",
            filename,
            val.get_type()
        )
    })?;
    Ok(schema.clone())
}

/// Collect a projector's entities and read its `source` subscription.
///
/// Entities are gathered implicitly: every global bound to an `entity(...)` is a
/// table, named after its binding unless it carries an explicit `name=`. There
/// is no `entities = [...]` list to keep in sync.
fn read_projector(
    module: &FrozenModule,
    filename: &str,
) -> anyhow::Result<(Vec<EntityDef>, Vec<EventSpec>)> {
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
        resolved
            .validate()
            .map_err(|err| anyhow::anyhow!("{}: {}", filename, err))?;
        entities.push(resolved);
    }
    // Deterministic DDL/output order regardless of how `names()` iterates.
    entities.sort_by(|a, b| a.name.cmp(&b.name));
    if entities.is_empty() {
        anyhow::bail!(
            "{}: projector defines no entities; assign one with `name = entity(...)`",
            filename
        );
    }

    let Some(owned) = module.get_option("source")? else {
        anyhow::bail!(
            "{}: projector must define `source = ...` (events(...), all_events(), or a list)",
            filename
        );
    };
    let sources = parse_event_specs(owned.value())
        .map_err(|err| anyhow::anyhow!("{}: `source` {}", filename, err))?;
    Ok((entities, sources))
}

/// Read an effect's `source` subscription. An effect has no entities: it reacts
/// to events and performs side effects (the durable-execution runtime lands in a
/// later phase). Its shape is a `source` plus `handle`; validating `source` here
/// means a broken subscription fails at load rather than at first dispatch.
fn read_effect(module: &FrozenModule, filename: &str) -> anyhow::Result<Vec<EventSpec>> {
    let Some(owned) = module.get_option("source")? else {
        anyhow::bail!(
            "{}: effect must define `source = ...` (events(...), all_events(), or a list)",
            filename
        );
    };
    parse_event_specs(owned.value())
        .map_err(|err| anyhow::anyhow!("{}: `source` {}", filename, err))
}

// ---------------------------------------------------------------------------
// Dispatch sketch
// ---------------------------------------------------------------------------

/// Call a handler. Synchronous and non-yielding, so this belongs on
/// `spawn_blocking`, never on a Tokio worker.
pub fn call_handler<'v>(
    module: &Module<'v>,
    func: Value<'v>,
    args: &[Value<'v>],
    max_instructions: u64,
) -> starlark::Result<Value<'v>> {
    let mut eval = Evaluator::new(module);
    eval.set_max_tick_count(max_instructions)?;
    eval.eval_function(func, args, &[])
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
    let mut eval = Evaluator::new(module);
    eval.set_max_tick_count(max_instructions)?;
    eval.extra = Some(ctx);
    eval.eval_function(func, args, &[])
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
    let mut eval = Evaluator::new(module);
    eval.set_max_tick_count(max_instructions)?;
    eval.extra = Some(ctx);
    eval.eval_function(func, args, &[])
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
    let mut eval = Evaluator::new(module);
    eval.set_max_tick_count(max_instructions)?;
    eval.extra = Some(ctx);
    eval.eval_function(func, args, &[])
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
pub fn command_builtins(builder: &mut GlobalsBuilder) {
    fn now(eval: &mut Evaluator) -> anyhow::Result<String> {
        match eval
            .extra
            .and_then(|extra| extra.downcast_ref::<HandleCtx>())
        {
            Some(ctx) => Ok(ctx.now.clone()),
            None => anyhow::bail!("now() is only available in handle()"),
        }
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
pub fn projector_builtins(builder: &mut GlobalsBuilder) {
    fn get<'v>(
        #[starlark(require = pos)] entity: Value<'v>,
        #[starlark(require = pos)] key: String,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let def = entity.downcast_ref::<EntityDef>().ok_or_else(|| {
            anyhow::anyhow!(
                "get() first argument must be an entity(...), got {}",
                entity.get_type()
            )
        })?;
        let ctx = eval
            .extra
            .and_then(|extra| extra.downcast_ref::<ProjectorCtx>())
            .ok_or_else(|| anyhow::anyhow!("get() is only available in a projector's handle()"))?;
        match ctx.reader.get(def.id, &key)? {
            Some(row) => Ok(eval.heap().alloc(row)),
            None => Ok(Value::new_none()),
        }
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
pub fn effect_builtins(builder: &mut GlobalsBuilder) {
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
        let limit = limit.map(|n| n.max(0) as usize);
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
pub fn http_builtins(builder: &mut GlobalsBuilder) {
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

/// Globals for effects: the base builtins plus the (currently stubbed) impure
/// capabilities and the `http` namespace.
pub fn effect_globals() -> Globals {
    GlobalsBuilder::standard()
        .with(runtime_builtins)
        .with(effect_builtins)
        .with_namespace("http", http_builtins)
        .build()
}

/// The globals a module of `kind` is evaluated against. Commands get `now()`,
/// effects get the impure capabilities, projectors stay pure.
pub fn globals_for(kind: ModuleKind) -> Globals {
    match kind {
        ModuleKind::Command => command_globals(),
        ModuleKind::Effect => effect_globals(),
        ModuleKind::Projector => projector_globals(),
    }
}

/// Allocate the initial state for a command on the given heap.
///
/// Reads the optional `initial` global by name. It may be a `def initial()`,
/// which we call (it returns a fresh mutable value each time) or a literal,
/// which is frozen, so we round-trip it through JSON to hand `fold` a fresh,
/// mutable copy. Absent `initial` yields `None`.
pub fn initial_state<'v>(
    frozen: &FrozenModule,
    module: &Module<'v>,
) -> starlark::Result<Value<'v>> {
    let Some(owned) = frozen.get_option("initial")? else {
        return Ok(Value::new_none());
    };
    let val = thaw(&owned, module);
    match val.to_json_value() {
        Ok(json) => Ok(module.heap().alloc(json)),
        Err(_) => call_handler(module, val, &[], 1_000_000),
    }
}

/// `fold` must return the updated state. Falling off the end gives `None`,
/// which is almost always a bug.
pub fn check_fold_result(val: Value<'_>) -> anyhow::Result<()> {
    if val.is_none() {
        anyhow::bail!(
            "fold() must return the updated state, not None. Did you forget `return state`?"
        );
    }
    Ok(())
}

/// `handle` must return `emit([...])` (possibly empty) or `reject(...)`. An
/// empty `emit` means "nothing to do" and is valid for idempotent commands.
pub fn check_handle_result(val: Value<'_>) -> anyhow::Result<()> {
    if val.downcast_ref::<Rejection>().is_some()
        || val.downcast_ref::<InvalidInput>().is_some()
        || val.downcast_ref::<EmitOutcome>().is_some()
    {
        return Ok(());
    }
    anyhow::bail!(
        "handle() must return emit([...]), reject(...) or invalid_input(...), got {}",
        val.get_type()
    );
}

/// Lift a frozen handler value into an evaluation heap so it can be passed to
/// `call_handler`.
///
/// `add_reference` keeps the frozen data alive for the evaluator's lifetime.
/// `FrozenValue::to_value<'v>` is valid for any `'v`: frozen data is
/// permanent, the lifetime just scopes the view.
pub fn thaw<'v>(func: &OwnedFrozenValue, module: &Module<'v>) -> Value<'v> {
    // SAFETY: add_reference ensures the source heap outlives `module`; the
    // returned Value<'v> is a valid view of permanently-allocated frozen data.
    unsafe { func.owned_frozen_value(module.frozen_heap()).to_value() }
}

// ---------------------------------------------------------------------------
// Tag parsing: shared by the `events` builtin and emitted events
// ---------------------------------------------------------------------------

/// Parse a starlark tags value into `(key, Option<value>)` pairs.
///
/// Dict form, a `None` value means a bare tag:
///   `{"task_id": input.task_id, "premium": None}`
/// List form, split on the first `:`, no colon means bare:
///   `["task_id:abc-123", "premium"]`
pub fn parse_tags(tags: Value<'_>) -> anyhow::Result<Vec<(String, Option<String>)>> {
    if let Some(dict) = DictRef::from_value(tags) {
        let mut pairs = Vec::with_capacity(dict.len());
        for (k, v) in dict.iter() {
            let key = k
                .unpack_str()
                .ok_or_else(|| anyhow::anyhow!("tag keys must be strings, got {}", k.get_type()))?;
            let val = if v.is_none() {
                None
            } else {
                Some(match v.unpack_str() {
                    Some(s) => s.to_owned(),
                    None => v.to_string(),
                })
            };
            pairs.push((key.to_owned(), val));
        }
        Ok(pairs)
    } else if let Some(list) = ListRef::from_value(tags) {
        let mut pairs = Vec::with_capacity(list.len());
        for v in list.iter() {
            let s = v.unpack_str().ok_or_else(|| {
                anyhow::anyhow!("tag list items must be strings, got {}", v.get_type())
            })?;
            // split on first ':'; no colon → bare tag
            match s.find(':') {
                Some(i) => pairs.push((s[..i].to_owned(), Some(s[i + 1..].to_owned()))),
                None => pairs.push((s.to_owned(), None)),
            }
        }
        Ok(pairs)
    } else {
        anyhow::bail!("tags must be a dict or list, got {}", tags.get_type());
    }
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

/// Build the `event` struct passed to `fold`: `event.type` and `event.data`.
pub fn alloc_event<'v>(
    module: &Module<'v>,
    event_type: &str,
    data: &serde_json::Value,
) -> Value<'v> {
    let fields: Vec<(&str, Value<'v>)> = vec![
        ("type", module.heap().alloc(event_type)),
        ("data", module.heap().alloc(data.clone())),
    ];
    module.heap().alloc(AllocStruct(fields))
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

/// Interpret the value `handle` returned: `reject(...)`, `invalid_input(...)`, or
/// the events an `emit(...)` collected, lowered to plain data for the store.
pub fn parse_handle_result(val: Value<'_>) -> anyhow::Result<HandleOutcome> {
    if let Some(rejection) = val.downcast_ref::<Rejection>() {
        return Ok(HandleOutcome::Reject(rejection.clone()));
    }
    if let Some(invalid) = val.downcast_ref::<InvalidInput>() {
        return Ok(HandleOutcome::InvalidInput(invalid.clone()));
    }
    if let Some(emit) = val.downcast_ref::<EmitOutcome>() {
        let events = emit
            .events
            .iter()
            .map(|event| EmittedEvent {
                event_type: event.event_type.clone(),
                data: serde_json::from_str(&event.data_json).unwrap_or(serde_json::Value::Null),
                tags: event.tags.clone(),
            })
            .collect();
        return Ok(HandleOutcome::Emit(events));
    }
    anyhow::bail!(
        "handle() must return emit([...]), reject(...) or invalid_input(...), got {}",
        val.get_type()
    );
}

/// Interpret the value a projector's `handle` returned: a list of `put(...)` /
/// `delete(...)` ops (possibly empty).
pub fn parse_entity_ops(val: Value<'_>) -> anyhow::Result<Vec<EntityOp>> {
    let list = ListRef::from_value(val).ok_or_else(|| {
        anyhow::anyhow!(
            "projector handle() must return a list of put(...)/delete(...) ops, got {}",
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
    fields: &[(String, FieldKind)],
    obj: &serde_json::Map<String, serde_json::Value>,
    full: bool,
) -> anyhow::Result<()> {
    for key in obj.keys() {
        if !fields.iter().any(|(name, _)| name == key) {
            anyhow::bail!("unknown field `{key}`");
        }
    }
    for (name, kind) in fields {
        match obj.get(name) {
            Some(value) if value.is_null() && !kind.is_nullable() => {
                anyhow::bail!("field `{name}` is not optional and cannot be null");
            }
            None if full && !kind.is_nullable() => {
                anyhow::bail!("required field `{name}` is missing");
            }
            _ => {}
        }
    }
    Ok(())
}

/// Validate a JSON object against a set of declared fields: no unknown fields,
/// every non-`optional` field present and non-null, and each value well-typed for
/// its `FieldKind`. `what` names the subject in error messages (e.g. an event
/// type, or "input"). Shared by event-payload and command-input validation.
pub fn check_fields(
    what: &str,
    fields: &[(String, FieldKind)],
    obj: &serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<()> {
    for key in obj.keys() {
        if !fields.iter().any(|(name, _)| name == key) {
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

/// Validate a constructed event's payload against its declared fields. This is the
/// check the event constructor runs at emit time, so a malformed event fails where
/// it is built.
pub fn validate_event_payload(
    event_type: &str,
    fields: &[(String, FieldKind)],
    obj: &serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<()> {
    check_fields(&format!("event `{event_type}`"), fields, obj)
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
        FieldKind::I64 => {
            if !value.is_i64() && !value.is_u64() {
                anyhow::bail!("expected an integer");
            }
        }
        FieldKind::U64 => {
            if !value.is_u64() {
                anyhow::bail!("expected a non-negative integer");
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
    digits(whole) && parts.next().map(digits).unwrap_or(true)
}

/// Build the tags for a constructed event from its declared tag fields. A tag's
/// value is the field's scalar value stringified; a null (an `optional` tag field
/// left unset) contributes no tag.
fn derive_tags(
    event_type: &str,
    tag_fields: &[String],
    obj: &serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<Vec<(String, Option<String>)>> {
    let mut tags = Vec::with_capacity(tag_fields.len());
    for field in tag_fields {
        let text = match obj.get(field) {
            None | Some(serde_json::Value::Null) => continue,
            Some(serde_json::Value::String(value)) => value.clone(),
            Some(serde_json::Value::Number(value)) => value.to_string(),
            Some(serde_json::Value::Bool(value)) => value.to_string(),
            Some(other) => anyhow::bail!(
                "event `{event_type}`: tag field `{field}` must be a scalar, got a {}",
                json_kind(other)
            ),
        };
        tags.push((field.clone(), Some(text)));
    }
    Ok(tags)
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

/// Interpret a `query` result or a projector `source`: a single `events(...)` /
/// `all_events()`, or a list of them OR'd together. The specs lower to a tephra
/// `Query` (OR across items, AND within an item's tags).
pub fn parse_event_specs(val: Value<'_>) -> anyhow::Result<Vec<EventSpec>> {
    if let Some(spec) = val.downcast_ref::<EventSpec>() {
        return Ok(vec![spec.clone()]);
    }
    let list = ListRef::from_value(val).ok_or_else(|| {
        anyhow::anyhow!(
            "must be events(...)/all_events() or a list of them, got {}",
            val.get_type()
        )
    })?;
    if list.is_empty() {
        anyhow::bail!("empty list matches nothing; use all_events() to match every event");
    }
    let mut specs = Vec::with_capacity(list.len());
    for item in list.iter() {
        let spec = item.downcast_ref::<EventSpec>().ok_or_else(|| {
            anyhow::anyhow!("list items must be events(...), got {}", item.get_type())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fields() -> Vec<(String, FieldKind)> {
        vec![
            ("id".to_owned(), FieldKind::Uuid),
            ("amount".to_owned(), FieldKind::Money),
            (
                "kind".to_owned(),
                FieldKind::OneOf(vec!["a".to_owned(), "b".to_owned()]),
            ),
            (
                "note".to_owned(),
                FieldKind::Optional(Box::new(FieldKind::Text { max_length: None })),
            ),
        ]
    }

    fn object(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        value.as_object().unwrap().clone()
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
    fn derives_tags_and_skips_null_optionals() {
        let obj = object(serde_json::json!({"id": "u1", "amount": "1", "kind": "a"}));
        let tags = derive_tags("t", &["id".to_owned(), "note".to_owned()], &obj).unwrap();
        assert_eq!(tags, vec![("id".to_owned(), Some("u1".to_owned()))]);
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
        use starlark::environment::Module;

        use crate::context::HandleCtx;

        let ast = parse_module("t.star", "def f():\n    return now()\n".to_owned()).unwrap();
        let frozen = eval_frozen(ast, &command_globals(), None).unwrap();
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
}
