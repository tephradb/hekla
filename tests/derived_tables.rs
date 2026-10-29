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
use heklang::{Interpreter, Value, layout};

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
    let layout = layout(&program);

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
    let filled = apply_batch(
        &store,
        &interpreter,
        &program,
        &layout,
        &tables,
        &records,
        head,
    )
    .expect("the batch applies");

    assert_eq!(filled.records, 4);
    // Three, not four: `@order.placed` names `lifetime_spend` for customers 7 and 9, and
    // `@order.cancelled` names `cancelled` for 9 alone. Customer 7's `cancelled` column is
    // never touched, and holds the seed its `DEFAULT` gave it when the row came into
    // being. That is the per-column write and the seed default doing their jobs together.
    assert_eq!(filled.columns, 3, "{filled:?}");
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
    let layout = layout(&program);

    let mut interpreter = Interpreter::new(&program);
    let first = appended(&mut interpreter, vec![placed(7, 1_000)]);
    let path = dir.path().join("tables.db");
    let store = FoldStore::open(&path, &tables).unwrap();
    store.apply(&digest, &tables).unwrap();
    apply_batch(&store, &interpreter, &program, &layout, &tables, &first, 0).unwrap();

    // A second batch about a different customer.
    let second = vec![Record {
        id: "1".to_owned(),
        position: 1,
        at: 0,
        event: placed(9, 250),
    }];
    interpreter.append(placed(9, 250));
    let filled = apply_batch(&store, &interpreter, &program, &layout, &tables, &second, 1).unwrap();
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
    let layout_one = layout(&first_program);
    let mut interpreter = Interpreter::new(&first_program);
    let records = appended(&mut interpreter, vec![cancelled(7)]);
    let store = FoldStore::open(&path, &tables).unwrap();
    store.apply(&Digest::of(&first_program), &tables).unwrap();
    apply_batch(
        &store,
        &interpreter,
        &first_program,
        &layout_one,
        &tables,
        &records,
        0,
    )
    .unwrap();
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

    let filled = backfill(&store, &interpreter, &tables).unwrap();
    assert_eq!(
        filled.columns, 2,
        "one key, and the two columns that are behind it: {filled:?}"
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
    let layout = layout(&program);
    let mut interpreter = Interpreter::new(&program);
    let records = appended(&mut interpreter, vec![ordered(1, 9001, 5)]);

    let path = dir.path().join("tables.db");
    let store = FoldStore::open(&path, &tables).unwrap();
    store.apply(&Digest::of(&program), &tables).unwrap();
    let filled = apply_batch(
        &store,
        &interpreter,
        &program,
        &layout,
        &tables,
        &records,
        1,
    )
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
    let layout = layout(&program);
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
    apply_batch(
        &store,
        &interpreter,
        &program,
        &layout,
        &tables,
        &records,
        2,
    )
    .unwrap();

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
    apply_batch(
        &store,
        &interpreter,
        &first,
        &layout(&first),
        &tables,
        &records,
        1,
    )
    .unwrap();
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
    apply_batch(
        &store,
        &interpreter,
        &grown,
        &layout(&grown),
        &tables,
        &records,
        2,
    )
    .unwrap();

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
