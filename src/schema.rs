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
            // The `?` binds to the type and a constraint follows it, so a nullable
            // bounded string is `String? @max(200)`; suffixing the whole rendering
            // produced `String @max(200)?`, which is not a declaration anyone wrote.
            FieldKind::Optional(inner) => match inner.as_ref() {
                FieldKind::Text {
                    max_length: Some(n),
                } => format!("String? @max({n})"),
                // A `OneOf` renders as its variants rather than the enum's name, which
                // the kind does not carry. Unbracketed, the `?` would read as though it
                // belonged to the last variant alone.
                FieldKind::OneOf(values) => format!("({})?", values.join(" | ")),
                other => format!("{}?", other.describe()),
            },
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
            // before a field could hold one. `Secret` is the strongest of the four,
            // since rule 16 keeps one out of an `emit` and a projector write both.
            Type::Rounding | Type::Response | Type::Outcome | Type::Secret => FieldKind::Json,
        }
    }

    /// Strip an `Optional(..)` wrapper to reach the underlying kind.
    pub fn base(&self) -> &FieldKind {
        match self {
            FieldKind::Optional(inner) => inner,
            other => other,
        }
    }

    /// Whether a column of this kind has a stable total order in SQLite, which is what
    /// the read API's pagination cursor needs. *Which* order hardly matters there: all a
    /// cursor has to do is walk every row exactly once.
    ///
    /// `Money` has no usable order at all, being stored as its decimal string, so `"2"`
    /// sorts above `"10"`. A `Bool` holds two values and would truncate a cursor to two
    /// pages, `Json` is unordered, and an optional column can be NULL, which compares as
    /// neither side of anything.
    pub fn is_keyable(&self) -> bool {
        matches!(
            self,
            FieldKind::Text { .. }
                | FieldKind::I64
                | FieldKind::Uuid
                | FieldKind::Timestamp
                | FieldKind::OneOf(_)
        )
    }

    /// Whether SQLite's order over a column of this kind is the order the *declaration*
    /// means, which is what a range filter needs and a cursor does not.
    ///
    /// Everything [`FieldKind::is_keyable`] refuses, and enums on top: a `OneOf` stores
    /// the variant's spelling, so `priority.gte=Low` would walk the alphabet rather than
    /// the severity that was declared. Fine for a cursor to page by, wrong to answer a
    /// question with.
    pub fn is_comparable(&self) -> bool {
        self.is_keyable() && !matches!(self, FieldKind::OneOf(_))
    }
}

/// A declared field: its type plus the per-field policy that governs tagging and
/// subject-scoped encryption. `indexed` decides whether the field becomes a store
/// tag; `subject` names a sibling field whose per-subject key encrypts this field's
/// value (in the tag, the payload, and any read-model column).
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

    /// The generated name of the declared index called `declared`: the name it carries
    /// in SQLite, and the one a scan pins with `INDEXED BY`. Qualified by the table,
    /// because index names share one namespace across a whole SQLite database and two
    /// entities can both declare `by_shop_id`.
    pub fn index_name(&self, declared: &str) -> String {
        format!("{}_{}", self.name, declared)
    }

    /// The columns one declared index is built over: what was declared, then the key.
    ///
    /// **The trailing key is what makes a filtered scan a seek rather than a sort.** The
    /// scan orders by the key, so an index that stops at the declared columns leaves
    /// SQLite to sort every matching row into key order on *every page*. Equality on all
    /// of the declared columns leaves the key as the next ordered one, so the rows arrive
    /// in the order the scan wanted and `LIMIT` stops reading at the page boundary.
    ///
    /// It buys that for a filter that uses the whole index and not for a shorter prefix,
    /// where a declared column still sits between the filter and the key.
    /// `read_api::choose_index` is the other half: it takes the narrowest index that
    /// serves a filter, so an entity declaring both `index (a)` and `index (a, b)` gets
    /// the seek for `a` alone rather than the sort.
    ///
    /// Appended rather than declared, because it is hekla's storage decision and not
    /// something an author asked for: heklang's digest covers the declared columns, so
    /// adding it here moves no definition hash and rebuilds nothing. That is also why
    /// [`crate::read_model::ReadModel::open`] reconciles the live index against this,
    /// rather than trusting `CREATE INDEX IF NOT EXISTS` to notice.
    pub fn index_columns(&self, ix: &IndexDef) -> Vec<String> {
        let mut columns = ix.columns.clone();
        if !columns.contains(&self.key) {
            columns.push(self.key.clone());
        }
        columns
    }

    pub fn create_index_sql(&self) -> Vec<String> {
        self.indexes
            .iter()
            .map(|ix| {
                let columns: Vec<String> = self
                    .index_columns(ix)
                    .iter()
                    .map(|col| quote_ident(col))
                    .collect();
                format!(
                    "CREATE INDEX IF NOT EXISTS {} ON {} ({})",
                    quote_ident(&self.index_name(&ix.name)),
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
            // `?order_by=` names either a declared index or the key column, and resolves
            // an index first. Index names are generated as `by_<columns>` and never
            // authored, so the two can only ever collide on a key literally spelled that
            // way, and then the key's own ordering becomes unnameable while both spell
            // the same cursor token: a cursor from one ordering would be accepted under
            // the other. Refusing the name is cheaper than making the request path
            // adjudicate it.
            if ix.name == self.key {
                anyhow::bail!(
                    "entity `{}`: the index over ({}) is named `{}`, which is also the key column, so `order_by={}` could mean either; rename the key",
                    self.name,
                    ix.columns.join(", "),
                    ix.name,
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
        // typed filter, so the key must be a present kind that orders. The nullable
        // case is split out because `is_keyable` refuses an optional for a reason the
        // author cannot act on by changing the type: it is the `?` that has to go.
        if key_meta.is_nullable() {
            anyhow::bail!(
                "entity `{}`: key `{}` may not be optional",
                self.name,
                self.key
            );
        }
        if !key_meta.kind.is_keyable() {
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
        // A read filter targets the key or a column of a declared index, so a field
        // named like a reserved read query param could never be filtered. Reject at load
        // rather than surprising the author with a silent no-op at request time.
        //
        // Derived from `filterable_fields` rather than open-coded, because this gate is
        // the only thing stopping the OpenAPI generator from emitting a duplicate query
        // parameter: an entity with a column named `limit` in an index would load while
        // `scan_params` emitted a second `limit` parameter, shadowing the page-size
        // control with an invalid document.
        //
        // This widened when filtering did, from each index's leading column to every
        // column of every index, so an entity that loaded before this and names a
        // reserved param anywhere in an index stops loading. That break is the point:
        // the alternative is a column that cannot be filtered and does not say so.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn optional(inner: FieldKind) -> FieldKind {
        FieldKind::Optional(Box::new(inner))
    }

    /// `describe` is the vocabulary three introspection surfaces report a field in, so
    /// it has to be a declaration an author would recognise as their own.
    #[test]
    fn an_optional_marks_the_type_rather_than_the_whole_declaration() {
        assert_eq!(
            FieldKind::Text {
                max_length: Some(500)
            }
            .describe(),
            "String @max(500)"
        );
        assert_eq!(
            optional(FieldKind::Text {
                max_length: Some(500)
            })
            .describe(),
            "String? @max(500)"
        );
        // Nothing follows the type, so the suffix is unambiguous where it lands.
        assert_eq!(optional(FieldKind::Uuid).describe(), "Uuid?");
        assert_eq!(
            optional(FieldKind::Money { scale: 3 }).describe(),
            "Money(3)?"
        );
        assert_eq!(
            optional(FieldKind::Text { max_length: None }).describe(),
            "String?"
        );
    }

    /// A variant list is not a name, so the `?` needs something to bind to that is not
    /// the last variant.
    #[test]
    fn an_optional_enum_brackets_its_variants() {
        let status = FieldKind::OneOf(vec!["Active".to_owned(), "Claimed".to_owned()]);
        assert_eq!(status.describe(), "Active | Claimed");
        assert_eq!(optional(status).describe(), "(Active | Claimed)?");
    }
}
