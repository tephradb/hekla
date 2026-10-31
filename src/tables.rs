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

use heklang::ir::Type;
use heklang::value::Defs;
use heklang::{ArmMatch, Column, Program, Table, layout};

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
    /// `heklang::Column::args`: the fold's own parameter name for each part of the
    /// table's key, in key order, which is what `fold_stored` has to be called with.
    ///
    /// Carried by the layout rather than derived here. hekla did derive it once, by
    /// matching `part_names` back onto the parameters, and that is a second
    /// implementation of a rule with one right answer.
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
    /// The fold's declared return type, which is the column's.
    ///
    /// Kept whole beside `meta.kind` because reading a stored cell back as a value needs
    /// the declared type and not the storage it shares with another: `Value::from_json`
    /// takes the type, and a `Map(String, Int)` and a `List(Int)` are both `FieldKind::Json`.
    pub ty: Type,
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
        let def = column_def(program, column, defs);
        if matches!(def.meta.kind.base(), FieldKind::Json) {
            collections.push(def);
        } else {
            scalars.push(def);
        }
    }
    (scalars, collections)
}

fn column_def(program: &Program, column: &Column, defs: Defs<'_>) -> ColumnDef {
    // **Every sealed column is marked, and none names an id column.** A derived cell
    // carries its own subject id (`fold_store::SealedCell`), because a fold's key need not
    // hold one: `OrderEmail(order_id, shop_id) -> Sealed(String, Customer)?` is keyed on
    // an order. Naming the key part only when there is one would leave a reader that looks
    // the id up in the row working for some columns and silently not for others.
    let sealed_under = column.ty.subject().map(|subject| Seal {
        chain: vec![(subject.clone(), String::new())],
    });
    let kind = FieldKind::of(&column.ty, defs);
    ColumnDef {
        name: column.name.clone(),
        fold: column.fold.clone(),
        args: column.args.clone(),
        default: seed_literal(program, column, &kind, sealed_under.is_some()),
        ty: column.ty.clone(),
        meta: FieldMeta {
            kind,
            indexed: false,
            sealed_under,
            identifies: None,
        },
    }
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
