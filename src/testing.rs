//! `hek test`: heklang's `test` declarations, run against hekla's own world.
//!
//! The comparison logic is not here. `heklang::run_tests_in` owns what `expect` means,
//! and this supplies the [`World`] it runs in: a real tephra log, real SQLite read
//! models, a real key store, and a stubbed network. One definition of the assertions,
//! two worlds, which is what stops `hek test` and the language's own suite from
//! drifting into two dialects.
//!
//! What that buys over the in-memory harness is everything below the seam. A subject
//! column really is ciphertext here, so `erased … / project … / expect Row { field:
//! none }` exercises crypto-shredding rather than a flag; an append really goes through
//! the Dynamic Consistency Boundary; and a row really round-trips through the column
//! types the read API will serve it from.

use std::collections::HashMap;
use std::path::Path;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use heklang::host::Rows;
use heklang::interp::{Error, ErrorKind};
use heklang::value::Key;
use heklang::{Event, Program, Reply, Row, TestOutcome, World};
use tempfile::TempDir;
use tephra::{SegmentConfig, SegmentSet, WriteCoordinator, WriterConfig};

use crate::cli;
use crate::context::CommandContext;
use crate::crypto::{KeyStore, MasterKeys};
use crate::heklang_host::{HeklaHost, RowWriter};
use crate::http::{HttpClient, HttpRequest, HttpResponse};
use crate::loader::LoadedProject;
use crate::opdb::OpDb;
use crate::read_model::ReadModel;
use crate::schema::{EntityDef, EventDefs};
use crate::store::Store;

/// Throwaway per-test stores stay small, but the segment must still clear the writer's
/// default max batch size.
const SEGMENT_SIZE: usize = 16 * 1024 * 1024;

/// The append time every test runs at, so a pinned `now()` is written down rather than
/// observed.
const TEST_NOW: &str = "1970-01-01T00:00:00Z";

/// A fixed master key. Tests exercise the real encryption path, so they need a real
/// key; making it a constant keeps a run reproducible.
const TEST_MASTER_KEY: [u8; 32] = [0x2a; 32];

/// Run every `test` declaration under a project directory.
pub fn run(dir: &Path) -> ExitCode {
    let project = LoadedProject::load(dir);
    let findings = cli::collect_findings(&project);
    let mut failed = false;
    for finding in &findings {
        eprintln!("{}", cli::render_finding(finding));
        failed |= matches!(finding.severity, crate::loader::Severity::Error);
    }
    if failed {
        eprintln!("error: the project has errors; fix them before running tests");
        return ExitCode::FAILURE;
    }

    let program = Arc::new(project.program);
    let events = Arc::new(project.events);
    let entities = entity_map(&program);

    let mut built = Ok(());
    let results = heklang::run_tests_in(&program, &mut || match HeklaWorld::fresh(
        &program, &events, &entities,
    ) {
        Ok(world) => Ok(world),
        Err(err) => {
            built = Err(format!("{err:#}"));
            Err(Error::new(ErrorKind::Host(format!("{err:#}"))))
        }
    });
    if let Err(why) = built {
        eprintln!("error: could not build a test world: {why}");
        return ExitCode::FAILURE;
    }

    let mut passed = 0usize;
    let mut failures = 0usize;
    for result in &results {
        match &result.outcome {
            TestOutcome::Passed => {
                passed += 1;
                println!("ok: {:?}", result.name);
            }
            TestOutcome::Failed(why) => {
                failures += 1;
                println!("FAIL: {:?}: {why}", result.name);
            }
            TestOutcome::Errored(why) => {
                failures += 1;
                println!("ERROR: {:?}: {why}", result.name);
            }
        }
    }
    println!("\n{passed} passed, {failures} failed");
    if failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Every entity in the program, with the projector that declares it.
///
/// A test names one projector but a world is built before the action is read, so the
/// read models hold every entity and a write is resolved back to its projector by name.
fn entity_map(program: &Program) -> HashMap<String, (String, EntityDef)> {
    let mut map = HashMap::new();
    for projector in &program.projectors {
        for entity in EntityDef::all(program, projector) {
            map.insert(entity.name.clone(), (projector.name.clone(), entity));
        }
    }
    map
}

/// One test's world: its own log, its own read models, its own keys.
pub struct HeklaWorld {
    host: HeklaHost,
    rows: TestRows,
    http: Arc<ScriptedHttp>,
    keystore: Option<Arc<KeyStore>>,
}

impl HeklaWorld {
    fn fresh(
        program: &Arc<Program>,
        events: &Arc<EventDefs>,
        entities: &HashMap<String, (String, EntityDef)>,
    ) -> anyhow::Result<HeklaWorld> {
        let dir = TempDir::new()?;
        let set = SegmentSet::open(dir.path().join("events"), SegmentConfig::new(SEGMENT_SIZE))?;
        let (coordinator, store) = WriteCoordinator::start(set, WriterConfig::default())?;
        let store = Store::writing(store);

        let opdb = Arc::new(Mutex::new(OpDb::open(&dir.path().join("hekla.db"))?));
        let keystore = Some(Arc::new(KeyStore::new(
            opdb,
            MasterKeys::new(TEST_MASTER_KEY, Vec::new()),
        )));

        let tables: Vec<EntityDef> = entities
            .values()
            .map(|(_, entity)| entity.clone())
            .collect();
        let model = ReadModel::open(&dir.path().join("read.db"), &tables)?;

        let http = Arc::new(ScriptedHttp::default());
        let host = HeklaHost {
            program: Arc::clone(program),
            events: Arc::clone(events),
            store: store.clone(),
            keystore: keystore.clone(),
            ctx: CommandContext::new(uuid::Uuid::nil()),
            now: TEST_NOW.to_owned(),
            idem_tag: None,
            // Only an effect's `invoke` keys an append on a journaled call.
            call: None,
            appended: None,
            emitted: Vec::new(),
            unavailable: None,
            duplicated: false,
            http: Some(Arc::clone(&http) as Arc<dyn HttpClient>),
            // Written down rather than observed, so `Uuid.derive(e.id, ..)` is
            // something a test can spell.
            retry_after: None,
            last_transport: None,
            minted: Some(0),
            sealed: false,
        };
        Ok(HeklaWorld {
            rows: TestRows {
                model,
                program: Arc::clone(program),
                tables: entities
                    .iter()
                    .map(|(name, (_, entity))| (name.clone(), entity.clone()))
                    .collect(),
                owners: entities.clone(),
                keystore: keystore.clone(),
                _coordinator: coordinator,
                _dir: dir,
            },
            host,
            http,
            keystore,
        })
    }
}

impl World for HeklaWorld {
    type Host = HeklaHost;
    type Rows = TestRows;

    fn given(&mut self, event: Event) -> Result<(), Error> {
        // Through the same lowering a live append uses, so a seeded event is byte for
        // byte what a command would have written: envelope, tags and ciphertext.
        heklang::host::Log::append(
            &mut self.host,
            &[event],
            &heklang::AppendCondition {
                after: 0,
                slices: Vec::new(),
            },
        )
    }

    fn respond(&mut self, url: &str, reply: Reply) -> Result<(), Error> {
        self.http.script(url, reply);
        Ok(())
    }

    fn erased(&mut self, subject: &str, id: &str) -> Result<(), Error> {
        let Some(keystore) = &self.keystore else {
            return Ok(());
        };
        // A key that was never created is already erased, which is what the key store
        // answers. Creating one only to destroy it would be the same state by a longer
        // road, so a subject with no data needs nothing here.
        keystore
            .erase(subject, id)
            .map(|_| ())
            .map_err(|err| Error::new(ErrorKind::Host(err.to_string())))
    }

    fn open(self) -> Result<(HeklaHost, TestRows), Error> {
        Ok((self.host, self.rows))
    }
}

/// A test's read models: every projector's entities in one database, resolved back to
/// the projector that declared each.
pub struct TestRows {
    model: ReadModel,
    program: Arc<Program>,
    owners: HashMap<String, (String, EntityDef)>,
    /// The same entities keyed for the SQL layer, which names a table not a projector.
    tables: HashMap<String, EntityDef>,
    keystore: Option<Arc<KeyStore>>,
    /// The fixture, kept alive by whatever `open` handed back.
    ///
    /// `World::open` consumes the world, so anything the run still needs has to leave
    /// with the pair it returns. The read models are in this directory too, which is
    /// why they are the half that owns it. Declared last so they drop last: the
    /// connection closes before the directory goes.
    _coordinator: WriteCoordinator,
    _dir: TempDir,
}

impl TestRows {
    fn writer(&self, entity: &str) -> Result<RowWriter<'_>, Error> {
        let (projector, _) = self.owners.get(entity).ok_or_else(|| {
            Error::new(ErrorKind::Host(format!(
                "entity `{entity}` is not declared"
            )))
        })?;
        let declared = self.program.projector(projector).ok_or_else(|| {
            Error::new(ErrorKind::Host(format!(
                "projector `{projector}` is not declared"
            )))
        })?;
        Ok(RowWriter {
            model: &self.model,
            program: &self.program,
            projector: declared,
            entities: &self.tables,
            keystore: self.keystore.as_deref(),
        })
    }
}

impl Rows for TestRows {
    fn row(&self, entity: &str, key: &Key) -> Result<Option<Row>, Error> {
        self.writer(entity)?.row(entity, key)
    }

    fn put(&mut self, entity: &String, key: Key, row: Row) -> Result<(), Error> {
        let writer = self.writer(entity)?;
        let mut writer = writer;
        writer.put(entity, key, row)
    }

    fn delete(&mut self, entity: &String, key: &Key) -> Result<(), Error> {
        let writer = self.writer(entity)?;
        let mut writer = writer;
        writer.delete(entity, key)
    }
}

/// The network a test scripts: a queue of replies per URL, taken in order.
#[derive(Default)]
struct ScriptedHttp {
    replies: Mutex<HashMap<String, Vec<Reply>>>,
}

impl ScriptedHttp {
    fn script(&self, url: &str, reply: Reply) {
        self.replies
            .lock()
            .expect("the reply queue is not poisoned")
            .entry(url.to_owned())
            .or_default()
            .push(reply);
    }
}

impl HttpClient for ScriptedHttp {
    fn send(&self, request: &HttpRequest) -> anyhow::Result<HttpResponse> {
        let mut replies = self
            .replies
            .lock()
            .expect("the reply queue is not poisoned");
        let queued = replies
            .get_mut(&request.url)
            .filter(|queue| !queue.is_empty());
        // An unscripted URL answers 404 rather than failing the transport: a test that
        // did not mention it is saying the call should not have mattered, and a 404 is
        // a decidable answer the handler can act on.
        let Some(queue) = queued else {
            return Ok(HttpResponse {
                status: 404,
                headers: Vec::new(),
                body: Vec::new(),
            });
        };
        match queue.remove(0) {
            Reply::Status(status) => Ok(HttpResponse {
                status,
                headers: Vec::new(),
                body: Vec::new(),
            }),
            Reply::Body(status, body) => Ok(HttpResponse {
                status,
                headers: Vec::new(),
                body: crate::heklang_host::from_heklang_json(&body)
                    .to_string()
                    .into_bytes(),
            }),
            Reply::Transport(why) => anyhow::bail!("{why}"),
        }
    }
}
