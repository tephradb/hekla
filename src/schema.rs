//! The schema model: what a project declares, as plain data.
//!
//! Every consumer of these is language-agnostic already. The read model builds its DDL
//! from an [`EntityDef`], the read API types a column from a [`FieldKind`], the OpenAPI
//! document walks an [`EventDef`], and none of them cares what parsed it. Keeping them
//! here is what lets the language underneath change without those files moving with it.
//!
//! Built from heklang's IR and from nothing else. The conversion runs one way: heklang
//! decides what a declaration means, and this is the runtime's view of the result, so
//! nothing here settles a question the checker has not already settled.

use std::fmt;

use heklang::ir::{self, Type};
use heklang::{Defs, Program, Projector};

use crate::read_api::{self, RESERVED_QUERY_PARAMS};
use crate::read_model::quote_ident;

// ---------------------------------------------------------------------------
// Field types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum FieldKind {
    Text {
        max_length: Option<u32>,
    },
    I64,
    Bool,
    Uuid,
    Timestamp,
    /// Fixed-scale decimal. Do not use floats for money. The scale is part of
    /// heklang's type and checked there, so it is carried rather than assumed.
    Money {
        scale: u8,
    },
    Json,
    OneOf(Vec<String>),
    Optional(Box<FieldKind>),
}

impl FieldKind {
    /// SQLite column type. The runtime generates DDL from this.
    pub fn sql_type(&self) -> &'static str {
        match self {
            FieldKind::Text { .. } | FieldKind::Uuid | FieldKind::OneOf(_) => "TEXT",
            FieldKind::I64 => "INTEGER",
            // Money is a decimal string on the wire; store it verbatim so a value like
            // "10.50" round-trips and reads back the same JSON type whether or not the
            // field is subject-encrypted.
            FieldKind::Money { .. } => "TEXT",
            FieldKind::Bool => "INTEGER",
            FieldKind::Timestamp => "TEXT", // ISO-8601, sorts lexicographically
            FieldKind::Json => "TEXT",
            FieldKind::Optional(inner) => inner.sql_type(),
        }
    }

    pub fn is_nullable(&self) -> bool {
        matches!(self, FieldKind::Optional(_))
    }

    /// The kind spelled the way an author declared it, so introspection reports the
    /// heklang type rather than the storage it happens to share with another.
    pub fn describe(&self) -> String {
        match self {
            FieldKind::Text {
                max_length: Some(n),
            } => format!("String @max({n})"),
            FieldKind::Text { max_length: None } => "String".to_owned(),
            FieldKind::I64 => "Int".to_owned(),
            FieldKind::Bool => "Bool".to_owned(),
            FieldKind::Uuid => "Uuid".to_owned(),
            FieldKind::Timestamp => "Timestamp".to_owned(),
            FieldKind::Money { scale } => format!("Money({scale})"),
            FieldKind::Json => "Json".to_owned(),
            FieldKind::OneOf(values) => values.join(" | "),
            FieldKind::Optional(inner) => format!("{}?", inner.describe()),
        }
    }

    /// heklang's type, as the runtime stores it.
    ///
    /// A seal does not show up here: whether a column holds ciphertext is
    /// [`FieldMeta::sql_type`]'s question, and the kind underneath is what the read API
    /// re-types the plaintext back to on the way out.
    pub fn of(ty: &Type, defs: Defs<'_>) -> FieldKind {
        match ty {
            Type::Sealed(inner, _) => FieldKind::of(inner, defs),
            Type::Opt(inner) => FieldKind::Optional(Box::new(FieldKind::of(inner, defs))),
            Type::Bool => FieldKind::Bool,
            Type::Int => FieldKind::I64,
            Type::String => FieldKind::Text { max_length: None },
            Type::Uuid => FieldKind::Uuid,
            Type::Timestamp => FieldKind::Timestamp,
            Type::Money(scale) | Type::Decimal(scale) => FieldKind::Money { scale: *scale },
            // The variants come off the declaration rather than the type, which is why
            // this needs the definitions a projector's own enums shadow.
            Type::Enum(name) => FieldKind::OneOf(
                defs.enum_def(name)
                    .map(|def| def.variants.clone())
                    .unwrap_or_default(),
            ),
            // A record, a list and a map are stored as the JSON rule 8 already says
            // they are on the wire, so a column holds one encoding rather than two.
            Type::Record(_) | Type::List(_) | Type::Map(..) | Type::Json => FieldKind::Json,
            // Not writable at a declared position: the checker rejects these long
            // before a field could hold one.
            Type::Rounding | Type::Response | Type::Outcome => FieldKind::Json,
        }
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
#[derive(Debug, Clone, PartialEq)]
pub struct FieldMeta {
    pub kind: FieldKind,
    pub indexed: bool,
    pub subject: Option<String>,
}

impl FieldMeta {
    /// A plain field: indexed and unscoped. The default for every field that opts
    /// into nothing.
    pub fn plain(kind: FieldKind) -> FieldMeta {
        FieldMeta {
            kind,
            indexed: true,
            subject: None,
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
// ---------------------------------------------------------------------------
// Input schema (commands)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct InputSchema {
    pub fields: Vec<(String, FieldKind)>,
}

impl fmt::Display for InputSchema {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "schema({} fields)", self.fields.len())
    }
}

// ---------------------------------------------------------------------------
// Entity schema (projectors)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
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

#[derive(Debug, Clone)]
pub struct EntityDef {
    /// The table name, which is the entity's declared name.
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
            FieldKind::Bool | FieldKind::Json | FieldKind::Money { .. }
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
        //
        // Derived from `filterable_fields` rather than open-coded, because this gate is
        // the only thing stopping the OpenAPI generator from emitting a duplicate query
        // parameter. Widen filterability there (to any prefix of a composite index, say)
        // and an entity whose index leads on a column named `limit` would start loading
        // while `scan_params` emitted a second `limit` parameter, shadowing the page-size
        // control with an invalid document.
        for field in read_api::filterable_fields(self) {
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

/// Whether `name` is a plain SQL identifier: ascii letters, digits and underscores,
/// starting with a letter or underscore.
fn is_sql_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// ---------------------------------------------------------------------------
// Event definition: declares fields and which fields become store tags
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct EventDef {
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

// ---------------------------------------------------------------------------
// Entity operations (projectors): what a projector's `handle` emits per event
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
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
        /// The subscription: the event types its handlers select, OR'd together into
        /// the read query. A command has no equivalent, because its boundary is
        /// resolved per invocation from the arguments it was called with.
        sources: Vec<String>,
    },
    Effect {
        name: String,
        /// The subscription: the event types its arms select.
        sources: Vec<String>,
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
// Plain data the rest of the runtime reads
// ---------------------------------------------------------------------------

/// The scalar string form of a JSON value for a tag or a subject id: strings as-is,
/// numbers and bools by their canonical text. `None` for null or a composite.
pub fn scalar_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Number(number) => Some(number.to_string()),
        serde_json::Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

/// One event emitted by `handle`, lowered to plain data for the store.
#[derive(Debug, Clone)]
pub struct EmittedEvent {
    pub event_type: String,
    pub data: serde_json::Value,
    pub tags: Vec<(String, Option<String>)>,
}

// ---------------------------------------------------------------------------
// Reading a program's declarations off heklang's IR
// ---------------------------------------------------------------------------

/// The wire name of an event path: `@order.placed` is stored, tagged and queried as
/// `order.placed`, because the sigil is heklang's syntax rather than part of the name.
pub fn event_type(path: &ir::EventPath) -> String {
    path.segments.join(".")
}

/// Pushes a declared `@max` down onto the text kind it bounds, through an optional.
fn bounded(kind: FieldKind, max_len: Option<usize>) -> FieldKind {
    match (kind, max_len) {
        (FieldKind::Optional(inner), max) => FieldKind::Optional(Box::new(bounded(*inner, max))),
        (FieldKind::Text { .. }, Some(max)) => FieldKind::Text {
            max_length: u32::try_from(max).ok(),
        },
        (kind, _) => kind,
    }
}

impl EventDef {
    /// One declared event, as the runtime stores and tags it.
    pub fn of(def: &ir::EventDef, defs: Defs<'_>) -> EventDef {
        EventDef {
            event_type: event_type(&def.path),
            fields: def
                .fields
                .iter()
                .map(|field| {
                    (
                        field.name.clone(),
                        FieldMeta {
                            kind: bounded(FieldKind::of(&field.ty, defs), field.max_len),
                            indexed: field.indexed,
                            subject: field.subject.clone(),
                        },
                    )
                })
                .collect(),
        }
    }

    /// Every event a program declares.
    pub fn all(program: &Program) -> Vec<EventDef> {
        let defs = Defs::of(program);
        program
            .events
            .iter()
            .map(|def| EventDef::of(def, defs))
            .collect()
    }
}

impl EntityDef {
    /// One declared entity, as a table.
    ///
    /// The subject is read off the column rather than off an annotation:
    /// `docs/projectors.md` rule 9 propagates a seal onto whichever column receives
    /// sealed content, so heklang has already worked out whose key a column needs.
    pub fn of(def: &ir::EntityDef, defs: Defs<'_>) -> EntityDef {
        let key = def.key_field().name.clone();
        EntityDef {
            name: def.name.clone(),
            fields: def
                .fields
                .iter()
                .map(|field| {
                    // A column is filterable in the read API when the author asked for
                    // it: `@key`, or a column named by an `@index`.
                    let indexed = field.name == key
                        || def
                            .indexes
                            .iter()
                            .any(|index| index.fields.contains(&field.name));
                    (
                        field.name.clone(),
                        FieldMeta {
                            kind: bounded(FieldKind::of(&field.ty, defs), field.max_len),
                            indexed,
                            subject: field.subject.clone(),
                        },
                    )
                })
                .collect(),
            key,
            indexes: def
                .indexes
                .iter()
                .map(|index| IndexDef {
                    name: format!("by_{}", index.fields.join("_")),
                    columns: index.fields.clone(),
                })
                .collect(),
        }
    }

    /// Every entity one projector declares, in declaration order.
    pub fn all(program: &Program, projector: &Projector) -> Vec<EntityDef> {
        let defs = Defs::in_projector(program, projector);
        projector
            .entities
            .iter()
            .map(|def| EntityDef::of(def, defs))
            .collect()
    }
}

impl InputSchema {
    /// A command's parameters, which are its request body.
    pub fn of(command: &ir::Command, defs: Defs<'_>) -> InputSchema {
        InputSchema {
            fields: command
                .params
                .iter()
                .map(|param| (param.name.clone(), FieldKind::of(&param.ty, defs)))
                .collect(),
        }
    }
}

/// Every declared event by its wire type, which is what the append and read paths look
/// a definition up by.
pub type EventDefs = std::collections::HashMap<String, EventDef>;
