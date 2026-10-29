//! `src/tables.rs`: the derived read model as SQLite.
//!
//! The grouping is `heklang::layout`'s and is proved in heklang's own `tests/tables.rs`.
//! What is worth proving here is the half hekla owns: that the DDL is real SQLite, that a
//! collection-typed fold is split off from the start, that a keyless fold really holds one
//! row, and that an arm leaving a key part open gets the index its range update needs.

use hekla::tables::TableDef;
use heklang::{Program, parse};
use rusqlite::Connection;

fn program(source: &str) -> Program {
    parse(source).unwrap_or_else(|err| panic!("the source under test checks: {err}"))
}

/// Every table a program implies, created in one in-memory database, indexes and all.
///
/// This is the assertion: nothing here inspects the SQL, it runs it. A `CREATE TABLE`
/// SQLite will not take is a schema no deployment could have.
fn opened(source: &str) -> (Connection, Vec<TableDef>) {
    let program = program(source);
    let tables = TableDef::all(&program);
    let conn = Connection::open_in_memory().expect("an in-memory database");
    for table in &tables {
        conn.execute_batch(&table.create_table_sql())
            .unwrap_or_else(|err| panic!("{}\n{err}", table.create_table_sql()));
        for sql in table.create_index_sql() {
            conn.execute_batch(&sql)
                .unwrap_or_else(|err| panic!("{sql}\n{err}"));
        }
    }
    (conn, tables)
}

/// The declared column type per column, as SQLite reads its own schema back.
fn columns(conn: &Connection, table: &str) -> Vec<(String, String, bool)> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info(\"{table}\")"))
        .unwrap();
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)? == 1,
            ))
        })
        .unwrap();
    rows.map(|row| row.unwrap()).collect()
}

const SPEND: &str = "subject Customer(Int)

event @order.placed { customer_id: Customer, order_id: Int, total: Money(2) }
event @order.cancelled { customer_id: Customer }

fold LifetimeSpend(customer_id: Customer) -> spend: Money(2) = 0 {
  on @order.placed(customer_id) { total } => spend + total
}

fold Cancelled(customer_id: Customer) -> Bool = false {
  on @order.cancelled(customer_id) => true
}
";

#[test]
fn every_derived_table_is_sql_sqlite_takes() {
    let (conn, tables) = opened(SPEND);
    assert_eq!(
        tables.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
        vec!["customer"],
        "one table per key signature, named for its key"
    );
    assert_eq!(
        columns(&conn, "customer"),
        vec![
            ("customer".to_owned(), "INTEGER".to_owned(), true),
            ("cancelled".to_owned(), "INTEGER".to_owned(), true),
            ("lifetime_spend".to_owned(), "TEXT".to_owned(), true),
        ],
        "the key leads, then one column per fold alphabetically; `Money` is its decimal \
         string, so it is TEXT, and a non-optional column is NOT NULL because a column \
         holds the fold's value and the seed is a value"
    );
}

/// A row exists once any event matched the key, and every column of it holds that fold's
/// value, which is the seed where no arm has fired. So the only `NULL` a derived column
/// can hold is an optional fold answering `none` or a sealed column whose subject is gone.
#[test]
fn only_an_optional_or_a_sealed_column_is_nullable() {
    let (conn, _) = opened(
        "subject Shop(Int)

event @shop.connected { shop_id: Shop, token: String @subject(shop_id) }
event @shop.seen { shop_id: Shop }

fold ShopToken(shop_id: Shop) -> Sealed(String, Shop)? = none {
  on @shop.connected(shop_id) { token } => token
}

fold ShopSeen(shop_id: Shop) -> Bool = false {
  on @shop.seen(shop_id) => true
}
",
    );
    let nullable: Vec<(String, bool)> = columns(&conn, "shop")
        .into_iter()
        .map(|(name, _, not_null)| (name, !not_null))
        .collect();
    assert_eq!(
        nullable,
        vec![
            ("shop".to_owned(), false),
            ("shop_seen".to_owned(), false),
            ("shop_token".to_owned(), true),
        ]
    );
}

/// **The split is from the start, not from when it shows up.** Measured on the ported
/// application's widest row: 87 columns of which 5 are collections, 46 event types that
/// touch it and 40 of them writing scalars only, so a median two-column write would
/// rewrite five collections for nothing. Nothing bounds one either, since `@max` applies
/// to `String` and nothing else.
#[test]
fn a_collection_typed_fold_goes_to_a_side_table_with_the_same_key() {
    let (conn, tables) = opened(
        "subject Shop(Int)

event @order.placed { shop_id: Shop, order_id: Int, line: Int }

fold OrderLines(shop_id: Shop, order_id: Int) -> lines: List(Int) = [] {
  on @order.placed(shop_id, order_id) { line } => lines.push(line)
}

fold OrderPlaced(shop_id: Shop, order_id: Int) -> Bool = false {
  on @order.placed(shop_id, order_id) => true
}
",
    );
    assert_eq!(
        tables.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
        vec!["shop__order", "_side_shop__order"],
        "the side table sits just after the table it splits, and a leading `_` cannot \
         collide with a layout name"
    );
    // The same key, so a row in either is reached by the same value.
    let key = |table: &str| -> Vec<String> {
        columns(&conn, table)
            .into_iter()
            .filter(|(name, _, _)| name == "shop" || name == "order")
            .map(|(name, _, _)| name)
            .collect()
    };
    assert_eq!(key("shop__order"), key("_side_shop__order"));
    assert_eq!(
        columns(&conn, "_side_shop__order")
            .into_iter()
            .map(|(name, ty, _)| (name, ty))
            .collect::<Vec<_>>(),
        vec![
            ("shop".to_owned(), "INTEGER".to_owned()),
            ("order".to_owned(), "INTEGER".to_owned()),
            ("order_lines".to_owned(), "TEXT".to_owned()),
        ],
        "a list stores as the JSON rule 8 already puts it on the wire"
    );
}

/// A fold with no parameters has one value, not a table of them.
#[test]
fn a_fold_with_no_parameters_is_one_row() {
    let (conn, tables) = opened(
        "event @thing.happened { n: Int }

fold Things() -> n: Int = 0 {
  on @thing.happened() => n + 1
}
",
    );
    assert_eq!(tables[0].name, "global");
    conn.execute(
        "INSERT INTO \"global\" (\"_one\", \"things\") VALUES (0, 1)",
        [],
    )
    .expect("the one row");
    let second = conn.execute(
        "INSERT INTO \"global\" (\"_one\", \"things\") VALUES (1, 2)",
        [],
    );
    assert!(
        second.is_err(),
        "a second row is refused by the guard column rather than stored"
    );
}

/// `ArmMatch::Open` is a range update over that part, so whatever writes the row has to
/// find every existing key matching the parts the arm did pin. The index that makes it a
/// seek is one the query side wants anyway.
#[test]
fn an_arm_that_leaves_a_key_part_open_gets_the_index_its_range_needs() {
    let (_, tables) = opened(
        "subject Shop(Int)

event @shop.rendered { shop_id: Shop, surface: String }
event @shop.uninstalled { shop_id: Shop }

fold ShopRendered(shop_id: Shop, surface: String) -> Bool = false {
  on @shop.rendered(shop_id, surface) => true
  on @shop.uninstalled(shop_id) => false
}
",
    );
    let indexes: Vec<Vec<String>> = tables[0]
        .indexes
        .iter()
        .map(|ix| ix.columns.clone())
        .collect();
    assert_eq!(
        indexes,
        vec![vec!["shop".to_owned()]],
        "the `@shop.uninstalled` arm pins `shop` and leaves `surface` open, so the range \
         is every surface of one shop"
    );
}

// ---------------------------------------------------------------------------
// The store and its column catalog
// ---------------------------------------------------------------------------

use hekla::fold_store::{FoldStore, Verdict, catalog, reconcile};
use heklang::Digest;

/// Deploy `source` over `dir` and hand back what it changed.
fn deploy(dir: &std::path::Path, source: &str) -> Vec<hekla::fold_store::Change> {
    let program = program(source);
    let digest = Digest::of(&program);
    let tables = TableDef::all(&program);
    let store = FoldStore::open(&dir.join("tables.db"), &tables).expect("the store opens");
    store.apply(&program, &digest, &tables).expect("applied")
}

fn verdicts(changes: &[hekla::fold_store::Change]) -> Vec<(&str, &str)> {
    changes
        .iter()
        .filter(|change| change.verdict != Verdict::UpToDate)
        .map(|change| (change.column.as_str(), change.verdict.label()))
        .collect()
}

/// **The headline win over a projector, and the one worth not losing.** A column is keyed
/// by its fold's digest entry hash, so editing one fold's arm rebuilds one column and
/// leaves every other column of the same table alone. A projector rebuilds its whole
/// database for the same edit.
#[test]
fn editing_one_fold_rebuilds_one_column() {
    let dir = tempfile::tempdir().unwrap();
    let first = deploy(dir.path(), SPEND);
    assert_eq!(
        verdicts(&first),
        vec![("cancelled", "add"), ("lifetime_spend", "add")],
        "a first deploy adds every column"
    );

    assert!(
        verdicts(&deploy(dir.path(), SPEND)).is_empty(),
        "the same program again changes nothing"
    );

    let edited = deploy(
        dir.path(),
        &SPEND.replace("=> spend + total", "=> spend + total + total"),
    );
    assert_eq!(
        verdicts(&edited),
        vec![("lifetime_spend", "refold")],
        "one fold's arm moved, so one column refolds and the other is untouched"
    );
}

/// A moved signature is a column of another shape under the same name, so it goes and
/// comes back rather than being refolded where it is: its table, its key order or its own
/// type has changed.
#[test]
fn retyping_a_fold_reshapes_its_column_rather_than_refolding_it() {
    let dir = tempfile::tempdir().unwrap();
    deploy(dir.path(), SPEND);
    let retyped = deploy(
        dir.path(),
        &SPEND
            .replace("-> Bool = false", "-> Int = 0")
            .replace("=> true", "=> 1"),
    );
    assert_eq!(verdicts(&retyped), vec![("cancelled", "reshape")]);
}

/// A fold deleted from the program takes its column with it.
#[test]
fn deleting_a_fold_drops_its_column() {
    let dir = tempfile::tempdir().unwrap();
    deploy(dir.path(), SPEND);
    let without = deploy(
        dir.path(),
        "subject Customer(Int)

event @order.placed { customer_id: Customer, order_id: Int, total: Money(2) }
event @order.cancelled { customer_id: Customer }

fold LifetimeSpend(customer_id: Customer) -> spend: Money(2) = 0 {
  on @order.placed(customer_id) { total } => spend + total
}
",
    );
    assert_eq!(verdicts(&without), vec![("cancelled", "drop")]);
}

/// **A column added to a populated table is given the fold's seed, not a placeholder.**
/// `ALTER TABLE ... ADD COLUMN` needs a default for a `NOT NULL` column, and for a key no
/// arm of the new fold has fired for, the seed is the answer the column should hold.
#[test]
fn a_column_added_to_a_populated_table_defaults_to_its_folds_seed() {
    let dir = tempfile::tempdir().unwrap();
    let one_fold = "subject Customer(Int)

event @order.placed { customer_id: Customer, total: Money(2) }

fold Placed(customer_id: Customer) -> Bool = false {
  on @order.placed(customer_id) => true
}
";
    deploy(dir.path(), one_fold);

    // A row the first deploy's column was folded for.
    let path = dir.path().join("tables.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute(
        "INSERT INTO \"customer\" (\"customer\", \"placed\") VALUES (7, 1)",
        [],
    )
    .unwrap();
    drop(conn);

    let added = deploy(
        dir.path(),
        &format!(
            "{one_fold}
fold LifetimeSpend(customer_id: Customer) -> spend: Money(2) = 0 {{
  on @order.placed(customer_id) {{ total }} => spend + total
}}
"
        ),
    );
    assert_eq!(verdicts(&added), vec![("lifetime_spend", "add")]);

    let conn = Connection::open(&path).unwrap();
    let seeded: String = conn
        .query_row(
            "SELECT \"lifetime_spend\" FROM \"customer\" WHERE \"customer\" = 7",
            [],
            |row| row.get(0),
        )
        .expect("the existing row has the new column");
    assert_eq!(
        seeded, "0.00",
        "a Money(2) seed of `0` is the amount at its scale, not an integer"
    );
}

/// Obligation 4: one position for the whole store, so a row is never half updated and a
/// query reads one position. A projector holds its own, which is why this is a property
/// of the file rather than of a table.
#[test]
fn the_store_holds_one_checkpoint_for_every_table() {
    let dir = tempfile::tempdir().unwrap();
    let program = program(SPEND);
    let tables = TableDef::all(&program);
    let store = FoldStore::open(&dir.path().join("tables.db"), &tables).unwrap();
    assert_eq!(store.read_checkpoint().unwrap().get(), 0);

    let tx = store.begin().unwrap();
    store
        .write_checkpoint(tephra::Position::new(42), &tx)
        .unwrap();
    tx.commit().unwrap();
    assert_eq!(store.read_checkpoint().unwrap().get(), 42);
}

/// A refilled column records `built_to = 0`, which is what says it holds its seed and
/// nothing else until whatever follows the log catches it up.
#[test]
fn a_column_that_needs_refolding_records_that_it_holds_nothing_yet() {
    let dir = tempfile::tempdir().unwrap();
    let program = program(SPEND);
    let digest = Digest::of(&program);
    let tables = TableDef::all(&program);
    let path = dir.path().join("tables.db");
    let store = FoldStore::open(&path, &tables).unwrap();
    store.apply(&program, &digest, &tables).unwrap();

    let recorded = store.recorded().unwrap();
    assert_eq!(recorded.len(), 2);
    assert!(recorded.iter().all(|row| row.built_to == 0));
    assert!(
        recorded.iter().all(|row| !row.fold_hash.is_empty()),
        "every named fold earns a digest entry, so every column has an identity"
    );
    assert_eq!(
        recorded
            .iter()
            .map(|row| row.fold.as_str())
            .collect::<Vec<_>>(),
        vec!["Cancelled", "LifetimeSpend"],
    );
}

/// The reconcile is pure so that `hekla plan` can forecast a deploy without a second
/// implementation of the rule, the way `projector::reconcile_from` is reused by
/// `plan::forecast`.
#[test]
fn the_reconcile_is_a_function_of_two_catalogs() {
    let before = catalog(
        &Digest::of(&program(SPEND)),
        &TableDef::all(&program(SPEND)),
    );
    assert!(
        reconcile(&before, &before)
            .iter()
            .all(|c| c.verdict == Verdict::UpToDate)
    );
    assert_eq!(
        reconcile(&[], &before)
            .iter()
            .map(|c| c.verdict)
            .collect::<Vec<_>>(),
        vec![Verdict::Add, Verdict::Add],
        "nothing recorded is every column added"
    );
    assert_eq!(
        reconcile(&before, &[])
            .iter()
            .map(|c| c.verdict)
            .collect::<Vec<_>>(),
        vec![Verdict::Drop, Verdict::Drop],
        "nothing current is every column dropped"
    );
}
