//! Derived tables: the read model a program's named folds imply, as SQLite.
//!
//! One sentence carries it: **a read model is a named fold, kept for every key.** A fold
//! already says what a value is, which events change it and what it is keyed by, so the
//! grouping is not hekla's to decide. [`heklang::layout`] decides it, `hek check --tables`
//! prints what it returns, and this turns the same answer into DDL. Writing a second
//! derivation here would let `--tables` and the real tables disagree about what an author
//! is looking at, which is why the rules live in the library.
//!
//! Built from heklang's IR one way only, like [`crate::schema`]: nothing here settles a
//! question the checker has not.

use std::collections::BTreeMap;

use anyhow::Context;

use heklang::value::Defs;
use heklang::value::Record;
use heklang::{ArmMatch, Column, KeyPart, Layout, Program, Table, layout};

use crate::schema::{FieldKind, FieldMeta, IndexDef, Seal};

/// One derived table: one key signature, one row per key, one column per fold.
#[derive(Debug, Clone)]
pub struct TableDef {
    /// `layout`'s own name, which is already unique across the program including the
    /// disambiguate-by-part-type pass, or `_side_<name>` for a collection table.
    pub name: String,
    /// The `layout` table this one holds columns of, which is [`TableDef::name`] itself for
    /// a main table and the table it splits for a side table.
    ///
    /// A layout table's columns are divided between the two by kind, so neither name on
    /// its own answers "which table holds this column": `layout` reports a collection
    /// column under the key signature it belongs to, and the column lives in the side
    /// table. This is what joins the two back up.
    pub source: String,
    /// The key columns in key order, which is the table's `PRIMARY KEY`. Empty for the
    /// single-row `global` table.
    pub key: Vec<(String, FieldMeta)>,
    /// One column per fold, alphabetical, as `layout` orders them.
    pub columns: Vec<ColumnDef>,
    pub indexes: Vec<IndexDef>,
}

/// One column, and the fold it is kept for.
#[derive(Debug, Clone)]
pub struct ColumnDef {
    pub name: String,
    /// The fold's declared name. What the column catalog keys this column by, so that
    /// changing one fold rebuilds one column.
    pub fold: String,
    /// The fold's own parameter name for each part of the table's key, in key order.
    ///
    /// A key part is named for the parameter with one trailing `_id` taken off, or for the
    /// subject it keys, so the part's name is usually not the parameter's and
    /// `Interpreter::fold_stored` wants the parameter's. Per column rather than per table
    /// because two folds sharing a key signature may spell their parameters differently:
    /// `A(shop_id: Shop)` and `B(shop: Shop)` are one key and two names for it.
    pub args: Vec<String>,
    /// The fold's seed as a SQL literal, for a column that cannot hold `NULL`.
    ///
    /// **Both paths onto a column need it, for one reason.** `ALTER TABLE ... ADD COLUMN`
    /// needs a default to give the rows already there, and a `CREATE TABLE` column needs
    /// one because the writer inserts a row one column at a time: two folds of one key are
    /// two columns an event may touch separately, so the columns it did not touch have to
    /// come into being holding something. For both, the something is the seed, which is
    /// what folding that column for that key would return.
    pub default: Option<String>,
    pub meta: FieldMeta,
}

/// The guard column a keyless table carries, so `global` really holds one row.
///
/// A fold with no parameters has one value, not a table of them. `_hekla_checkpoint`
/// states the same thing the same way, and a leading `_` cannot collide with a column
/// name, which is the snake_case of a fold name.
const ONE_ROW: &str = "_one";

impl TableDef {
    /// Every table a program's named folds imply, main tables and collection tables
    /// together, in `layout`'s order with each side table just after the table it splits.
    pub fn all(program: &Program) -> Vec<TableDef> {
        let defs = Defs::of(program);
        let mut out = Vec::new();
        for table in &layout(program).tables {
            let (scalars, collections) = split(program, table, defs);
            out.push(TableDef {
                name: table.name.clone(),
                source: table.name.clone(),
                key: key_columns(table, defs),
                columns: scalars,
                indexes: ranged_indexes(table),
            });
            // **From the start, not when it shows up.** The widest row in the ported
            // application is 87 columns of which 5 are collections, and 40 of the 46
            // event types that touch it write scalars only: a two-column write would
            // otherwise rewrite 87 scalars plus five collections for nothing. Nothing
            // bounds a collection either, since `@max` applies to `String` and nothing
            // else.
            if !collections.is_empty() {
                out.push(TableDef {
                    name: format!("_side_{}", table.name),
                    source: table.name.clone(),
                    key: key_columns(table, defs),
                    columns: collections,
                    indexes: Vec::new(),
                });
            }
        }
        out
    }

    /// DDL generation. The payoff of a derived schema: nobody writes SQL or migrations.
    pub fn create_table_sql(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        if self.key.is_empty() {
            lines.push(format!(
                "  {} INTEGER NOT NULL CHECK ({} = 0)",
                quote(ONE_ROW),
                quote(ONE_ROW)
            ));
        }
        for (name, meta) in &self.key {
            lines.push(format!("  {} {} NOT NULL", quote(name), meta.sql_type()));
        }
        for column in &self.columns {
            let null = if nullable(&column.meta) {
                ""
            } else {
                " NOT NULL"
            };
            // The writer inserts a row one column at a time, so a column this write did
            // not touch has to come into being holding its fold's seed.
            let default = match &column.default {
                Some(literal) => format!(" DEFAULT {literal}"),
                None => String::new(),
            };
            lines.push(format!(
                "  {} {}{null}{default}",
                quote(&column.name),
                column.meta.sql_type(),
            ));
        }
        // Trailing rather than inlined on one column, because a derived key has as many
        // parts as the fold has parameters.
        let key: Vec<String> = if self.key.is_empty() {
            vec![quote(ONE_ROW)]
        } else {
            self.key.iter().map(|(name, _)| quote(name)).collect()
        };
        lines.push(format!("  PRIMARY KEY ({})", key.join(", ")));
        format!(
            "CREATE TABLE IF NOT EXISTS {} (\n{}\n)",
            quote(&self.name),
            lines.join(",\n")
        )
    }

    pub fn create_index_sql(&self) -> Vec<String> {
        self.indexes
            .iter()
            .map(|ix| {
                let columns: Vec<String> = ix.columns.iter().map(|name| quote(name)).collect();
                format!(
                    "CREATE INDEX IF NOT EXISTS {} ON {} ({})",
                    quote(&format!("{}_{}", self.name, ix.name)),
                    quote(&self.name),
                    columns.join(", ")
                )
            })
            .collect()
    }
}

/// **A column holds the fold's value, and the seed is a value.** A key minted by
/// `@order.placed` has `order_total` folded and `order_cancelled` sitting at `false`,
/// because that is what folding the column for that key returns. So `NULL` means one of
/// two things only: an optional fold answering `none`, or a sealed column whose subject
/// has been erased, which is why a sealed column is nullable whatever its type says.
fn nullable(meta: &FieldMeta) -> bool {
    meta.is_nullable() || meta.sealed_under.is_some()
}

fn key_columns(table: &Table, defs: Defs<'_>) -> Vec<(String, FieldMeta)> {
    table
        .key
        .iter()
        .map(|part| {
            (
                part.name.clone(),
                FieldMeta {
                    kind: FieldKind::of(&part.ty, defs),
                    indexed: true,
                    // A key part is an id, never sealed content: heklang refuses a filter
                    // on sealed content, so nothing could look a row up by one.
                    sealed_under: None,
                    identifies: part.subject.clone(),
                },
            )
        })
        .collect()
}

/// The table's columns, scalars first and collections second.
///
/// A record, a list and a map all store as the JSON rule 8 already puts them on the wire,
/// so `FieldKind::Json` is exactly the set that belongs in the side table.
fn split(program: &Program, table: &Table, defs: Defs<'_>) -> (Vec<ColumnDef>, Vec<ColumnDef>) {
    let mut scalars = Vec::new();
    let mut collections = Vec::new();
    for column in &table.columns {
        let def = column_def(program, table, column, defs);
        if matches!(def.meta.kind.base(), FieldKind::Json) {
            collections.push(def);
        } else {
            scalars.push(def);
        }
    }
    (scalars, collections)
}

fn column_def(program: &Program, table: &Table, column: &Column, defs: Defs<'_>) -> ColumnDef {
    // The subject comes off the key rather than being found by type the way an entity's
    // does: a fold's key says which part keys which subject, so there is nothing to
    // infer. A seal whose subject is not in the key has no id column to read, which is
    // what the `None` arm leaves for a store to refuse rather than guess at.
    let sealed_under = column.ty.subject().and_then(|subject| {
        id_column(table, subject).map(|id| Seal {
            chain: vec![(subject.clone(), id)],
        })
    });
    let kind = FieldKind::of(&column.ty, defs);
    ColumnDef {
        name: column.name.clone(),
        fold: column.fold.clone(),
        args: arg_names(program, &column.fold, &table.key),
        default: seed_literal(program, column, &kind, sealed_under.is_some()),
        meta: FieldMeta {
            kind,
            indexed: false,
            sealed_under,
            identifies: None,
        },
    }
}

fn id_column(table: &Table, subject: &str) -> Option<String> {
    table
        .key
        .iter()
        .find(|part| part.subject.as_deref() == Some(subject))
        .map(|part: &KeyPart| part.name.clone())
}

/// One index per distinct set of parts an arm pins while leaving another part open.
///
/// `ArmMatch::Open` is a **range update** over that part, so whatever writes the row has
/// to find every existing key matching the parts the arm did pin. The index that makes
/// that a seek is one the query side wants anyway, which is why it is derived here rather
/// than waited for.
fn ranged_indexes(table: &Table) -> Vec<IndexDef> {
    let mut seen: BTreeMap<Vec<String>, ()> = BTreeMap::new();
    for column in &table.columns {
        for arm in &column.arms {
            if arm.fields.iter().all(ArmMatch::pinned) {
                continue;
            }
            let pinned: Vec<String> = arm
                .fields
                .iter()
                .zip(&table.key)
                .filter(|(field, _)| field.pinned())
                .map(|(_, part)| part.name.clone())
                .collect();
            // Every part open is not a range over a subset, it is every row of the
            // table, and an index over nothing cannot help.
            if !pinned.is_empty() {
                seen.insert(pinned, ());
            }
        }
    }
    seen.into_keys()
        .map(|columns| IndexDef {
            name: format!("by_{}", columns.join("_")),
            columns,
        })
        .collect()
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// What one record pins, for one column, before the open parts are filled in.
///
/// `parts` is one entry per key part in the table's key order: `Some` where the arm fixed
/// the part, `None` where it left it open and whatever writes the row has to range over
/// every value of it that already exists.
#[derive(Debug, Clone, PartialEq)]
pub struct Pins {
    pub table: String,
    pub column: String,
    pub fold: String,
    pub parts: Vec<Option<heklang::Value>>,
}

impl Pins {
    /// Whether every part is pinned, which makes this one key rather than a range.
    pub fn complete(&self) -> bool {
        self.parts.iter().all(Option::is_some)
    }
}

/// Every column one record could have changed, with the key parts that record pins.
///
/// **This is `layout`'s answer and not a second derivation.** An arm says which key it
/// writes; `ArmMatch::Field` names the event field the value comes from, which is often
/// not the parameter's name, `ArmMatch::Fixed` is rule 13's conjunct resolved against an
/// empty frame, and `ArmMatch::Open` is a range over that part. Conflating the last two
/// would tell a writer to range over the very part an arm fixed.
///
/// **Deliberately an over-approximation.** A `ColumnArm` carries the key parts and not the
/// arm's field filters, so a record reaching an arm whose filter it fails is still
/// reported here. That costs a refold of a column that will come back the value it already
/// held, and it cannot be wrong: `docs/host.md` rule 4's asymmetry, where over-delivering
/// is a cost and under-delivering is a bug, holds the same way round for this.
pub fn pinned(program: &Program, layout: &Layout, record: &Record) -> Vec<Pins> {
    let mut out = Vec::new();
    for table in &layout.tables {
        for column in &table.columns {
            for arm in &column.arms {
                if arm.event != record.event.path {
                    continue;
                }
                let parts: Vec<Option<heklang::Value>> = arm
                    .fields
                    .iter()
                    .zip(&table.key)
                    .map(|(field, part)| match field {
                        ArmMatch::Field(name) => record.event.fields.get(name).cloned(),
                        // A conjunct resolves once against an empty frame, which is the
                        // whole of rule 13's guarantee, so a failure here is a program
                        // that should not have checked rather than a key to guess at.
                        ArmMatch::Fixed(at) => {
                            heklang::fixed(program, &column.fold, part, *at).ok()
                        }
                        ArmMatch::Open => None,
                    })
                    .collect();
                out.push(Pins {
                    table: table.name.clone(),
                    column: column.name.clone(),
                    fold: column.fold.clone(),
                    parts,
                });
            }
        }
    }
    out
}

/// One stored key part back as the value a fold takes as its argument.
///
/// The store binds a key part as text, because that is the one rendering every key type
/// shares, and a fold's parameter is typed. This is the one conversion between them.
pub fn key_value(kind: &FieldKind, text: &str) -> anyhow::Result<heklang::Value> {
    use heklang::Value;
    Ok(match kind.base() {
        FieldKind::I64 => Value::Int(text.parse().with_context(|| format!("`{text}` as Int"))?),
        FieldKind::Bool => Value::Bool(text == "1" || text == "true"),
        FieldKind::Uuid => Value::uuid(text),
        FieldKind::Timestamp => Value::Timestamp(
            heklang::value::timestamp(text).with_context(|| format!("`{text}` as Timestamp"))?,
        ),
        // An enum key is its variant name, and a `Money` or a `Json` cannot be one.
        _ => Value::str(text),
    })
}

/// The fold's parameter name for each part of `key`, in key order.
///
/// `heklang::tables::part_names` is the same derivation the layout used, read in parameter
/// order, so matching on it cannot drift from the names the key actually carries. A part
/// with no parameter is impossible for a program that checked, and falls back to the
/// part's own name so that the failure is a missing argument naming the fold rather than a
/// panic here.
fn arg_names(program: &Program, fold: &str, key: &[KeyPart]) -> Vec<String> {
    let Some(def) = program.fold(fold) else {
        return key.iter().map(|part| part.name.clone()).collect();
    };
    let parts = heklang::tables::part_names(def.params.iter().map(|p| (&p.name, &p.ty)));
    key.iter()
        .map(|part| {
            parts
                .iter()
                .position(|name| *name == part.name)
                .map_or_else(|| part.name.clone(), |at| def.params[at].name.clone())
        })
        .collect()
}

/// The fold's seed as a SQL literal, or `None` for a column that can hold `NULL`.
///
/// A nullable column needs none: SQLite's own default is `NULL`, which is the value an
/// optional fold answering `none` holds and the value an erased seal reads back as.
fn seed_literal(
    program: &Program,
    column: &Column,
    kind: &FieldKind,
    sealed: bool,
) -> Option<String> {
    if sealed || kind.is_nullable() {
        return None;
    }
    let def = program.fold(&column.fold)?;
    let seed = def.stage.folds.first()?;
    // An empty frame, which is what a seed resolves in: a seed that reads a parameter
    // makes the fold unstorable, so no column reaches here with one.
    let value = heklang::interp::eval_pure(program, &def.exprs, def.frame, seed.init).ok()?;
    let stored = crate::heklang_host::column_form(
        kind,
        crate::heklang_host::from_heklang_json(&heklang::Json::from_value(&value)),
    );
    Some(match stored {
        serde_json::Value::Null => "NULL".to_owned(),
        serde_json::Value::Bool(flag) => (if flag { "1" } else { "0" }).to_owned(),
        serde_json::Value::Number(number) => number.to_string(),
        serde_json::Value::String(text) => sql_string(&text),
        // A record, a list and a map store as the JSON they go on the wire as, so the
        // document is the value and quoting it is the whole of the rendering.
        other => sql_string(&other.to_string()),
    })
}

/// `ALTER TABLE ... DEFAULT` and a `CREATE TABLE` default take no bind parameter, so these
/// are the two places hekla writes a value into SQL rather than binding it.
fn sql_string(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// The table that actually holds `column` of the layout table `source`.
///
/// `layout` reports every column of a key signature together, and [`TableDef::all`] splits
/// the collection-typed ones into a side table, so a caller holding a layout table's name
/// and a column name cannot assume the two are in the same place.
pub fn holding<'a>(
    tables: &'a [TableDef],
    source: &str,
    column: &str,
) -> Option<(&'a TableDef, &'a ColumnDef)> {
    tables
        .iter()
        .filter(|table| table.source == source)
        .find_map(|table| {
            table
                .columns
                .iter()
                .find(|held| held.name == column)
                .map(|held| (table, held))
        })
}

/// The main table of a key signature, which is where every key of it is recorded.
///
/// A side table holds only the keys a collection column has been written for, so ranging
/// over one answers a narrower question than "every key this table has". The main table is
/// the one that answers it.
pub fn main_of<'a>(tables: &'a [TableDef], source: &str) -> Option<&'a TableDef> {
    tables
        .iter()
        .find(|table| table.source == source && table.name == source)
}
