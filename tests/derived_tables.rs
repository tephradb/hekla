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
    store.apply(&digest, &tables).expect("applied")
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
    assert_eq!(store.read_checkpoint().unwrap(), 0);

    let tx = store.begin().unwrap();
    store.write_checkpoint(42, &tx).unwrap();
    tx.commit().unwrap();
    assert_eq!(store.read_checkpoint().unwrap(), 42);
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
    store.apply(&digest, &tables).unwrap();

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
    )
    .unwrap();
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

// ---------------------------------------------------------------------------
// Filling the tables
// ---------------------------------------------------------------------------

use hekla::fold_runtime::{apply_batch, backfill};
use heklang::EventPath;
use heklang::value::{Event, Record};
use heklang::{Interpreter, Value};

/// Append `events` to an interpreter's log and hand back the records a batch would carry.
///
/// The envelope is the harness's, and the positions are the ones `fold_stored` will see,
/// which is what makes the comparison below a comparison and not a coincidence.
fn appended<'a>(interpreter: &mut Interpreter<'a>, events: Vec<Event>) -> Vec<Record> {
    let mut records = Vec::new();
    for (at, event) in events.into_iter().enumerate() {
        interpreter.append(event.clone());
        records.push(Record {
            id: format!("{at}"),
            position: at as u64,
            at: 0,
            event,
        });
    }
    records
}

fn placed(customer: i64, total: i64) -> Event {
    Event::new(
        EventPath::new(["order", "placed"]),
        [
            ("customer_id".to_owned(), Value::Int(customer)),
            ("order_id".to_owned(), Value::Int(1)),
            ("total".to_owned(), Value::money(total, 2)),
        ],
    )
}

fn cancelled(customer: i64) -> Event {
    Event::new(
        EventPath::new(["order", "cancelled"]),
        [("customer_id".to_owned(), Value::Int(customer))],
    )
}

fn cell(conn: &Connection, sql: &str) -> Option<String> {
    conn.query_row(sql, [], |row| row.get::<_, Option<String>>(0))
        .unwrap()
}

/// **The check the whole design rests on: the store and the read cannot disagree, because
/// one is the other's definition.** A column is filled by `Interpreter::fold_stored` and
/// served by `Interpreter::fold`, so every stored value is compared here against folding
/// the same fold for the same key directly.
#[test]
fn a_filled_column_holds_what_folding_it_returns() {
    let dir = tempfile::tempdir().unwrap();
    let program = program(SPEND);
    let digest = Digest::of(&program);
    let tables = TableDef::all(&program);

    let mut interpreter = Interpreter::new(&program);
    let records = appended(
        &mut interpreter,
        vec![
            placed(7, 1_000),
            placed(7, 500),
            cancelled(9),
            placed(9, 250),
        ],
    );
    let head = records.len() as u64;

    let path = dir.path().join("tables.db");
    let store = FoldStore::open(&path, &tables).unwrap();
    store.apply(&digest, &tables).unwrap();
    let filled = apply_batch(&store, &program, &tables, &records, head).expect("the batch applies");

    assert_eq!(filled.records, 4);
    // Four cell writes, not four columns: `@order.placed` names `lifetime_spend` for
    // customer 7 twice, once per event, and for 9 once, and `@order.cancelled` names
    // `cancelled` for 9. Customer 7's `cancelled` column is never touched and holds the
    // seed its `DEFAULT` gave it when the row came into being, which is the per-column
    // write and the seed default doing their jobs together.
    assert_eq!(filled.columns, 4, "{filled:?}");
    assert_eq!(
        store.read_checkpoint().unwrap(),
        head,
        "the values and the position commit together"
    );

    let conn = Connection::open(&path).unwrap();
    for customer in [7, 9] {
        for (column, fold) in [
            ("lifetime_spend", "LifetimeSpend"),
            ("cancelled", "Cancelled"),
        ] {
            let stored = cell(
                &conn,
                &format!(
                    "SELECT CAST(\"{column}\" AS TEXT) FROM \"customer\" WHERE \"customer\" = {customer}"
                ),
            );
            let folded = interpreter
                .fold(fold, [("customer_id".to_owned(), Value::Int(customer))])
                .expect("folded");
            let expected = match folded {
                Value::Bool(flag) => Some((if flag { "1" } else { "0" }).to_owned()),
                other => Some(other.to_string()),
            };
            assert_eq!(
                stored, expected,
                "`{column}` of customer {customer} must be what `{fold}` folds to"
            );
        }
    }

    assert_eq!(
        cell(
            &conn,
            "SELECT CAST(\"cancelled\" AS TEXT) FROM \"customer\" WHERE \"customer\" = 7"
        ),
        Some("0".to_owned()),
        "a column no arm named holds its seed, which is what folding it would return"
    );
}

/// A record names the columns it could have changed, and nothing else is touched. The
/// other key's row is not rewritten, which is what makes a two-column write a two-column
/// write rather than a row rewrite.
#[test]
fn a_batch_touches_only_the_keys_its_records_name() {
    let dir = tempfile::tempdir().unwrap();
    let program = program(SPEND);
    let digest = Digest::of(&program);
    let tables = TableDef::all(&program);

    let mut interpreter = Interpreter::new(&program);
    let first = appended(&mut interpreter, vec![placed(7, 1_000)]);
    let path = dir.path().join("tables.db");
    let store = FoldStore::open(&path, &tables).unwrap();
    store.apply(&digest, &tables).unwrap();
    apply_batch(&store, &program, &tables, &first, 0).unwrap();

    // A second batch about a different customer.
    let second = vec![Record {
        id: "1".to_owned(),
        position: 1,
        at: 0,
        event: placed(9, 250),
    }];
    interpreter.append(placed(9, 250));
    let filled = apply_batch(&store, &program, &tables, &second, 1).unwrap();
    assert_eq!(
        filled.columns, 1,
        "one record, one arm, one key, one column: {filled:?}"
    );
}

/// A column a deploy added records `built_to = 0`, and this is the pass that answers it.
/// Only the column that is behind is folded, which is obligation 3 holding through a
/// backfill as well as through a deploy.
#[test]
fn a_backfill_fills_the_column_that_is_behind_and_no_other() {
    let dir = tempfile::tempdir().unwrap();
    let one_fold = "subject Customer(Int)

event @order.placed { customer_id: Customer, order_id: Int, total: Money(2) }
event @order.cancelled { customer_id: Customer }

fold Cancelled(customer_id: Customer) -> Bool = false {
  on @order.cancelled(customer_id) => true
}
";
    let path = dir.path().join("tables.db");

    // Deploy one fold and fill it, so the table has a row.
    let first_program = program(one_fold);
    let tables = TableDef::all(&first_program);
    let mut interpreter = Interpreter::new(&first_program);
    let records = appended(&mut interpreter, vec![cancelled(7)]);
    let store = FoldStore::open(&path, &tables).unwrap();
    store.apply(&Digest::of(&first_program), &tables).unwrap();
    apply_batch(&store, &first_program, &tables, &records, 0).unwrap();
    drop(store);

    // Now deploy a second fold over the same data directory.
    let grown = program(SPEND);
    let tables = TableDef::all(&grown);
    let store = FoldStore::open(&path, &tables).unwrap();
    let changes = store.apply(&Digest::of(&grown), &tables).unwrap();
    assert_eq!(
        changes
            .iter()
            .filter(|c| c.verdict != Verdict::UpToDate)
            .map(|c| (c.column.as_str(), c.verdict.label()))
            .collect::<Vec<_>>(),
        vec![("lifetime_spend", "add")]
    );
    assert_eq!(
        store.behind().unwrap().len(),
        0,
        "the checkpoint is still 0 here, so nothing is behind it yet"
    );

    // One event later, the new column is behind and the backfill answers it.
    let mut interpreter = Interpreter::new(&grown);
    interpreter.append(cancelled(7));
    interpreter.append(placed(7, 400));
    let tx = store.begin().unwrap();
    // The log is two events long now, so the head is 2. Both columns are behind it, and
    // honestly so: the first was folded at a head of 1 and the checkpoint moved past it
    // here by hand. In a running store `apply_batch` moves a caught-up column and the
    // checkpoint together, so only a column a deploy reset is ever behind.
    store.write_checkpoint(2, &tx).unwrap();
    tx.commit().unwrap();
    assert!(
        store
            .behind()
            .unwrap()
            .iter()
            .any(|row| row.column == "lifetime_spend"),
        "the added column is behind"
    );

    let filled = backfill(&store, &grown, &interpreter, &tables).unwrap();
    assert_eq!(
        filled.columns, 2,
        "two records replayed, each written to the one behind column it names: {filled:?}"
    );
    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        cell(
            &conn,
            "SELECT \"lifetime_spend\" FROM \"customer\" WHERE \"customer\" = 7"
        ),
        Some("4.00".to_owned())
    );
    assert!(
        store.behind().unwrap().is_empty(),
        "and nothing is behind now"
    );
}

// ---------------------------------------------------------------------------
// What a review found, pinned
// ---------------------------------------------------------------------------

const LINES: &str = "subject Shop(Int)

event @order.placed { shop_id: Shop, order_id: Int, line: Int }

fold OrderLines(shop_id: Shop, order_id: Int) -> lines: List(Int) = [] {
  on @order.placed(shop_id, order_id) { line } => lines.push(line)
}

fold OrderPlaced(shop_id: Shop, order_id: Int) -> Bool = false {
  on @order.placed(shop_id, order_id) => true
}
";

fn ordered(shop: i64, order: i64, line: i64) -> Event {
    Event::new(
        EventPath::new(["order", "placed"]),
        [
            ("shop_id".to_owned(), Value::Int(shop)),
            ("order_id".to_owned(), Value::Int(order)),
            ("line".to_owned(), Value::Int(line)),
        ],
    )
}

/// `layout` reports a collection column under the key signature it belongs to, and
/// `TableDef::all` puts it in the side table, so a batch that resolved a column by the
/// layout's table name alone found the scalars-only table and failed outright.
#[test]
fn a_batch_fills_a_collection_column_in_its_side_table() {
    let dir = tempfile::tempdir().unwrap();
    let program = program(LINES);
    let tables = TableDef::all(&program);
    let mut interpreter = Interpreter::new(&program);
    let records = appended(&mut interpreter, vec![ordered(1, 9001, 5)]);

    let path = dir.path().join("tables.db");
    let store = FoldStore::open(&path, &tables).unwrap();
    store.apply(&Digest::of(&program), &tables).unwrap();
    let filled = apply_batch(&store, &program, &tables, &records, 1)
        .expect("a collection column is filled rather than failing the batch");
    assert_eq!(
        filled.columns, 2,
        "one scalar and one collection: {filled:?}"
    );

    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        cell(&conn, "SELECT \"order_lines\" FROM \"_side_shop__order\""),
        Some("[5]".to_owned()),
        "the list stores as the JSON it goes on the wire as"
    );
}

/// An arm leaving a key part open is a range over every key carrying the parts it pinned,
/// and a key this same batch minted is one of them. Reading the range before the pinned
/// writes missed it, and the column was then marked built holding its `DEFAULT`.
#[test]
fn a_range_arm_sees_a_key_the_same_batch_minted() {
    let source = "subject Shop(Int)

event @shop.rendered { shop_id: Shop, surface: String }
event @shop.hidden { shop_id: Shop, surface: String }
event @shop.uninstalled { shop_id: Shop }

fold ShopRendered(shop_id: Shop, surface: String) -> Bool = false {
  on @shop.rendered(shop_id, surface) => true
}

// `surface` is pinned by the hidden arm, which is what makes this storable, and the
// uninstall arm leaves it open. Nothing in the batch below fires the hidden arm, so this
// column's only touch is the range, and the key it ranges over is one the other column's
// write mints in the same batch.
fold ShopGone(shop_id: Shop, surface: String) -> Bool = false {
  on @shop.hidden(shop_id, surface) => true
  on @shop.uninstalled(shop_id) => true
}
";
    let dir = tempfile::tempdir().unwrap();
    let program = program(source);
    let tables = TableDef::all(&program);
    let mut interpreter = Interpreter::new(&program);
    let records = appended(
        &mut interpreter,
        vec![
            Event::new(
                EventPath::new(["shop", "rendered"]),
                [
                    ("shop_id".to_owned(), Value::Int(1)),
                    ("surface".to_owned(), Value::str("a")),
                ],
            ),
            Event::new(
                EventPath::new(["shop", "uninstalled"]),
                [("shop_id".to_owned(), Value::Int(1))],
            ),
        ],
    );

    let path = dir.path().join("tables.db");
    let store = FoldStore::open(&path, &tables).unwrap();
    store.apply(&Digest::of(&program), &tables).unwrap();
    apply_batch(&store, &program, &tables, &records, 2).unwrap();

    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        cell(
            &conn,
            "SELECT CAST(\"shop_gone\" AS TEXT) FROM \"shop__surface\" WHERE \"surface\" = 'a'"
        ),
        Some("1".to_owned()),
        "the uninstall arm ranges over the surface the render arm created in this batch"
    );
}

/// A column a deploy reset must not be advanced by the next batch. Advancing it made
/// `behind` empty, left the backfill nothing to do, and stranded the column on its seed
/// for every key that existed before the deploy.
#[test]
fn a_batch_does_not_advance_a_column_that_is_behind() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tables.db");
    let one = "subject Customer(Int)

event @order.placed { customer_id: Customer, order_id: Int, total: Money(2) }
event @order.cancelled { customer_id: Customer }

fold Cancelled(customer_id: Customer) -> Bool = false {
  on @order.cancelled(customer_id) => true
}
";
    // Fill one fold over one event, so the table has a key.
    let first = program(one);
    let tables = TableDef::all(&first);
    let mut interpreter = Interpreter::new(&first);
    let records = appended(&mut interpreter, vec![cancelled(7)]);
    let store = FoldStore::open(&path, &tables).unwrap();
    store.apply(&Digest::of(&first), &tables).unwrap();
    apply_batch(&store, &first, &tables, &records, 1).unwrap();
    drop(store);

    // Add a second fold, which records `built_to = 0`, then run a batch that touches only
    // the first fold's column.
    let grown = program(SPEND);
    let tables = TableDef::all(&grown);
    let store = FoldStore::open(&path, &tables).unwrap();
    store.apply(&Digest::of(&grown), &tables).unwrap();
    let mut interpreter = Interpreter::new(&grown);
    interpreter.append(cancelled(7));
    let records = vec![Record {
        id: "1".to_owned(),
        position: 1,
        at: 0,
        event: cancelled(9),
    }];
    interpreter.append(cancelled(9));
    apply_batch(&store, &grown, &tables, &records, 2).unwrap();

    assert!(
        store
            .behind()
            .unwrap()
            .iter()
            .any(|row| row.column == "lifetime_spend"),
        "the added column is still behind, so the backfill still has work"
    );
}

/// The seed is in the fold's stage and not in its signature, so changing it moves `hash`
/// alone. That would have been a refold, the column would have stayed, and SQLite cannot
/// alter a `DEFAULT`: every row created afterwards would hold the old seed.
#[test]
fn changing_only_a_folds_seed_reshapes_its_column() {
    let dir = tempfile::tempdir().unwrap();
    deploy(dir.path(), SPEND);
    let reseeded = deploy(
        dir.path(),
        &SPEND.replace("-> Bool = false", "-> Bool = true"),
    );
    assert_eq!(verdicts(&reseeded), vec![("cancelled", "reshape")]);

    let conn = Connection::open(dir.path().join("tables.db")).unwrap();
    let ddl: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name = 'customer'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        ddl.contains("\"cancelled\" INTEGER NOT NULL DEFAULT 1"),
        "the live default is the new seed: {ddl}"
    );
}

/// Every named fold earns a digest entry, so a column without one is an invariant
/// violation. Two empty hashes compare equal, so defaulting it would have read as up to
/// date on every deploy and never rebuilt the column.
#[test]
fn a_column_whose_fold_has_no_digest_entry_is_loud() {
    let program = program(SPEND);
    let tables = TableDef::all(&program);
    let empty = Digest::of(&heklang::Program::default());
    let why = catalog(&empty, &tables).expect_err("no entry, no identity");
    assert!(
        why.to_string().contains("has no digest entry"),
        "the message names the cause: {why}"
    );
}

// ---------------------------------------------------------------------------
// Sealed columns
// ---------------------------------------------------------------------------

use hekla::fold_store::SealedCell;

/// Keyed on an order and sealed under a customer, which is the shape four of the ported
/// application's sealed columns have: no column of the row is a customer id.
const CONTACT: &str = "subject Customer(Int)

event @order.contact.recorded { order_id: Int, customer_id: Customer, email: String? @subject(customer_id) }
event @order.contact.touched { order_id: Int }

fold OrderEmail(order_id: Int) -> held: Sealed(String, Customer)? = none {
  on @order.contact.recorded(order_id) { email } => email
  on @order.contact.touched(order_id) => held
}
";

fn contact(order: i64, sealed: Value) -> Event {
    Event::new(
        EventPath::new(["order", "contact", "recorded"]),
        [
            ("order_id".to_owned(), Value::Int(order)),
            ("customer_id".to_owned(), Value::Int(42)),
            ("email".to_owned(), sealed),
        ],
    )
}

fn sealed_email() -> Value {
    Value::Opt {
        inner: heklang::ir::Type::String,
        value: Some(Box::new(Value::Sealed {
            field: "email".to_owned(),
            subject: "Customer".to_owned(),
            id: "42".to_owned(),
            content: "ciphertext".into(),
        })),
    }
}

/// **A sealed cell carries what opens it.** The row is an order's, so the customer id has
/// to ride in the cell, and the field it was sealed under has to too, since the key store
/// binds that name into the ciphertext. Then an arm that reads the column back gets the
/// same seal it wrote, rather than the refusal it used to.
#[test]
fn a_sealed_column_keyed_off_its_subject_keeps_what_opens_it() {
    let dir = tempfile::tempdir().unwrap();
    let program = program(CONTACT);
    let digest = Digest::of(&program);
    let tables = TableDef::all(&program);
    let path = dir.path().join("tables.db");
    let store = FoldStore::open(&path, &tables).unwrap();
    store.apply(&digest, &tables).unwrap();

    let touched = Event::new(
        EventPath::new(["order", "contact", "touched"]),
        [("order_id".to_owned(), Value::Int(1))],
    );
    let mut interpreter = Interpreter::new(&program);
    let records = appended(&mut interpreter, vec![contact(1, sealed_email()), touched]);
    apply_batch(&store, &program, &tables, &records, 2).expect("a sealed write and its read back");

    let conn = Connection::open(&path).unwrap();
    let stored = cell(
        &conn,
        "SELECT \"order_email\" FROM \"order\" WHERE \"order\" = 1",
    )
    .expect("the column holds the seal");
    assert_eq!(
        SealedCell::parse(&stored).unwrap(),
        SealedCell {
            subject: "Customer".to_owned(),
            id: "42".to_owned(),
            field: "email".to_owned(),
            sealed: "ciphertext".to_owned(),
        }
    );
}

/// Plaintext in a sealed column would be personal data at rest in the clear, and nothing
/// would ever erase it. heklang seals an arm's value before it reaches a host, so this is
/// the store's own backstop, written to directly.
#[test]
fn a_sealed_column_refuses_plaintext() {
    let dir = tempfile::tempdir().unwrap();
    let program = program(CONTACT);
    let tables = TableDef::all(&program);
    let store = FoldStore::open(&dir.path().join("tables.db"), &tables).unwrap();
    store.apply(&Digest::of(&program), &tables).unwrap();
    let table = &tables[0];
    let column = &table.columns[0];

    let plain = Value::Opt {
        inner: heklang::ir::Type::String,
        value: Some(Box::new(Value::str("ann@example.com"))),
    };
    let tx = store.begin().unwrap();
    let why = store
        .write(&tx, table, &["1".to_owned()], column, &plain)
        .expect_err("refused");
    assert!(format!("{why:#}").contains("handed plaintext"), "{why:#}");
    store
        .write(&tx, table, &["1".to_owned()], column, &sealed_email())
        .expect("a seal is what the column holds");
}

/// A store written before sealed cells were wrapped holds bare ciphertext that reads back
/// as nothing, and no fold's hash moved to say so. The format version is what does, and
/// the answer is to refold from the log rather than to read the old cells.
#[test]
fn a_store_in_another_format_is_emptied_and_refolded() {
    let dir = tempfile::tempdir().unwrap();
    let program = program(CONTACT);
    let digest = Digest::of(&program);
    let tables = TableDef::all(&program);
    let path = dir.path().join("tables.db");
    {
        let store = FoldStore::open(&path, &tables).unwrap();
        store.apply(&digest, &tables).unwrap();
        let mut interpreter = Interpreter::new(&program);
        let records = appended(&mut interpreter, vec![contact(1, sealed_email())]);
        apply_batch(&store, &program, &tables, &records, 1).unwrap();
    }
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("PRAGMA user_version = 0").unwrap();
    drop(conn);

    let store = FoldStore::open(&path, &tables).unwrap();
    assert_eq!(
        store.read_checkpoint().unwrap(),
        0,
        "it follows from the start"
    );
    let changes = store.apply(&digest, &tables).unwrap();
    assert!(
        changes.iter().all(|change| change.verdict == Verdict::Add),
        "every column is new again: {changes:?}"
    );
    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        cell(&conn, "SELECT CAST(COUNT(*) AS TEXT) FROM \"order\""),
        Some("0".to_owned())
    );
}

/// A key whose only write so far is a collection is still a key of its table. Every reader
/// of a signature's keys reads the main table, so a row only in the side table would be
/// invisible to a scan, a range arm and a backfill alike.
#[test]
fn a_key_only_a_collection_was_written_for_is_in_the_main_table() {
    let source = "subject Shop(Int)

event @order.lined { shop_id: Shop, order_id: Int, line: Int }
event @order.flagged { shop_id: Shop, order_id: Int }

fold OrderLines(shop_id: Shop, order_id: Int) -> lines: List(Int) = [] {
  on @order.lined(shop_id, order_id) { line } => lines.push(line)
}

fold OrderFlagged(shop_id: Shop, order_id: Int) -> Bool = false {
  on @order.flagged(shop_id, order_id) => true
}
";
    let dir = tempfile::tempdir().unwrap();
    let program = program(source);
    let tables = TableDef::all(&program);
    let lined = Event::new(
        EventPath::new(["order", "lined"]),
        [
            ("shop_id".to_owned(), Value::Int(1)),
            ("order_id".to_owned(), Value::Int(9001)),
            ("line".to_owned(), Value::Int(5)),
        ],
    );
    let mut interpreter = Interpreter::new(&program);
    let records = appended(&mut interpreter, vec![lined]);
    let path = dir.path().join("tables.db");
    let store = FoldStore::open(&path, &tables).unwrap();
    store.apply(&Digest::of(&program), &tables).unwrap();
    apply_batch(&store, &program, &tables, &records, 1).unwrap();

    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        cell(
            &conn,
            "SELECT CAST(\"order_flagged\" AS TEXT) FROM \"shop__order\" \
             WHERE \"shop\" = 1 AND \"order\" = 9001"
        ),
        Some("0".to_owned()),
        "the main row exists, holding the other column's seed"
    );
}

/// A row whose collection no event has written serves that collection's seed, as a main
/// column holding its `DEFAULT` does: there is no side row to join, and the fold's answer
/// for the key is still `[]`, not nothing.
#[test]
fn a_served_row_holds_the_seed_of_a_collection_never_written() {
    let source = "subject Shop(Int)

event @order.lined { shop_id: Shop, order_id: Int, line: Int }
event @order.flagged { shop_id: Shop, order_id: Int }

fold OrderLines(shop_id: Shop, order_id: Int) -> lines: List(Int) = [] {
  on @order.lined(shop_id, order_id) { line } => lines.push(line)
}

fold OrderFlagged(shop_id: Shop, order_id: Int) -> Bool = false {
  on @order.flagged(shop_id, order_id) => true
}
";
    let dir = tempfile::tempdir().unwrap();
    let program = program(source);
    let tables = TableDef::all(&program);
    let flagged = Event::new(
        EventPath::new(["order", "flagged"]),
        [
            ("shop_id".to_owned(), Value::Int(1)),
            ("order_id".to_owned(), Value::Int(9001)),
        ],
    );
    let mut interpreter = Interpreter::new(&program);
    let records = appended(&mut interpreter, vec![flagged]);
    let path = dir.path().join("tables.db");
    let store = FoldStore::open(&path, &tables).unwrap();
    store.apply(&Digest::of(&program), &tables).unwrap();
    apply_batch(&store, &program, &tables, &records, 1).unwrap();

    let main = hekla::table_read::find(&tables, "shop__order").unwrap();
    let (row, _) = hekla::table_read::get_one(
        &path,
        &tables,
        main,
        &["1".to_owned(), "9001".to_owned()],
        None,
    )
    .unwrap();
    let row = row.expect("the key an event named");
    assert_eq!(row["order_flagged"], true);
    assert_eq!(row["order_lines"], serde_json::json!([]), "{row}");
}

// ---------------------------------------------------------------------------
// A backfill replays the log
// ---------------------------------------------------------------------------

/// Deploy `source` over `path`, then fill it from `events` with the checkpoint at the end
/// of them, the way a running store would have.
fn deployed_and_filled(path: &std::path::Path, source: &str, events: &[Event]) -> u64 {
    let program = program(source);
    let tables = TableDef::all(&program);
    let mut interpreter = Interpreter::new(&program);
    let records = appended(&mut interpreter, events.to_vec());
    let store = FoldStore::open(path, &tables).unwrap();
    store.apply(&Digest::of(&program), &tables).unwrap();
    let head = records.len() as u64;
    apply_batch(&store, &program, &tables, &records, head).unwrap();
    head
}

/// Redeploy `source` over a store already filled from `events`, and backfill what it left
/// behind from the same log.
fn redeployed(path: &std::path::Path, source: &str, events: &[Event]) -> FoldStore {
    let program = program(source);
    let tables = TableDef::all(&program);
    let store = FoldStore::open(path, &tables).unwrap();
    store.apply(&Digest::of(&program), &tables).unwrap();
    let mut interpreter = Interpreter::new(&program);
    for event in events {
        interpreter.append(event.clone());
    }
    backfill(&store, &program, &interpreter, &tables).unwrap();
    assert!(
        store.behind().unwrap().is_empty(),
        "nothing is behind after it"
    );
    store
}

/// **A fold added in a table of its own is filled from the history before it.** Its keys
/// are ones no row holds yet, so a backfill that refolded the rows already there filled
/// nothing, marked the column built, and served an empty table.
#[test]
fn a_fold_added_in_a_new_table_is_filled_from_the_history_before_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tables.db");
    let events = [placed(7, 400), placed(8, 100), placed(7, 50)];
    deployed_and_filled(&path, SPEND, &events);

    let grown = format!(
        "{SPEND}
fold CustomerOrders(customer_id: Customer) -> n: Int = 0 {{
  on @order.placed(customer_id) => n + 1
}}

fold OrdersNumbered(order_id: Int) -> n: Int = 0 {{
  on @order.placed(order_id) => n + 1
}}
"
    );
    redeployed(&path, &grown, &events);

    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        cell(
            &conn,
            "SELECT CAST(\"orders_numbered\" AS TEXT) FROM \"order\" WHERE \"order\" = 1"
        ),
        Some("3".to_owned()),
        "a table that did not exist before the deploy"
    );
    assert_eq!(
        cell(
            &conn,
            "SELECT CAST(\"customer_orders\" AS TEXT) FROM \"customer\" WHERE \"customer\" = 7"
        ),
        Some("2".to_owned()),
        "a column added to a table that did"
    );
}

/// **A range arm in a replay reaches only the keys that existed at its record**, as it did
/// when the log was followed. The store at the checkpoint holds keys minted after that
/// record too, and a range over those would count a bump towards a surface rendered later.
#[test]
fn a_replayed_range_reaches_only_the_keys_minted_before_its_record() {
    let seen = "subject Shop(Int)

event @shop.rendered { shop_id: Shop, surface: String }
event @shop.bumped { shop_id: Shop }

fold SurfaceSeen(shop_id: Shop, surface: String) -> Bool = false {
  on @shop.rendered(shop_id, surface) => true
}
";
    let counted = format!(
        "{seen}
fold SurfaceCount(shop_id: Shop, surface: String) -> n: Int = 0 {{
  on @shop.rendered(shop_id, surface) => n + 1
  on @shop.bumped(shop_id) => n + 10
}}
"
    );
    let events = [bumped(1), rendered(1, "a"), bumped(1), rendered(1, "b")];

    // The answer, from a store that followed the log with the fold already deployed.
    let live = tempfile::tempdir().unwrap();
    deployed_and_filled(&live.path().join("tables.db"), &counted, &events);

    // The same fold deployed after the log, so both surfaces' rows exist before the replay.
    let late = tempfile::tempdir().unwrap();
    let path = late.path().join("tables.db");
    deployed_and_filled(&path, seen, &events);
    redeployed(&path, &counted, &events);

    let sql = "SELECT CAST(\"surface_count\" AS TEXT) FROM \"shop__surface\" WHERE \"surface\" = ";
    for (surface, want) in [("a", "11"), ("b", "1")] {
        let query = format!("{sql}'{surface}'");
        let followed = cell(
            &Connection::open(live.path().join("tables.db")).unwrap(),
            &query,
        );
        assert_eq!(followed.as_deref(), Some(want), "followed live: {surface}");
        assert_eq!(
            cell(&Connection::open(&path).unwrap(), &query),
            followed,
            "replayed, surface {surface}"
        );
    }
}

/// A refolded column starts its replay from the seed, not from what the old fold left in
/// it, which would count every record twice.
#[test]
fn a_refolded_column_is_replayed_from_its_seed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tables.db");
    let events = [placed(7, 400), placed(7, 100)];
    deployed_and_filled(&path, SPEND, &events);
    redeployed(
        &path,
        &SPEND.replace("=> spend + total", "=> spend + total + total"),
        &events,
    );
    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        cell(
            &conn,
            "SELECT \"lifetime_spend\" FROM \"customer\" WHERE \"customer\" = 7"
        ),
        Some("10.00".to_owned())
    );
}

fn rendered(shop: i64, surface: &str) -> Event {
    Event::new(
        EventPath::new(["shop", "rendered"]),
        [
            ("shop_id".to_owned(), Value::Int(shop)),
            ("surface".to_owned(), Value::str(surface)),
        ],
    )
}

fn bumped(shop: i64) -> Event {
    Event::new(
        EventPath::new(["shop", "bumped"]),
        [("shop_id".to_owned(), Value::Int(shop))],
    )
}

/// A collection column added later is replayed into the side table, and its key is made
/// to exist in the main table too, where every reader of the keys looks.
#[test]
fn a_collection_column_added_later_is_replayed_into_its_side_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tables.db");
    let placed_only = LINES.replace(
        "fold OrderLines(shop_id: Shop, order_id: Int) -> lines: List(Int) = [] {
  on @order.placed(shop_id, order_id) { line } => lines.push(line)
}
",
        "",
    );
    let events = [line(1, 9, 3), line(1, 9, 4), line(2, 5, 7)];
    deployed_and_filled(&path, &placed_only, &events);
    redeployed(&path, LINES, &events);

    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        cell(
            &conn,
            "SELECT \"order_lines\" FROM \"_side_shop__order\" WHERE \"shop\" = 1 AND \"order\" = 9"
        ),
        Some("[3,4]".to_owned())
    );
    assert_eq!(
        cell(
            &conn,
            "SELECT \"order_lines\" FROM \"_side_shop__order\" WHERE \"shop\" = 2 AND \"order\" = 5"
        ),
        Some("[7]".to_owned())
    );
}

/// One record that mints a key with a pinned arm and ranges over the same key with
/// another reaches it in a replay as it did live: pinned arms go first within a record.
#[test]
fn a_replayed_record_ranges_over_the_key_it_just_minted() {
    let seen = "subject Shop(Int)

event @shop.rendered { shop_id: Shop, surface: String }
event @shop.bumped { shop_id: Shop }
event @shop.named { shop_id: Shop, surface: String }

fold SurfaceSeen(shop_id: Shop, surface: String) -> Bool = false {
  on @shop.rendered(shop_id, surface) => true
}
";
    let ranged = format!(
        "{seen}
fold SurfaceTouched(shop_id: Shop, surface: String) -> n: Int = 0 {{
  on @shop.named(shop_id, surface) => n
  on @shop.rendered(shop_id) => n + 1
}}
"
    );
    let events = [rendered(1, "a"), rendered(1, "b")];

    let live = tempfile::tempdir().unwrap();
    deployed_and_filled(&live.path().join("tables.db"), &ranged, &events);
    let late = tempfile::tempdir().unwrap();
    let path = late.path().join("tables.db");
    deployed_and_filled(&path, seen, &events);
    redeployed(&path, &ranged, &events);

    let sql =
        "SELECT CAST(\"surface_touched\" AS TEXT) FROM \"shop__surface\" WHERE \"surface\" = ";
    for (surface, want) in [("a", "2"), ("b", "1")] {
        let query = format!("{sql}'{surface}'");
        let followed = cell(
            &Connection::open(live.path().join("tables.db")).unwrap(),
            &query,
        );
        assert_eq!(followed.as_deref(), Some(want), "followed live: {surface}");
        assert_eq!(
            cell(&Connection::open(&path).unwrap(), &query),
            followed,
            "replayed, surface {surface}"
        );
    }
}

/// **A column that is not being rebuilt runs over history from its seed, not its final
/// value.** Its writes are dropped either way, but its body still runs, and doubling a
/// value the log already doubled sixty-two times overflows: the backfill would fail on a
/// column that was right all along, on every boot.
#[test]
fn a_column_left_alone_does_not_replay_from_its_final_value() {
    let doubled = "event @clock.ticked { n: Int }

fold Doubled() -> n: Int = 1 {
  on @clock.ticked() => n * 2
}
";
    let counted = format!(
        "{doubled}
fold Ticks() -> n: Int = 0 {{
  on @clock.ticked() => n + 1
}}
"
    );
    let events: Vec<Event> = (0..62).map(ticked).collect();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tables.db");
    deployed_and_filled(&path, doubled, &events);
    redeployed(&path, &counted, &events);

    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        cell(&conn, "SELECT CAST(\"ticks\" AS TEXT) FROM \"global\""),
        Some("62".to_owned())
    );
    assert_eq!(
        cell(&conn, "SELECT CAST(\"doubled\" AS TEXT) FROM \"global\""),
        Some((1i64 << 62).to_string()),
        "and the column left alone is as it was"
    );
}

fn line(shop: i64, order: i64, line: i64) -> Event {
    Event::new(
        EventPath::new(["order", "placed"]),
        [
            ("shop_id".to_owned(), Value::Int(shop)),
            ("order_id".to_owned(), Value::Int(order)),
            ("line".to_owned(), Value::Int(line)),
        ],
    )
}

fn ticked(n: i64) -> Event {
    Event::new(
        EventPath::new(["clock", "ticked"]),
        [("n".to_owned(), Value::Int(n))],
    )
}

// ---------------------------------------------------------------------------
// A fold keyed through another fold
// ---------------------------------------------------------------------------

const GAMES: &str = "event @game.added { game_id: Int, genre: String }
event @game.regenred { game_id: Int, genre: String }
event @game.purchased { game_id: Int, receipt_id: Int }
event @game.refunded { receipt_id: Int }

fold GameGenre(game_id: Int) -> String = \"unknown\" {
  on @game.added(game_id) { genre } => genre
  on @game.regenred(game_id) { genre } => genre
}

fold ReceiptGame(receipt_id: Int) -> Int = 0 {
  on @game.purchased(receipt_id) { game_id } => game_id
}
";

const GENRE_UNITS: &str = "
fold GenreUnits(genre: String) -> n: Int = 0 {
  on @game.purchased() { game_id } key genre = GameGenre { game_id } => n + 1
  on @game.refunded() { receipt_id }
    key genre = GameGenre { game_id: ReceiptGame { receipt_id } } => n - 1
}
";

fn games() -> Vec<Event> {
    let event = |path: [&str; 2], fields: Vec<(&str, Value)>| {
        Event::new(
            EventPath::new(path),
            fields
                .into_iter()
                .map(|(name, value)| (name.to_owned(), value)),
        )
    };
    let added = |game: i64, genre: &str, path: &str| {
        event(
            ["game", path],
            vec![("game_id", Value::Int(game)), ("genre", Value::str(genre))],
        )
    };
    let purchased = |game: i64, receipt: i64| {
        event(
            ["game", "purchased"],
            vec![
                ("game_id", Value::Int(game)),
                ("receipt_id", Value::Int(receipt)),
            ],
        )
    };
    vec![
        added(1, "rpg", "added"),
        added(2, "sim", "added"),
        purchased(1, 10),
        purchased(2, 11),
        // Game 1 moves genre, so the next sale of it counts towards the new one.
        added(1, "sim", "regenred"),
        purchased(1, 12),
        // And the refund of its first sale subtracts where the game is now, which is the
        // as-of rule applied to the refund rather than to the sale.
        event(["game", "refunded"], vec![("receipt_id", Value::Int(10))]),
    ]
}

fn genre_units(path: &std::path::Path) -> Vec<(String, String)> {
    let conn = Connection::open(path).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT \"genre\", CAST(\"genre_units\" AS TEXT) FROM \"genre\" ORDER BY \"genre\"",
        )
        .unwrap();
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// **A lookup reads the looked-up fold as of each record, followed or replayed.** Added
/// after the log, the column is rebuilt by a replay that has to carry `GameGenre` and
/// `ReceiptGame` beside it at their values back then, since what the store holds for them
/// now is where they ended, and every sale would count towards a game's final genre.
#[test]
fn a_fold_keyed_through_another_is_replayed_as_of_each_record() {
    let all = format!("{GAMES}{GENRE_UNITS}");
    let events = games();

    let live = tempfile::tempdir().unwrap();
    deployed_and_filled(&live.path().join("tables.db"), &all, &events);
    let followed = genre_units(&live.path().join("tables.db"));
    assert_eq!(
        followed,
        vec![
            ("rpg".to_owned(), "1".to_owned()),
            ("sim".to_owned(), "1".to_owned())
        ]
    );

    let late = tempfile::tempdir().unwrap();
    let path = late.path().join("tables.db");
    deployed_and_filled(&path, GAMES, &events);
    redeployed(&path, &all, &events);
    assert_eq!(genre_units(&path), followed, "replayed");

    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        cell(
            &conn,
            "SELECT \"game_genre\" FROM \"game\" WHERE \"game\" = 1"
        ),
        Some("sim".to_owned()),
        "and the folds it looked up are where they ended, not where the replay left them"
    );
}

/// An edit to a looked-up fold refolds what keys through it, in place: its values move,
/// its table and type do not.
#[test]
fn editing_a_looked_up_fold_refolds_what_keys_through_it() {
    let dir = tempfile::tempdir().unwrap();
    let all = format!("{GAMES}{GENRE_UNITS}");
    deploy(dir.path(), &all);
    let edited = deploy(
        dir.path(),
        &all.replace(
            "on @game.regenred(game_id) { genre } => genre",
            "on @game.regenred(game_id) { genre } => \"retired\"",
        ),
    );
    assert_eq!(
        verdicts(&edited),
        vec![("game_genre", "refold"), ("genre_units", "refold")]
    );

    // Both behind at once, so `GenreUnits` reads `GameGenre` out of the store while the
    // same replay rebuilds it, and has to agree with a store that followed the edited
    // program from the start.
    let retired = all.replace(
        "on @game.regenred(game_id) { genre } => genre",
        "on @game.regenred(game_id) { genre } => \"retired\"",
    );
    let events = games();
    let live = tempfile::tempdir().unwrap();
    deployed_and_filled(&live.path().join("tables.db"), &retired, &events);
    let followed = genre_units(&live.path().join("tables.db"));
    assert!(
        followed.contains(&("retired".to_owned(), "0".to_owned())),
        "the edit is visible: {followed:?}"
    );
    let refolded = tempfile::tempdir().unwrap();
    let path = refolded.path().join("tables.db");
    deployed_and_filled(&path, &all, &events);
    redeployed(&path, &retired, &events);
    assert_eq!(genre_units(&path), followed);
}
