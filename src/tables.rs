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

use heklang::value::Defs;
use heklang::{ArmMatch, Column, KeyPart, Program, Table, layout};

use crate::schema::{FieldKind, FieldMeta, IndexDef, Seal};

/// One derived table: one key signature, one row per key, one column per fold.
#[derive(Debug, Clone)]
pub struct TableDef {
    /// `layout`'s own name, which is already unique across the program including the
    /// disambiguate-by-part-type pass, or `_side_<name>` for a collection table.
    pub name: String,
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
            let (scalars, collections) = split(table, defs);
            out.push(TableDef {
                name: table.name.clone(),
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
            lines.push(format!(
                "  {} {}{}",
                quote(&column.name),
                column.meta.sql_type(),
                if nullable(&column.meta) {
                    ""
                } else {
                    " NOT NULL"
                }
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
fn split(table: &Table, defs: Defs<'_>) -> (Vec<ColumnDef>, Vec<ColumnDef>) {
    let mut scalars = Vec::new();
    let mut collections = Vec::new();
    for column in &table.columns {
        let def = column_def(table, column, defs);
        if matches!(def.meta.kind.base(), FieldKind::Json) {
            collections.push(def);
        } else {
            scalars.push(def);
        }
    }
    (scalars, collections)
}

fn column_def(table: &Table, column: &Column, defs: Defs<'_>) -> ColumnDef {
    // The subject comes off the key rather than being found by type the way an entity's
    // does: a fold's key says which part keys which subject, so there is nothing to
    // infer. A seal whose subject is not in the key has no id column to read, which is
    // what the `None` arm leaves for a store to refuse rather than guess at.
    let sealed_under = column.ty.subject().and_then(|subject| {
        id_column(table, subject).map(|id| Seal {
            chain: vec![(subject.clone(), id)],
        })
    });
    ColumnDef {
        name: column.name.clone(),
        fold: column.fold.clone(),
        meta: FieldMeta {
            kind: FieldKind::of(&column.ty, defs),
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
