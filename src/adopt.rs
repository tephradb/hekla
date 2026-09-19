//! Moving a subject's key to where its declaration says it belongs.
//!
//! A key row's wrapping is chosen when the row is first minted and never revisited, so
//! declaring `under Shop` onto a `Customer` that already has rows leaves every one of
//! them wrapped under the master. New customers hang from their shop; the ones that came
//! before do not, and erasing the shop reports success having missed them. That is the
//! one failure a deletion feature cannot have, because nothing about it looks wrong: the
//! reads work, `hekla verify` is clean, and the only symptom is a shred that quietly did
//! less than it claimed.
//!
//! This module closes it, and the runtime runs it before serving, so a deployment whose
//! stored keys disagree with its declared hierarchy does not reach traffic.
//!
//! # Where the parent comes from
//!
//! Not from the key store, which knows only that customer 88 exists. The fact that 88
//! belongs to shop 7 lives in the events that sealed under it, so this folds the log.
//!
//! That works on events written *before* the parent was declared, which is the whole
//! population in question, because three refusals already in the tree compose:
//! heklang's `check_ancestry` forces the parent id onto every event sealing under a
//! child as a required, plaintext field; [`crate::heklang_host::unanswered_history`]
//! refuses a boot that adds a required field to an event type with stored instances
//! unless it carries `@absent`; and `value::stored_field` materialises that absent value
//! on read. So deploying `under` onto a log is only possible after the author has said
//! what the old events' parent is, and a fold reads it rather than guessing.
//!
//! # The rule
//!
//! A root row is rewrapped under the parent named by the **earliest** event that seals
//! under it. That is not a new rule: the docs already say a key is minted once, under
//! whichever event arrived first, so this replays the decision the write path would have
//! made had the parent been declared from the start.
//!
//! What it never does is re-parent a row that is already a child, in either direction.
//! [`crate::crypto::KeyStore::adopt_in`] carries that reasoning.
//!
//! # What it costs
//!
//! One indexed count per boot, and nothing else once the store is settled. When there is
//! work, the scan is narrowed to the event types that can seal under a parented subject
//! and **stops as soon as every waiting subject has a parent**, which is usually early:
//! the rows waiting are by definition old, so their first events are near the head of
//! the log.
//!
//! Memory is bounded by the number of subjects waiting, not by the length of the log.

use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};
use std::fmt;
use std::thread;
use std::time::Duration;

use anyhow::anyhow;
use heklang::ir::Program;
use tephra::Position;

use crate::crypto::{Adopted, KeyStore};
use crate::heklang_host;
use crate::opdb::SWEEP_CHUNK;
use crate::schema::{self, EventDefs, Subjects};
use crate::store::Store;

/// How many candidate identities to read per page.
const PAGE: usize = 1000;

/// How many passes a run makes before reporting what is left.
///
/// More than one because a pass can move less than it resolved when something else is
/// writing, and because a backlog bigger than [`BATCH`] takes one pass per batch. Bounded
/// because a directory under continuous erase-and-recreate would otherwise never converge,
/// and a boot that says what is left beats one that hangs. Generous, because a pass that
/// filled its batch did useful work and stopping on it would leave the job half done.
const PASSES: usize = 512;

/// How many waiting identities one pass takes on.
///
/// This is the memory bound. A pass holds its batch for the whole fold, so without a cap
/// a first deploy onto a store with millions of subjects allocated all of them inside
/// `Runtime::open`, before the listener was up and while the directory lock was held. Big
/// enough that any ordinary migration is one pass, small enough that the worst case is
/// megabytes rather than gigabytes.
const BATCH: usize = 50_000;

/// How many passes that saw the whole backlog and still left rows behind a run tolerates
/// before reporting what is left. That is contention with a live writer rather than more
/// work to do, and each retry costs a fold of the log.
const CONTENDED_PASSES: usize = 3;

/// How many disagreeing subjects a run names before it stops collecting them. Enough to
/// show the shape of the problem, few enough that the report stays readable.
const DISAGREEMENTS: usize = 20;

/// How many identities a refusal names before it says "and N more". The count carries the
/// size of the problem; the names are there to start looking.
const NAMED: usize = 20;

/// A pause between commit chunks, so a large adoption does not monopolise the operational
/// lock that every command and effect also needs. The same discipline the retention sweep
/// uses, and for the same reason.
const CHUNK_PAUSE: Duration = Duration::from_millis(10);

/// One subject whose events do not agree about its parent.
///
/// Already a documented state rather than a fault: the parent is asserted per event, and
/// nothing can check that two events agree. Reported because this fold is the only thing
/// that ever looks, and an operator who is about to rely on a tenant erase is the one
/// person who wants to know.
#[derive(Debug, Clone)]
pub struct Disagreement {
    pub subject: String,
    pub subject_value: String,
    /// The ancestry taken, from the earliest event seen.
    pub kept: String,
    /// A different one a later event named.
    pub also: String,
}

/// What a pass did.
#[derive(Debug, Default)]
pub struct Adoption {
    /// Rows that moved under their declared parent.
    pub adopted: usize,
    /// Rows that were waiting when the run began, asked of the store once. Not derived
    /// from the passes: a row that waits through two of them is one row, not two.
    ///
    /// On a value returned by `pass` this is that batch's size instead, which `run` reads
    /// to tell "more backlog" from "contention" before `absorb` drops it.
    /// Folding one pass into the run must therefore never accumulate this field: doing so is what made a run
    /// report half again the rows that were actually waiting.
    pub waiting: usize,
    /// Events read to find the parents.
    pub scanned: usize,
    /// Subjects still wrapped under the master because the fold found no event sealing
    /// under them. Unreachable through the declaration refusals above, which is why it is
    /// reported loudly rather than skipped.
    pub unresolved: Vec<(String, String)>,
    /// Subjects whose events named more than one parent. One entry per subject, capped:
    /// only those seen before the scan stopped, so this is what the pass noticed rather
    /// than an exhaustive audit.
    pub disagreements: Vec<Disagreement>,
    /// Rows still wrapped under the master when the last pass finished, re-asked of the
    /// key store rather than inferred.
    ///
    /// Not the same as [`Adoption::unresolved`], and the difference decides what to do: a
    /// subject nothing accounts for will still be there next time, while a row that was
    /// merely not moved (a compare-and-set lost, a row erased since the scan, an adoption
    /// undone because its parent went mid-flight) moves on a later look.
    pub remaining: u64,
    /// Ancestor key rows this run had to create, because the subjects waiting named
    /// ancestors with no key of their own.
    ///
    /// Counted rather than explained: an erased row and one that never existed are the
    /// same absence from here, so this says what happened and leaves why to whoever knows
    /// whether they erased something.
    pub minted_ancestors: usize,
    /// Rows whose adoption returned an error rather than an outcome.
    ///
    /// Carried rather than logged, because these are not contention: a corrupt wrapping,
    /// a row under an unconfigured master, a subject churned past the mint retry. Reading
    /// them as contention told an operator to look for a second process instead of at the
    /// row, and let a deploy gate pass on a store that could not be adopted at all.
    pub failed: usize,
    /// What the first of them said, for a message that names something concrete.
    pub first_failure: Option<String>,
}

impl Adoption {
    /// Whether every key is where the declaration says it is.
    ///
    /// **Asked of the store, not of the bookkeeping.** Counting what the fold resolved
    /// says nothing: `adopt_in` legitimately declines rows, and a subject it declined is
    /// gone from `unresolved` while still sitting under the master. A caller that read
    /// this as "done" would serve, or exit zero, on exactly the state the phase exists to
    /// prevent.
    pub fn settled(&self) -> bool {
        unfinished(self).is_none()
    }

    /// Fold one pass's counts into a run's.
    fn absorb(&mut self, pass: Adoption) {
        self.adopted += pass.adopted;
        self.scanned += pass.scanned;
        self.minted_ancestors += pass.minted_ancestors;
        // The last pass, not the sum. A row that fails once and moves on the next look is
        // not a fault, and summing also counted one stubborn row once per pass, overstating
        // the size of the problem on the line meant to report its size.
        self.failed = pass.failed;
        self.first_failure = pass.first_failure;
        self.unresolved = pass.unresolved;
        let known: BTreeSet<(String, String)> = self
            .disagreements
            .iter()
            .map(|one| (one.subject.clone(), one.subject_value.clone()))
            .collect();
        // The cap again, not just the dedup: `fold` applies it per pass, so three passes
        // each noticing twenty different subjects would otherwise print sixty lines from a
        // constant whose whole point is that the report stays readable.
        self.disagreements.extend(
            pass.disagreements
                .into_iter()
                .filter(|one| !known.contains(&(one.subject.clone(), one.subject_value.clone())))
                .take(DISAGREEMENTS.saturating_sub(known.len())),
        );
    }
}

/// How many rows are waiting, without doing anything about them.
///
/// What a boot asks first and, on a settled store, all it asks. Also what `hekla verify`
/// and `hekla plan` report, since neither may advance state.
pub fn waiting(program: &Program, keystore: &KeyStore) -> anyhow::Result<u64> {
    let subjects = Subjects::of(program);
    let parented = subjects.parented();
    if parented.is_empty() {
        return Ok(0);
    }
    keystore.pending_adoptions(&parented)
}

/// The same question per subject, and only for subjects that actually have rows waiting.
///
/// What `hekla verify` reports with. The flat count cannot say where to look, and naming
/// every subject that declares a parent would put innocent ones in a violation an operator
/// then has to go and clear. One lookup per parented subject, which is a handful, against
/// an index that is empty on a healthy store.
pub fn waiting_by_subject(
    program: &Program,
    keystore: &KeyStore,
) -> anyhow::Result<Vec<(String, u64)>> {
    let subjects = Subjects::of(program);
    let mut found = Vec::new();
    for subject in subjects.parented() {
        let count = keystore.pending_adoptions(&[subject])?;
        if count > 0 {
            found.push((subject.to_owned(), count));
        }
    }
    Ok(found)
}

/// Adopt every root of a parented subject, and report what happened.
///
/// `progress` is called with `(position, resolved)` as the fold advances, which is the
/// half that runs long: the writes that follow are bounded by the number of rows waiting,
/// and the scan is bounded by the log. A caller with nothing to draw ignores both.
pub fn run(
    program: &Program,
    events: &EventDefs,
    store: &Store,
    keystore: &KeyStore,
    progress: &mut dyn FnMut(u64, usize),
) -> anyhow::Result<Adoption> {
    let subjects = Subjects::of(program);
    let parented = subjects.parented();
    if parented.is_empty() {
        return Ok(Adoption::default());
    }
    // Asked once, of the store, before anything moves. Summing the passes double-counted a
    // row that waited through two of them, and latching the first reported one batch of a
    // larger backlog; neither is the number of rows that were waiting.
    let mut done = Adoption {
        waiting: keystore.pending_adoptions(&parented)? as usize,
        ..Adoption::default()
    };
    // A pass takes a batch, folds the log for its parents, and moves them, so a backlog
    // larger than [`BATCH`] is several passes: the batch is what bounds the memory a boot
    // needs, and without it a first deploy onto a store with millions of subjects
    // allocated all of them before a single row moved.
    //
    // Two different reasons to go round again, and they need different budgets, because a
    // pass costs a fold of the log. A pass that **filled** its batch found more waiting
    // than it could take, so there is backlog left and going again is how the job gets
    // done: those do not count against the limit below. A pass that did not fill its batch
    // saw everything there was and still left some behind, which is contention with a live
    // writer, and repeating that is the case where the answer is to say what is left
    // rather than to keep decoding the whole log hoping to win a race.
    let mut contended = 0;
    for _ in 0..PASSES {
        let before = done.adopted;
        let batch = pass(program, events, store, keystore, &parented, progress)?;
        let was_full = batch.waiting >= BATCH;
        done.absorb(batch);
        // Asked before any of the breaks below, so every verdict this run reaches is one
        // the store has been consulted about. Breaking on `unresolved` first left
        // `remaining` at zero, which made that one verdict the exception: an operator who
        // erased the offending subject while `hekla adopt` was folding would be refused,
        // and told to add an `@absent` for a key that no longer exists.
        done.remaining = keystore.pending_adoptions(&parented)?;
        if done.remaining == 0 {
            break;
        }
        // A subject no event accounts for will not be accounted for by looking again.
        if !done.unresolved.is_empty() {
            break;
        }
        // Nothing moved at all, so whatever is holding those rows is still holding them.
        if done.adopted == before {
            break;
        }
        if !was_full {
            contended += 1;
            if contended >= CONTENDED_PASSES {
                break;
            }
        }
    }
    Ok(done)
}

/// One pass: list what is waiting, fold the log for their parents, move them.
fn pass(
    program: &Program,
    events: &EventDefs,
    store: &Store,
    keystore: &KeyStore,
    parented: &[&str],
    progress: &mut dyn FnMut(u64, usize),
) -> anyhow::Result<Adoption> {
    let mut pending = candidates(keystore, parented)?;
    if pending.is_empty() {
        return Ok(Adoption::default());
    }
    let waiting = pending.len();

    let (resolved, scanned, disagreements) = fold(program, events, store, &mut pending, progress)?;

    // Which ancestors have no key yet, asked once for the pass rather than once per row.
    // Adopting mints them, and an operator who erased one is owed the fact that its
    // identity is back; the key store cannot say *why* it was absent (an erased row and
    // one that never existed are the same absence), so this counts rather than explains.
    //
    // **Observed, not inferred**, and the third attempt at that. Which rows the mint
    // actually creates is not derivable from `adopt_in`'s outcome in either direction:
    // `Contended` and `Undone` both return *after* the mint, so they can have created one,
    // and `Moved` does not imply it, because `mint_secret_in` stops at the first ancestor
    // it can already open and never visits the links above it. So this asks the store
    // which ancestors had no key before the writes, and which of those have one after. It
    // does not care which row did it, which is why it is right.
    //
    // Distinct, too. Asking per row meant one query per member of a tenant to learn one
    // fact about the tenant, which on a full batch is fifty thousand round trips through
    // the shared operational lock, on the boot path, before the listener is up.
    let mut ancestors: BTreeSet<(&str, &str)> = BTreeSet::new();
    for chain in resolved.values() {
        for (subject, value) in &chain[1..] {
            ancestors.insert((subject.as_str(), value.as_str()));
        }
    }
    // Reachability, not presence: a row that is present but unopenable is replaced by a
    // fresh key here, and an identity flipping back to `live` in `/admin/subjects` is
    // exactly the reappearance this number exists to account for.
    let mut absent: BTreeSet<(&str, &str)> = BTreeSet::new();
    for (subject, value) in ancestors {
        if !keystore.is_reachable(subject, value)? {
            absent.insert((subject, value));
        }
    }

    let mut adopted = 0;
    let mut failed = 0;
    let mut first_failure = None;
    for (moved, chain) in resolved.values().enumerate() {
        let borrowed: Vec<(&str, &str)> = chain
            .iter()
            .map(|(subject, value)| (subject.as_str(), value.as_str()))
            .collect();
        // Per row, because one row that cannot be minted is not a reason to abandon the
        // rest.
        //
        // Carried out rather than logged. A failure here reached a `tracing::warn!` that
        // `hekla adopt` installs no subscriber for, so it went nowhere at all, and the run
        // then reported the rows it could not move as *contention* and exited zero: a
        // corrupt row read as "something else is writing", and a deploy gate went green.
        // Losing the mint race is not one of these: `adopt_in` reports that as
        // `Contended`, because it resolves by looking again.
        match keystore.adopt_in(&borrowed) {
            Ok(Adopted::Moved) => adopted += 1,
            Ok(_) => {}
            Err(err) => {
                failed += 1;
                first_failure.get_or_insert_with(|| format!("{err:#}"));
            }
        }
        // Commits are per row, so the pause is what keeps a long run from holding the
        // operational lock against live work. It also makes a killed pass resumable:
        // whatever moved stays moved, and the next pass has less to do.
        if (moved + 1) % SWEEP_CHUNK == 0 {
            thread::sleep(CHUNK_PAUSE);
        }
    }
    // After the writes have committed, so a hiccup here must not throw the run away. This
    // count is a log line; `adopted`, `failed` and what the next pass has left to do are
    // the answers that matter, and raising would discard all three over a diagnostic.
    let mut minted = 0;
    for (subject, value) in absent {
        if keystore.is_reachable(subject, value).unwrap_or(false) {
            minted += 1;
        }
    }

    Ok(Adoption {
        adopted,
        waiting,
        scanned,
        unresolved: pending.into_iter().collect(),
        disagreements,
        minted_ancestors: minted,
        failed,
        first_failure,
        // The run's business, not a pass's: it is re-asked of the store between passes.
        remaining: 0,
    })
}

/// Up to [`BATCH`] roots of a parented subject, paged in.
fn candidates(
    keystore: &KeyStore,
    parented: &[&str],
) -> anyhow::Result<BTreeSet<(String, String)>> {
    let mut found = BTreeSet::new();
    let mut after: Option<(String, String)> = None;
    loop {
        let cursor = after
            .as_ref()
            .map(|(subject, value)| (subject.as_str(), value.as_str()));
        let page = keystore.adoption_candidates(parented, cursor, PAGE)?;
        let Some(last) = page.last().cloned() else {
            break;
        };
        found.extend(page);
        after = Some(last);
        // One batch is all a pass takes on, because the set is held for the whole fold and
        // this is the only thing bounding what a boot allocates. What is left over stays
        // in the store, is counted by the re-ask between passes, and is picked up by the
        // next one.
        if found.len() >= BATCH {
            break;
        }
    }
    Ok(found)
}

/// Walk the log until every waiting subject has a parent, or it runs out.
///
/// Removes from `pending` as it resolves, so what is left when this returns is what no
/// event accounted for.
#[allow(clippy::type_complexity)]
fn fold(
    program: &Program,
    events: &EventDefs,
    store: &Store,
    pending: &mut BTreeSet<(String, String)>,
    progress: &mut dyn FnMut(u64, usize),
) -> anyhow::Result<(
    BTreeMap<(String, String), Vec<(String, String)>>,
    usize,
    Vec<Disagreement>,
)> {
    let mut resolved: BTreeMap<(String, String), Vec<(String, String)>> = BTreeMap::new();
    let mut disagreements = Vec::new();
    let mut reported: BTreeSet<(String, String)> = BTreeSet::new();
    let mut scanned = 0;

    let types = sealing_types(events);
    if types.is_empty() {
        return Ok((resolved, scanned, disagreements));
    }
    let query = heklang_host::query_of_types(&types).map_err(|err| anyhow!("{err}"))?;
    let mut reads = store.read(&query, Position::ZERO, None);
    while let Some(item) = reads.next() {
        let seq = item.map_err(|err| anyhow!("reading the event log: {err}"))?;
        scanned += 1;
        let record = heklang_host::record_of(program, seq.position, seq.event).map_err(|err| {
            anyhow!(
                "reading the event at position {}: {err}",
                seq.position.get()
            )
        })?;
        let ty = schema::event_type(&record.event.path);
        let Some(def) = events.get(&ty) else {
            continue;
        };
        let ids = heklang_host::subject_ids(def, &record.event);
        for (name, meta) in &def.fields {
            let Some(seal) = &meta.sealed_under else {
                continue;
            };
            if seal.chain.len() < 2 {
                continue;
            }
            // An event that sealed nothing here minted no key, so it did not decide where
            // this subject's key lives and must not be read as though it had. Skipping it
            // also leaves the subject pending, so the scan carries on to the event that
            // really minted it rather than stopping one short.
            if !heklang_host::seals_content(&record.event, name) {
                continue;
            }
            let Some(chain) = seal.resolve(|field| ids.get(field).map(String::as_str)) else {
                continue;
            };
            // **Every link of the chain, not just its head.** A chain says where the
            // sealing subject's key goes *and* where each of its ancestors' keys go, and
            // an ancestor is often the only one waiting: `Member under Tenant` already
            // deployed leaves tenant rows minted as ancestors, and declaring
            // `Tenant under Region` makes those the rows to move while every event that
            // could say so still seals under `Member`. Reading only the head left them
            // unresolved for ever and refused the boot with advice the author had already
            // followed. The last link is skipped because it has no ancestors of its own,
            // so there is nothing to file it under.
            for start in 0..chain.len() - 1 {
                let key = (chain[start].0.to_owned(), chain[start].1.to_owned());
                // Only subjects that are waiting, plus ones already resolved so a later
                // event can be noticed disagreeing with the one that won.
                if !pending.contains(&key) && !resolved.contains_key(&key) {
                    continue;
                }
                let owned: Vec<(String, String)> = chain[start..]
                    .iter()
                    .map(|(subject, value)| ((*subject).to_owned(), (*value).to_owned()))
                    .collect();
                keep_first(
                    key,
                    owned,
                    pending,
                    &mut resolved,
                    &mut disagreements,
                    &mut reported,
                );
            }
        }
        progress(seq.position.get(), resolved.len());
        // Everything waiting has a parent, so reading further would only refine the
        // disagreement report. Said in the doc above rather than pretended otherwise.
        if pending.is_empty() {
            break;
        }
    }
    Ok((resolved, scanned, disagreements))
}

/// Keep the first ancestry seen for a subject, and notice a later one that differs.
#[allow(clippy::type_complexity)]
fn keep_first(
    key: (String, String),
    owned: Vec<(String, String)>,
    pending: &mut BTreeSet<(String, String)>,
    resolved: &mut BTreeMap<(String, String), Vec<(String, String)>>,
    disagreements: &mut Vec<Disagreement>,
    reported: &mut BTreeSet<(String, String)>,
) {
    match resolved.entry(key.clone()) {
        Entry::Vacant(slot) => {
            slot.insert(owned);
            pending.remove(&key);
        }
        // One entry per subject, and no more than [`DISAGREEMENTS`] of them. Two events
        // disagreeing is the *expected* shape of a migration, since rows from before the
        // change take the `@absent` tenant and anything written after names a real one,
        // so an entry per mismatching event would grow with the log rather than with the
        // subjects and put one warning line on the operator's screen for each.
        Entry::Occupied(slot)
            if slot.get() != &owned
                && disagreements.len() < DISAGREEMENTS
                && !reported.contains(&key) =>
        {
            reported.insert(key.clone());
            disagreements.push(Disagreement {
                subject: key.0.clone(),
                subject_value: key.1.clone(),
                kept: render(&slot.get()[1..]),
                also: render(&owned[1..]),
            });
        }
        Entry::Occupied(_) => {}
    }
}

/// The event types that can seal something under a subject with ancestors. Narrowing the
/// scan to these is what keeps the fold off event types that could never answer it.
fn sealing_types(events: &EventDefs) -> Vec<String> {
    let mut types: Vec<String> = events
        .values()
        .filter(|def| {
            def.fields.iter().any(|(_, meta)| {
                meta.sealed_under
                    .as_ref()
                    .is_some_and(|seal| seal.chain.len() > 1)
            })
        })
        .map(|def| def.event_type.clone())
        .collect();
    types.sort();
    types
}

/// An ancestry as an operator reads it: ``  `Shop` = `7` under `Market` = `1` ``.
fn render(ancestors: &[(String, String)]) -> String {
    ancestors
        .iter()
        .map(|(subject, value)| format!("`{subject}` = `{value}`"))
        .collect::<Vec<_>>()
        .join(" under ")
}

/// One disagreement, as the boot logs it and as `hekla adopt` prints it.
///
/// Shared for the reason [`unfinished`] is: the two said almost the same sentence and had
/// already drifted apart, with the reference documenting only one of them.
pub fn disagreement_line(wrong: &Disagreement) -> String {
    let (subject, value) = (&wrong.subject, &wrong.subject_value);
    let (kept, also) = (&wrong.kept, &wrong.also);
    format!(
        "subject `{subject}` = `{value}` sits under {kept} by one event and under {also} by another; its key is filed under the first, which is the rule, and only that one's erasure reaches it"
    )
}

/// Why a run did not finish the job.
///
/// The two are not the same kind of trouble, and a caller has to treat them differently.
/// One is a declaration that cannot be satisfied and will be there next time too; the
/// other is just a race with a live writer, which is the *expected* condition for the
/// command that exists to run against a live directory.
#[derive(Debug)]
pub enum Unfinished {
    /// Subjects no event in the log accounts for. Fatal wherever it is found.
    ///
    /// A count and a sample, not the whole population: the message names at most twenty
    /// of them anyway, and a store where a whole subject is unaccounted for would
    /// otherwise copy millions of identities to build one line.
    Unaccounted {
        total: usize,
        sample: Vec<(String, String)>,
    },
    /// Rows that did not move, because something else kept winning the race. Fatal at
    /// boot, where nothing else should be writing and the guarantee has to hold before
    /// serving; ordinary for `hekla adopt` against a running deployment, where new roots
    /// are being minted under the old declaration the whole time it runs.
    Contended(u64),
    /// Rows the key store could not move at all: a corrupt wrapping, an ancestor under a
    /// master this process does not hold, a subject churned past the mint retry.
    ///
    /// Apart from [`Unfinished::Contended`] because the two send an operator to different
    /// places, and collapsing them sent every one of these to look for a second writer
    /// that was not there. Fatal everywhere: contention resolves itself and this does not.
    Failed { count: usize, first: String },
}

/// Why a run left rows behind, or `None` when it did not.
///
/// Its own function because the boot and `hekla adopt` have to agree about what counts as
/// done, and [`Adoption::settled`] is defined as this answering `None` so the two cannot
/// drift into different definitions.
pub fn unfinished(done: &Adoption) -> Option<Unfinished> {
    // **The store settles it, and it goes first.** If nothing is left under the master
    // then the job is done, however badly a pass along the way went: a row that errored in
    // one pass and moved in the next is not a fault, and reporting one refused the boot on
    // a completely adopted store. Nor is a subject the fold could not place, once its key
    // row has stopped being a root: there is nothing left to be wrong about. This is what
    // [`Adoption::settled`]'s doc has always claimed and what reading the bookkeeping
    // instead kept getting wrong, in both directions.
    //
    // Safe to put ahead of `unresolved` only because `run` now refreshes `remaining`
    // before it breaks on one. While it did not, `remaining` was zero on that path and
    // this would have waved every unaccounted row through.
    if done.remaining == 0 {
        return None;
    }
    if !done.unresolved.is_empty() {
        return Some(Unfinished::Unaccounted {
            total: done.unresolved.len(),
            sample: done.unresolved.iter().take(NAMED).cloned().collect(),
        });
    }
    // Something is left, so the question is *why*, and a failure is not a race. Keyed on
    // the count rather than on the message, so the two cannot get out of step.
    if done.failed > 0 {
        return Some(Unfinished::Failed {
            count: done.failed,
            first: done
                .first_failure
                .clone()
                .unwrap_or_else(|| "no detail was recorded".to_owned()),
        });
    }
    Some(Unfinished::Contended(done.remaining))
}

impl fmt::Display for Unfinished {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // Capped, for the reason `DISAGREEMENTS` is: this is one line, and a store
            // where a whole population is unaccounted for would otherwise put a megabyte
            // of identities into it. The count is what says how big the problem is; the
            // names are there to start looking.
            Unfinished::Unaccounted { total, sample } => {
                let named = sample
                    .iter()
                    .map(|(subject, value)| format!("`{subject}` = `{value}`"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let more = match total.saturating_sub(sample.len()) {
                    0 => String::new(),
                    rest => format!(" and {rest} more"),
                };
                let count = total;
                write!(
                    f,
                    "{count} stored key(s) belong to a subject that declares a parent, but no event in the log says which one: {named}{more}. Every one of these is wrapped under the master rather than its tenant, so erasing the tenant would silently miss it. An event sealing under the subject has to carry its ancestor's id, which `@absent(<id>)` answers for payloads written before the parent was declared"
                )
            }
            Unfinished::Failed { count, first } => write!(
                f,
                "{count} subject key(s) could not be moved under their declared parent. The first said: {first}. These are not a race with another writer: they do not resolve by looking again"
            ),
            Unfinished::Contended(remaining) => write!(
                f,
                "{remaining} subject key(s) are still wrapped under the master although their subject declares a parent, after every pass this run makes. Something else is writing to this data directory and keeps winning the race; erasing a tenant would miss these until they move"
            ),
        }
    }
}
