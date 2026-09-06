//! What one effect's dispatcher knows about the work it has in flight.
//!
//! `docs/effects.md` rule 15 splits an effect's single ordered stream into **lanes**: a
//! position's `@key` values name the lane it runs in, positions in one lane run in log
//! order, and positions in different lanes need not wait for each other. Everything that
//! decides *which position runs next* lives here, with no store, no op-DB and no clock
//! beyond an `Instant` the caller passes in, so it can be tested exhaustively rather than
//! by racing a real effect against a stub.
//!
//! # The two invariants
//!
//! **Mutual exclusion.** A lane with work is in exactly one of `ready`, `running` and
//! `deferred`, and a lane with none is in no place at all. That is what makes "one lane,
//! one worker at a time" structural rather than something a caller has to remember.
//!
//! **The mark is a low-water mark.** [`LaneState::low_water`] is the highest position
//! every event at or below is terminal, which under out-of-order completion is *not* the
//! newest thing finished. It is derived on every read from the in-flight set rather than
//! accumulated, so there is no state to get wrong: one wedged lane holds the mark at its
//! own position no matter how far the others run ahead.
//!
//! # A deferral is not a skip
//!
//! A position that fails is left at the **head of its lane's queue**, never popped. What
//! the failure releases is the worker, not the work: the lane parks until its backoff
//! expires and the same position is the next thing that lane runs. Without this a wedged
//! lane would hold a pool thread through a sixty-second sleep, and `pool_size` wedged
//! lanes would starve every healthy one: the eight-hour stall this rule exists to fix,
//! reappearing one level down.
//!
//! # A queue entry is a group, not a position
//!
//! Rule 15's `on latest` runs an arm once per key per dispatch batch, at the newest
//! matching position in it, so a queue entry is a group: the position that runs, and
//! the oldest one folded into it. **The batch is what the lane has queued**, which is
//! what makes the documented live behaviour true as well as the catch-up one: six plan
//! edits a minute apart, arriving while an earlier invocation is still running, are one
//! publish.
//!
//! For an `on` or `on live` arm every group is a group of one and nothing here behaves
//! differently from the day it shipped. The group is `(arm, lane)` and never the lane
//! alone: two arms have two bodies, so folding one into the other would drop work rather
//! than repeat it.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::time::Instant;

use crate::lane::LaneId;

/// Where a lane is in the schedule. A lane with queued work always has one of these, and
/// a lane with none has no entry, which is the mutual-exclusion invariant in one type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    /// Offered to the worker pool and not yet claimed. The pool holds the queue itself,
    /// because it is shared across every effect; this only records that the lane is in it.
    Queued,
    /// A worker owns it. Nothing else may take it.
    Running,
    /// Backing off after a failure, due at this instant.
    Deferred(Instant),
}

/// One queued unit of work.
///
/// `hi` is the position that runs. `lo` is the oldest position the group is responsible
/// for, equal to `hi` until an `on latest` arm folds something into it. The in-flight set
/// is keyed on `lo`, so the mark cannot pass a folded position before the invocation that
/// covers it has finished, and it holds one entry per group rather than one per position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pending {
    lo: u64,
    hi: u64,
    /// Which arm answers it. A collapse group is `(arm, lane)`, and the lane is fixed
    /// here, so within one queue the arm is the whole of it.
    arm: usize,
}

/// What a worker takes from a lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Work {
    /// The position to run.
    pub position: u64,
    /// The oldest position folded into it, or `None` when this invocation covers only its
    /// own. Recorded by the same statement that makes the invocation terminal.
    pub collapsed_from: Option<u64>,
}

/// What admitting one position came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Admitted {
    /// The lane had no place and now has one: offer it to the pool.
    pub armed: bool,
    /// It folded into a group already queued instead of queueing an invocation of its own.
    pub folded: bool,
}

/// One effect's scheduling state. Held behind a mutex by the dispatcher; every method
/// here is short and touches no I/O, so that lock is never held across a side effect.
#[derive(Debug, Default)]
pub struct LaneState {
    /// Groups queued per lane, ascending by the position that runs. The head is what that
    /// lane is working on, and it is popped only once that position is terminal.
    queues: HashMap<LaneId, VecDeque<Pending>>,
    /// The queued `on latest` group still open to supersession, per `(lane, arm)`, by the
    /// position that would run. A group leaves this the moment a worker takes it: collapse
    /// never revokes an invocation that has begun, because that one has a journal and
    /// abandoning it would discard the record of calls that really happened.
    latest: HashMap<(LaneId, usize), u64>,
    /// The highest position ever offered to a lane, whether it queued, folded, or was
    /// already covered.
    ///
    /// **One monotone test in place of two guards.** Deliveries ascend, so anything at or
    /// below a lane's high-water is either a position a re-subscribe is replaying or one
    /// an earlier process already finished; both must be dropped, and neither can be found
    /// in the in-flight set, which now holds one entry per group rather than one per
    /// position. Seeded from `effect_lane` so a resume skips what a lane already
    /// terminalised, which is an optimisation and not the correctness boundary:
    /// `begin_invocation` reporting `AlreadyTerminal` is that, and a boot with the table
    /// empty is correct and slower.
    high: HashMap<LaneId, u64>,
    /// Where each lane with work is. A lane with none has no entry, which is the
    /// mutual-exclusion invariant in one map: a lane is offered, owned or parked, never two
    /// of those.
    place: HashMap<LaneId, Place>,
    /// Ordered by deadline. Keyed by `(instant, lane)` rather than by instant alone
    /// because two lanes can back off to the same instant and a map keyed on the
    /// instant would silently drop one of them.
    deferred: BTreeSet<(Instant, LaneId)>,
    /// Every queued group that is not yet terminal, keyed on the oldest position it is
    /// responsible for. Ordered, because its first entry is both the low-water mark and
    /// the lane an operator has to act on.
    inflight: BTreeMap<u64, LaneId>,
    /// The subscription's own cursor: everything at or below it has been admitted, or
    /// scanned and found to be nothing this effect answers.
    scanned: u64,
    /// What `effect_lane` holds for this effect, as far as this dispatcher knows: the
    /// highest position recorded per lane. Seeded from the table at boot and updated as
    /// rows are written.
    ///
    /// Two jobs, and they are the same fact. It lets a resume skip positions a lane
    /// already finished (an optimisation: dropping it costs one `begin_invocation` round
    /// trip per position to be told the same thing), and it says whether a sweep would
    /// delete anything, which keeps the sweep off the per-event path.
    ///
    /// Positions rather than a count, because `record_effect_lane` is an upsert: counting
    /// writes drifts above the number of rows, and a count that never reaches zero makes
    /// every mark advance issue a delete that matches nothing.
    rows: HashMap<LaneId, u64>,
    /// Positions above the resume point that an earlier process already has an invocation
    /// row for, in any status.
    ///
    /// **Nothing here may be folded, and one of them ends the group it lands in.** The
    /// freeze in [`LaneState::head`] keeps collapse from revoking an invocation this
    /// process began; this is the same rule across a restart, where memory cannot say. A
    /// `running` row left by a crash would otherwise be folded away and never completed,
    /// and a `terminal` one would end up inside a range that also claims to cover it, so a
    /// replay would count that position twice.
    begun: HashSet<u64>,
    /// Whether [`LaneState::begun`] is known to be *incomplete*.
    ///
    /// A dispatcher that could not read every recorded position above its mark cannot tell
    /// which are safe to fold, so it folds nothing and every position runs on its own. That
    /// is the behaviour that shipped before rule 15: redundant work, never wrong work.
    ///
    /// Stated as the negative so the derived `Default` is the right one. A fresh state has
    /// nothing recorded above its mark and so has nothing to be missing, which is what
    /// `false` says; a `folding: bool` would have defaulted to "do not fold" and turned
    /// every test built on `LaneState::default()` into one that proved nothing.
    blind: bool,
    /// `(lane, position)` pairs an operator skip has already pulled forward, so a skip
    /// whose completion keeps failing does not re-promote the lane on every reader pass.
    /// Cleared when the position retires.
    skip_promoted: HashSet<(LaneId, u64)>,
}

impl LaneState {
    /// A dispatcher resuming at `scanned`, with `rows` read back from `effect_lane` and
    /// `begun` the positions above the mark that already have an invocation row.
    pub fn resuming(
        scanned: u64,
        rows: HashMap<LaneId, u64>,
        begun: HashSet<u64>,
        blind: bool,
    ) -> LaneState {
        LaneState {
            scanned,
            // What an earlier process finished is also the furthest this one has offered
            // a lane, which is what stops those positions being dispatched again.
            high: rows.clone(),
            rows,
            begun,
            blind,
            ..LaneState::default()
        }
    }

    /// Note that `effect_lane` now records this lane at this position.
    pub fn record_row(&mut self, lane: &LaneId, position: u64) {
        let held = self.rows.entry(lane.clone()).or_default();
        *held = (*held).max(position);
    }

    /// Whether a sweep bounded at `mark` would delete anything.
    ///
    /// A healthy effect records no rows, so this keeps the sweep off the per-event path
    /// rather than issuing a delete that matches nothing on every mark advance.
    pub fn rows_to_sweep(&self, mark: u64) -> bool {
        self.rows.values().any(|position| *position <= mark)
    }

    /// Queue `position` in `lane`, as work for `arm`.
    ///
    /// `collapsible` is the caller's answer to rule 15: the arm is `on latest` *and* the
    /// lane is one whose key could actually be read. A collapsible position supersedes the
    /// group already queued for the same arm, inheriting the range it was responsible for,
    /// so a lane holds at most one queued group per `on latest` arm however long the
    /// backlog is.
    ///
    /// The superseding group goes to the **back** of the queue rather than taking the
    /// place of the one it replaced. Survivors deliver in ascending position order,
    /// interleaved with every position that did not collapse, which is what `hek test`'s
    /// dispatcher does and what an `on` arm beside an `on latest` one depends on.
    pub fn admit(&mut self, lane: LaneId, position: u64, arm: usize, declared: bool) -> Admitted {
        // The arm asked to collapse and this dispatcher is in a position to. It is not when
        // it could not read everything already recorded above its mark, and then every
        // position runs on its own, which is only ever more work.
        let collapsible = declared && !self.blind;
        // A driver that lost the log and re-subscribed resumes from the persisted mark,
        // which is at or below anything still in flight, so it re-delivers positions this
        // dispatcher is already working on. Running one of those twice in its own lane is
        // what this stops, along with re-dispatching what an earlier process finished.
        if self.high.get(&lane).is_some_and(|&seen| seen >= position) {
            return Admitted::default();
        }
        self.high.insert(lane.clone(), position);

        // An earlier process already has a record for this position, so it runs on its own
        // whatever its arm declares: folding it away would abandon a `running` row nothing
        // would ever complete, and a `terminal` one would sit inside a range that also
        // claimed to cover it.
        // **Being folded into and being folded away are not the same thing**, and only the
        // second is the hazard. A position with an earlier record still takes over the open
        // group and becomes what runs, which reproduces the partition that process built;
        // what it may not do is let a *later* position supersede it, because then nothing
        // would run at it and its row would be abandoned inside another's range. Refusing
        // the takeover as well would leave the positions it should have absorbed to be
        // dispatched on their own, re-firing an invocation older than one that has already
        // happened, which is the thing `on latest` exists to prevent.
        let supersedable = collapsible && !self.begun.contains(&position);

        // A miss here would mean the open group had left the queue without leaving
        // `latest`, which nothing does. Answering it as a fresh group rather than
        // asserting keeps a scheduling bug from becoming a panicking worker: the cost is
        // one invocation that could have folded. Matched on the arm as well as the
        // position, so the lookup and the map it came from agree on what a group is even
        // though a queue's positions are unique on their own.
        let superseded = collapsible
            .then(|| self.latest.get(&(lane.clone(), arm)).copied())
            .flatten()
            .and_then(|open| {
                let queue = self.queues.get_mut(&lane)?;
                let index = queue
                    .iter()
                    .position(|group| group.hi == open && group.arm == arm)?;
                queue.remove(index)
            });
        let group = match superseded {
            // The in-flight entry stays where it is: the group is still responsible for
            // the same oldest position, and that is the one the mark cannot pass.
            Some(open) => Pending {
                lo: open.lo,
                hi: position,
                arm,
            },
            None => {
                self.inflight.insert(position, lane.clone());
                Pending {
                    lo: position,
                    hi: position,
                    arm,
                }
            }
        };
        if supersedable {
            self.latest.insert((lane.clone(), arm), position);
        } else if collapsible {
            // It ends the chain. Leaving the open group named here would let the *next*
            // position supersede it and stretch a range across this one, which has an
            // invocation of its own and would then be claimed by two at once.
            //
            // Safe only because `crate::effect::collapsible` answers from the arm's
            // delivery and the lane, both fixed for a `(lane, arm)` pair, so every position
            // in one chain agrees about it. A collapsibility that varied per event would
            // need this branch to fire on the varying condition instead.
            self.latest.remove(&(lane.clone(), arm));
        }
        self.queues
            .entry(lane.clone())
            .or_default()
            .push_back(group);
        let armed = !self.place.contains_key(&lane);
        if armed {
            self.place.insert(lane, Place::Queued);
        }
        Admitted {
            armed,
            folded: superseded.is_some(),
        }
    }

    /// Record how far the subscription has scanned. Set in the same critical section as
    /// the admissions from that batch, so the mark can never be computed from a cursor
    /// that has run ahead of the positions it admitted.
    pub fn set_scanned(&mut self, scanned: u64) {
        self.scanned = self.scanned.max(scanned);
    }

    /// The highest position every event at or below is terminal.
    ///
    /// With nothing in flight that is the subscription's cursor, which is what lets a
    /// caught-up effect advance over a tail of events it does not answer. With something
    /// in flight it is one below the oldest of them, however much later work has
    /// finished.
    pub fn low_water(&self) -> u64 {
        match self.inflight.first_key_value() {
            Some((&oldest, _)) => oldest.saturating_sub(1).min(self.scanned),
            None => self.scanned,
        }
    }

    /// The lane and position holding the mark down, for `/status` to name.
    ///
    /// Without this an operator sees lag in the thousands and no way to find the one bad
    /// shop; the mark also bounds journal retention, so a lane wedged for a month makes a
    /// month of journal rows unsweepable for every lane.
    /// Names the lane holding the mark down, and **the position that lane will run next**,
    /// which is its queue head. Not the oldest position in flight: that one may have been
    /// folded into a later invocation, and a folded position has nothing for an operator to
    /// skip. The two are the same number until something collapses.
    pub fn pinned_by(&self) -> Option<(LaneId, u64)> {
        let (&oldest, lane) = self.inflight.first_key_value()?;
        let position = self
            .queues
            .get(lane)
            .and_then(|queue| queue.front())
            .map_or(oldest, |group| group.hi);
        Some((lane.clone(), position))
    }

    pub fn inflight(&self) -> usize {
        self.inflight.len()
    }

    /// How many lanes a worker currently owns.
    ///
    /// The drain condition on shutdown. It is deliberately not "nothing in flight": a
    /// worker stops between positions, so queued work is abandoned rather than finished,
    /// and waiting for the in-flight set to empty would wait for invocations nobody is
    /// going to run. Those stay `running` in the op-DB and replay at the next start,
    /// which is the promise the sequential driver made too.
    pub fn running(&self) -> usize {
        self.place
            .values()
            .filter(|place| **place == Place::Running)
            .count()
    }

    /// Take ownership of a lane a worker has just been handed.
    ///
    /// Answers `false` for a lane that is not waiting to be claimed, which is what keeps
    /// mutual exclusion true even if the pool ever held a stale entry: the second worker
    /// declines rather than running the same lane alongside the first.
    pub fn claim(&mut self, lane: &LaneId) -> bool {
        if self.place.get(lane) != Some(&Place::Queued) {
            return false;
        }
        self.place.insert(lane.clone(), Place::Running);
        true
    }

    /// The work a claimed lane should run next, without removing it.
    ///
    /// Taking it **freezes** the group: nothing may fold into an invocation that is about
    /// to begin, so a position admitted from here on starts a fresh group behind this one.
    /// A wedged `on latest` lane therefore costs one extra invocation rather than
    /// abandoning a half-journaled one, and the range this returns cannot widen under the
    /// completion that records it.
    pub fn head(&mut self, lane: &LaneId) -> Option<Work> {
        let group = *self.queues.get(lane)?.front()?;
        if self.latest.get(&(lane.clone(), group.arm)) == Some(&group.hi) {
            self.latest.remove(&(lane.clone(), group.arm));
        }
        Some(Work {
            position: group.hi,
            collapsed_from: (group.lo < group.hi).then_some(group.lo),
        })
    }

    /// Retire a terminal position, and say whether its lane owes a durable row.
    ///
    /// A row is owed exactly when this completion does *not* move the mark: some older
    /// position is still in flight, so the mark stays behind and a restart would
    /// otherwise re-dispatch everything this lane has finished since. When the mark does
    /// move, the mark itself already records the progress and a row would be swept on the
    /// next pass anyway.
    ///
    /// Answering `true` when it was not needed costs one redundant row. Answering `false`
    /// wrongly costs a re-dispatch that `begin_invocation` turns into a no-op. Neither
    /// can lose work, which is why this may be decided under the lock and written outside
    /// it.
    pub fn complete(&mut self, lane: &LaneId, position: u64) -> bool {
        // The whole group retires, so what leaves the in-flight set is the oldest position
        // it was responsible for. Everything between that and `position` was folded into
        // this invocation and is covered by it.
        let mut retired = position;
        if let Some(queue) = self.queues.get_mut(lane)
            && queue.front().is_some_and(|group| group.hi == position)
            && let Some(group) = queue.pop_front()
        {
            retired = group.lo;
            if queue.is_empty() {
                self.queues.remove(lane);
            }
            if self.latest.get(&(lane.clone(), group.arm)) == Some(&group.hi) {
                self.latest.remove(&(lane.clone(), group.arm));
            }
        }
        self.inflight.remove(&retired);
        self.skip_promoted.remove(&(lane.clone(), position));
        // Against `position`, never against `retired`: the row records how far this lane
        // has got, which is the position that ran, and the question is whether the mark is
        // about to reach it. Another *lane* can hold the mark at a position inside this
        // group's range, so asking about the range's oldest end answers a different
        // question and skips a row this lane needs.
        self.inflight
            .first_key_value()
            .is_some_and(|(&oldest, _)| oldest < position)
    }

    /// Give a lane back. Returns `true` when it still has work and is runnable again.
    pub fn release(&mut self, lane: &LaneId) -> bool {
        self.place.remove(lane);
        if self.queues.contains_key(lane) {
            self.place.insert(lane.clone(), Place::Queued);
            return true;
        }
        false
    }

    /// Park a lane until `deadline`, keeping its queue intact. The position it failed on
    /// is still the head, so that lane resumes on the same work.
    pub fn defer(&mut self, lane: &LaneId, deadline: Instant) {
        self.place.insert(lane.clone(), Place::Deferred(deadline));
        self.deferred.insert((deadline, lane.clone()));
    }

    /// Move every lane whose backoff has expired back to `ready`, and report them so the
    /// caller can offer them to the pool.
    pub fn promote(&mut self, now: Instant) -> Vec<LaneId> {
        let due: Vec<(Instant, LaneId)> = self
            .deferred
            .iter()
            .take_while(|(deadline, _)| *deadline <= now)
            .cloned()
            .collect();
        let mut woken = Vec::with_capacity(due.len());
        for entry in due {
            self.deferred.remove(&entry);
            let (deadline, lane) = entry;
            // A lane deferred twice leaves a stale earlier entry; only the one the lane
            // is actually parked under may wake it.
            if self.place.get(&lane) == Some(&Place::Deferred(deadline)) {
                self.place.insert(lane.clone(), Place::Queued);
                woken.push(lane);
            }
        }
        woken
    }

    /// Wake any parked lane whose next position `wanted` accepts, ahead of its deadline.
    ///
    /// An operator skip is the caller: a lane backing off for a minute should honour a
    /// skip now rather than after the minute. The sequential driver did this by cutting
    /// the sleep short, which is not available once the wedged position is waiting on a
    /// deadline instead of on a thread.
    pub fn promote_matching(&mut self, mut wanted: impl FnMut(u64) -> bool) -> Vec<LaneId> {
        let promoted = &self.skip_promoted;
        let queues = &self.queues;
        let due: Vec<(Instant, LaneId)> = self
            .deferred
            .iter()
            .filter(|(_, lane)| {
                queues
                    .get(lane)
                    .and_then(|queue| queue.front())
                    // Once per lane and position. Without this, a skip whose durable write
                    // keeps failing is re-promoted on every pass, so the lane retries at
                    // the reader's cadence instead of its own backoff and hammers the
                    // op-DB that is already the reason the write is failing.
                    .is_some_and(|group| {
                        !promoted.contains(&(lane.clone(), group.hi)) && wanted(group.hi)
                    })
            })
            .cloned()
            .collect();
        let mut woken = Vec::with_capacity(due.len());
        for entry in due {
            self.deferred.remove(&entry);
            let (deadline, lane) = entry;
            if self.place.get(&lane) == Some(&Place::Deferred(deadline)) {
                if let Some(group) = self.queues.get(&lane).and_then(|queue| queue.front()) {
                    self.skip_promoted.insert((lane.clone(), group.hi));
                }
                self.place.insert(lane.clone(), Place::Queued);
                woken.push(lane);
            }
        }
        woken
    }

    /// When the earliest parked lane is due, so an idle dispatcher knows how long it may
    /// sleep.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.deferred.first().map(|(deadline, _)| *deadline)
    }

    /// Drop what the sweep has just deleted. A row at or below the mark says nothing the
    /// mark does not, which is why the sweep removes it and why this forgets it.
    pub fn prune_rows(&mut self, mark: u64) {
        self.rows.retain(|_, done| *done > mark);
    }

    /// Forget lanes the mark has passed.
    ///
    /// Called on every advance rather than only when a sweep runs, because a healthy
    /// effect records no lane rows at all and would otherwise grow one high-water entry
    /// per key it has ever seen. Safe at the mark: a lane with anything in flight has a
    /// high-water above it, since the mark is one below the oldest group's oldest
    /// position, and the subscription resumes at the mark so nothing at or below it is
    /// ever offered again.
    pub fn forget(&mut self, mark: u64) {
        self.high.retain(|_, seen| *seen > mark);
        self.begun.retain(|position| *position > mark);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn lane(name: &str) -> LaneId {
        LaneId::from(name)
    }

    /// The admission every test that is not about collapse wants: one arm, nothing folds.
    fn queue(state: &mut LaneState, lane: LaneId, position: u64) -> bool {
        state.admit(lane, position, 0, false).armed
    }

    /// A plain `on` arm's admission beside a collapsing one, in the same lane.
    ///
    /// **A different arm, and it has to be.** `crate::effect::collapsible` answers from the
    /// arm's delivery and the lane, so collapsibility is fixed for a `(lane, arm)` pair: a
    /// non-collapsible arm-0 position in a lane whose arm 0 collapses is a state the
    /// dispatcher cannot produce, and a fixture built from one would put a position with
    /// its own group inside another group's range.
    fn queue_plain_arm(state: &mut LaneState, lane: LaneId, position: u64) -> bool {
        state.admit(lane, position, 1, false).armed
    }

    /// An `on latest` admission, which may fold into the group already queued for `arm`.
    fn collapse(state: &mut LaneState, lane: LaneId, position: u64, arm: usize) -> Admitted {
        state.admit(lane, position, arm, true)
    }

    /// What `head` answers for a group that folded nothing.
    fn runs(position: u64) -> Option<Work> {
        Some(Work {
            position,
            collapsed_from: None,
        })
    }

    /// What the pool does: it holds the queue, and the worker takes ownership on arrival.
    /// Answers `None` when the lane is not waiting to be claimed.
    fn claim(state: &mut LaneState, name: &str) -> Option<LaneId> {
        let lane = lane(name);
        state.claim(&lane).then_some(lane)
    }

    #[test]
    fn an_empty_dispatcher_is_caught_up_at_its_cursor() {
        let mut state = LaneState::resuming(42, HashMap::new(), HashSet::new(), false);
        assert_eq!(state.low_water(), 42);
        assert_eq!(state.pinned_by(), None);
        state.set_scanned(50);
        assert_eq!(state.low_water(), 50);
    }

    /// The cursor only moves forward. A late batch reporting an older position must not
    /// drag the mark back over work already reported done.
    #[test]
    fn the_cursor_never_moves_backwards() {
        let mut state = LaneState::resuming(42, HashMap::new(), HashSet::new(), false);
        state.set_scanned(10);
        assert_eq!(state.low_water(), 42);
    }

    #[test]
    fn one_lane_at_a_time_reaches_a_worker() {
        let mut state = LaneState::default();
        assert!(queue(&mut state, lane("a"), 1));
        assert!(
            !queue(&mut state, lane("a"), 2),
            "a second position re-arms nothing"
        );
        assert_eq!(claim(&mut state, "a"), Some(lane("a")));
        assert_eq!(
            claim(&mut state, "a"),
            None,
            "a second worker declines a lane the first owns"
        );
    }

    /// The graceful-drain condition. It was silently always zero once, because the
    /// dispatcher never took ownership of a lane, so shutdown broke on its first pass and
    /// published its final mark before the invocations in flight had finished.
    #[test]
    fn a_claimed_lane_counts_as_running_until_it_is_given_back() {
        let mut state = LaneState::default();
        queue(&mut state, lane("a"), 1);
        assert_eq!(state.running(), 0, "queued is not owned");

        claim(&mut state, "a");
        assert_eq!(state.running(), 1);

        // Parking is not finishing, but it does hand the worker back.
        state.defer(&lane("a"), Instant::now() + Duration::from_millis(10));
        assert_eq!(state.running(), 0);

        state.promote(Instant::now() + Duration::from_millis(20));
        claim(&mut state, "a");
        assert_eq!(state.running(), 1);
        state.complete(&lane("a"), 1);
        state.release(&lane("a"));
        assert_eq!(state.running(), 0);
    }

    #[test]
    fn different_lanes_run_concurrently() {
        let mut state = LaneState::default();
        queue(&mut state, lane("a"), 1);
        queue(&mut state, lane("b"), 2);
        assert_eq!(claim(&mut state, "a"), Some(lane("a")));
        assert_eq!(
            claim(&mut state, "b"),
            Some(lane("b")),
            "b need not wait for a"
        );
    }

    /// The headline property: lane B racing ahead does not move the mark past lane A's
    /// stuck position, however much B finishes.
    #[test]
    fn a_wedged_lane_holds_the_mark_and_nothing_else() {
        let mut state = LaneState::default();
        queue(&mut state, lane("stuck"), 5);
        queue(&mut state, lane("fast"), 6);
        queue(&mut state, lane("fast"), 7);
        state.set_scanned(7);

        assert!(state.complete(&lane("fast"), 6));
        assert!(state.complete(&lane("fast"), 7));
        assert_eq!(state.low_water(), 4, "the mark stays one below the wedge");
        assert_eq!(state.pinned_by(), Some((lane("stuck"), 5)));

        state.complete(&lane("stuck"), 5);
        assert_eq!(state.low_water(), 7, "and jumps once the wedge clears");
        assert_eq!(state.pinned_by(), None);
    }

    /// A row is owed only when the completion leaves the mark behind. In the caught-up
    /// case (one position at a time, nothing older in flight) nothing is owed, which is
    /// what keeps a healthy effect at zero extra writes per event.
    #[test]
    fn a_row_is_owed_only_when_the_mark_cannot_follow() {
        let mut state = LaneState::default();
        queue(&mut state, lane("a"), 1);
        state.set_scanned(1);
        assert!(!state.complete(&lane("a"), 1), "nothing older is in flight");

        queue(&mut state, lane("old"), 2);
        queue(&mut state, lane("new"), 3);
        state.set_scanned(3);
        assert!(
            state.complete(&lane("new"), 3),
            "position 2 still pins the mark"
        );
    }

    #[test]
    fn a_deferred_lane_keeps_its_position_at_the_head() {
        let mut state = LaneState::default();
        queue(&mut state, lane("a"), 1);
        queue(&mut state, lane("a"), 2);
        claim(&mut state, "a");
        assert_eq!(state.head(&lane("a")), runs(1));

        let due = Instant::now() + Duration::from_millis(50);
        state.defer(&lane("a"), due);
        assert_eq!(
            claim(&mut state, "a"),
            None,
            "a parked lane is not claimable"
        );
        assert_eq!(
            state.head(&lane("a")),
            runs(1),
            "the failed position is still the next thing this lane runs"
        );

        assert!(state.promote(due - Duration::from_millis(1)).is_empty());
        assert_eq!(state.promote(due), vec![lane("a")]);
        assert_eq!(claim(&mut state, "a"), Some(lane("a")));
    }

    /// A lane deferred twice leaves an earlier entry behind in the ordered set. Waking on
    /// it would hand the lane to a second worker while the first still owns it.
    #[test]
    fn a_stale_deadline_does_not_wake_a_lane_twice() {
        let mut state = LaneState::default();
        queue(&mut state, lane("a"), 1);
        claim(&mut state, "a");
        let first = Instant::now();
        let second = first + Duration::from_millis(100);
        state.defer(&lane("a"), first);
        state.defer(&lane("a"), second);

        assert!(
            state.promote(first).is_empty(),
            "the superseded deadline wakes nothing"
        );
        assert_eq!(state.promote(second), vec![lane("a")]);
    }

    /// The sequential driver honoured a skip by cutting the wedged position's sleep
    /// short. Once a wedged lane waits on a deadline instead of on a thread, that is not
    /// available, so the skip has to reach the lane through the schedule instead.
    #[test]
    fn a_skip_wakes_a_parked_lane_ahead_of_its_deadline() {
        let mut state = LaneState::default();
        queue(&mut state, lane("wedged"), 7);
        queue(&mut state, lane("waiting"), 9);
        claim(&mut state, "wedged");
        claim(&mut state, "waiting");
        let far = Instant::now() + Duration::from_secs(60);
        state.defer(&lane("wedged"), far);
        state.defer(&lane("waiting"), far);

        assert_eq!(
            state.promote_matching(|position| position == 7),
            vec![lane("wedged")],
            "only the lane whose next position was skipped"
        );
        assert_eq!(claim(&mut state, "wedged"), Some(lane("wedged")));
        assert_eq!(
            claim(&mut state, "waiting"),
            None,
            "the other lane keeps waiting out its backoff"
        );

        // The lane parks again because its completion failed. A second pass must not pull
        // it forward again, or the skip would retry at the reader's cadence rather than
        // the lane's backoff.
        state.defer(&lane("wedged"), far);
        assert!(
            state.promote_matching(|position| position == 7).is_empty(),
            "one promotion per skipped position, not one per pass"
        );
    }

    #[test]
    fn releasing_re_arms_a_lane_that_still_has_work() {
        let mut state = LaneState::default();
        queue(&mut state, lane("a"), 1);
        queue(&mut state, lane("a"), 2);
        claim(&mut state, "a");
        state.complete(&lane("a"), 1);
        assert!(state.release(&lane("a")), "position 2 is still queued");
        assert_eq!(claim(&mut state, "a"), Some(lane("a")));

        state.complete(&lane("a"), 2);
        assert!(!state.release(&lane("a")), "an empty lane is not runnable");
        assert_eq!(claim(&mut state, "a"), None);
    }

    #[test]
    fn a_resumed_lane_skips_what_it_already_finished() {
        let mut state =
            LaneState::resuming(10, HashMap::from([(lane("a"), 20)]), HashSet::new(), false);
        queue(&mut state, lane("a"), 20);
        assert_eq!(
            state.inflight(),
            0,
            "already terminal in an earlier process"
        );
        queue(&mut state, lane("a"), 21);
        assert_eq!(state.head(&lane("a")), runs(21));
        queue(&mut state, lane("b"), 1);
        assert_eq!(
            state.head(&lane("b")),
            runs(1),
            "another lane is not covered by it"
        );
    }

    #[test]
    fn resume_hints_are_dropped_once_the_mark_passes_them() {
        let mut state = LaneState::resuming(
            0,
            HashMap::from([(lane("a"), 5), (lane("b"), 50)]),
            HashSet::new(),
            false,
        );
        state.prune_rows(10);
        state.forget(10);
        assert!(!state.rows_to_sweep(10), "the row itself is gone");
        // Below the mark the hint is redundant, because the subscription resumes there and
        // never offers those positions again; above it, it is still the only thing keeping
        // a lane that raced ahead from re-running its own work.
        queue(&mut state, lane("b"), 50);
        assert_eq!(
            state.inflight(),
            0,
            "a lane above the mark is still covered"
        );
    }

    // --- rule 15: batch collapse -------------------------------------------

    /// The headline: three positions for one key in one batch are one invocation, at the
    /// newest of them, and the range says what it stands for.
    #[test]
    fn a_latest_arm_folds_its_backlog_into_the_newest_position() {
        let mut state = LaneState::default();
        assert!(collapse(&mut state, lane("shop"), 1, 0).armed);
        assert!(collapse(&mut state, lane("shop"), 3, 0).folded);
        assert!(collapse(&mut state, lane("shop"), 5, 0).folded);

        assert_eq!(
            state.inflight(),
            1,
            "one group, not one entry per position: the cap counts work that will run"
        );
        state.claim(&lane("shop"));
        assert_eq!(
            state.head(&lane("shop")),
            Some(Work {
                position: 5,
                collapsed_from: Some(1)
            })
        );
        assert_eq!(
            state.low_water(),
            0,
            "the mark is held at the oldest position the group answers for, not the newest"
        );
    }

    /// The whole group retires together, so the mark clears every position it covered
    /// rather than only the one that ran.
    #[test]
    fn completing_a_group_clears_the_positions_it_folded() {
        let mut state = LaneState::default();
        collapse(&mut state, lane("shop"), 1, 0);
        collapse(&mut state, lane("shop"), 5, 0);
        state.set_scanned(5);
        state.claim(&lane("shop"));

        state.complete(&lane("shop"), 5);
        assert_eq!(state.inflight(), 0);
        assert_eq!(state.low_water(), 5);
    }

    /// Two arms have two bodies, so folding one into the other would drop work rather than
    /// repeat it. They share a lane (the key is the same) and are still two groups.
    #[test]
    fn two_arms_of_one_lane_are_two_groups() {
        let mut state = LaneState::default();
        collapse(&mut state, lane("shop"), 1, 0);
        assert!(
            !collapse(&mut state, lane("shop"), 2, 1).folded,
            "a different arm never folds into this one"
        );
        collapse(&mut state, lane("shop"), 3, 0);

        state.claim(&lane("shop"));
        assert_eq!(
            state.head(&lane("shop")),
            runs(2),
            "the arm that folded nothing keeps its place in log order"
        );
        state.complete(&lane("shop"), 2);
        assert_eq!(
            state.head(&lane("shop")),
            Some(Work {
                position: 3,
                collapsed_from: Some(1)
            })
        );
    }

    /// A superseding group goes to the back, so survivors still deliver in ascending
    /// position order alongside everything that did not collapse. `hek test`'s dispatcher
    /// does the same, and an `on` arm beside an `on latest` one depends on it.
    #[test]
    fn a_plain_arm_beside_a_folding_one_keeps_its_turn() {
        let mut state = LaneState::default();
        collapse(&mut state, lane("shop"), 1, 0);
        queue_plain_arm(&mut state, lane("shop"), 2);
        collapse(&mut state, lane("shop"), 3, 0);

        state.claim(&lane("shop"));
        assert_eq!(state.head(&lane("shop")), runs(2));
        state.complete(&lane("shop"), 2);
        assert_eq!(
            state.head(&lane("shop")),
            Some(Work {
                position: 3,
                collapsed_from: Some(1)
            })
        );
    }

    /// Nothing folds into an invocation that has begun. That one has a journal, and
    /// abandoning it would discard the record of calls that really happened, so a wedged
    /// `on latest` lane costs one extra invocation rather than a lost one.
    #[test]
    fn a_group_a_worker_has_taken_is_not_superseded() {
        let mut state = LaneState::default();
        collapse(&mut state, lane("shop"), 1, 0);
        state.claim(&lane("shop"));
        let taken = state.head(&lane("shop"));
        assert_eq!(taken, runs(1));

        assert!(
            !collapse(&mut state, lane("shop"), 4, 0).folded,
            "it starts a fresh group behind the one that is running"
        );
        assert_eq!(
            state.head(&lane("shop")),
            taken,
            "and the range the completion is about to record cannot widen under it"
        );

        state.complete(&lane("shop"), 1);
        assert_eq!(state.head(&lane("shop")), runs(4));
    }

    /// A re-subscribe replays from the persisted mark, so it re-delivers positions this
    /// dispatcher already folded. Those are not in the in-flight set to be found, which is
    /// what the lane high-water is for.
    #[test]
    fn a_replayed_batch_does_not_fold_twice() {
        let mut state = LaneState::default();
        collapse(&mut state, lane("shop"), 1, 0);
        collapse(&mut state, lane("shop"), 2, 0);
        collapse(&mut state, lane("shop"), 3, 0);

        for position in 1..=3 {
            assert_eq!(
                collapse(&mut state, lane("shop"), position, 0),
                Admitted::default(),
                "position {position} is already answered for"
            );
        }
        state.claim(&lane("shop"));
        assert_eq!(
            state.head(&lane("shop")),
            Some(Work {
                position: 3,
                collapsed_from: Some(1)
            })
        );
    }

    /// A dispatcher that could not read everything already recorded above its mark folds
    /// nothing, because a partial answer is worse than none: it would report a position as
    /// safe to fold when it is exactly the one that must not be. What that costs is the
    /// redundant invocations `on latest` exists to avoid, which is the behaviour that
    /// shipped before it.
    #[test]
    fn a_dispatcher_that_cannot_see_what_ran_folds_nothing() {
        let mut state = LaneState::resuming(0, HashMap::new(), HashSet::new(), true);
        collapse(&mut state, lane("shop"), 1, 0);
        assert!(!collapse(&mut state, lane("shop"), 3, 0).folded);
        assert!(!collapse(&mut state, lane("shop"), 5, 0).folded);

        state.claim(&lane("shop"));
        assert_eq!(state.head(&lane("shop")), runs(1));
        state.complete(&lane("shop"), 1);
        assert_eq!(state.head(&lane("shop")), runs(3));
        state.complete(&lane("shop"), 3);
        assert_eq!(state.head(&lane("shop")), runs(5));
    }

    /// The mark is held by the oldest position the group answers for; the operator is told
    /// the newest, because that is the one that will run and the only one a skip can name.
    #[test]
    fn the_pinning_report_names_the_position_that_runs() {
        let mut state = LaneState::default();
        collapse(&mut state, lane("shop"), 4, 0);
        collapse(&mut state, lane("shop"), 9, 0);
        state.set_scanned(9);

        assert_eq!(state.low_water(), 3);
        assert_eq!(state.pinned_by(), Some((lane("shop"), 9)));
    }

    /// The lane runs its queue in order, so what an operator can act on is the head, not
    /// whichever group happens to hold the oldest position. Reporting the second reads as a
    /// `pinning_position` beside a `last_error` about a different one, and a skip aimed at
    /// it never fires: `attempt` only honours a skip for the position it is running.
    #[test]
    fn the_pinning_report_names_the_head_and_not_the_oldest_group() {
        let mut state = LaneState::default();
        collapse(&mut state, lane("shop"), 1, 0);
        queue_plain_arm(&mut state, lane("shop"), 2);
        collapse(&mut state, lane("shop"), 3, 0);
        state.set_scanned(3);

        assert_eq!(state.low_water(), 0, "position 1 is still answered for");
        assert_eq!(
            state.pinned_by(),
            Some((lane("shop"), 2)),
            "position 2 is what the lane is working on"
        );
    }

    /// The row records how far the lane got, so what decides whether one is owed is whether
    /// the mark will reach *that*. Another lane can pin the mark at a position inside this
    /// group's range, which asking about the range's oldest end cannot see.
    #[test]
    fn a_collapsed_group_owes_a_row_when_another_lane_pins_the_mark_inside_it() {
        let mut state = LaneState::default();
        collapse(&mut state, lane("a"), 10, 0);
        queue(&mut state, lane("b"), 50);
        collapse(&mut state, lane("a"), 100, 0);
        state.set_scanned(100);

        assert!(
            state.complete(&lane("a"), 100),
            "lane b holds the mark at 49, so lane a's progress to 100 needs recording"
        );
    }

    /// A crash leaves `running` rows for invocations that began. Folding one of those away
    /// would abandon it: nothing would ever complete it, so its row and its journal would
    /// never be swept, and the range would claim to cover a position an invocation is still
    /// open on. The freeze in `head` says the same thing within one process; this is the
    /// half memory cannot answer.
    /// A position an earlier process already has a record for **runs**, and it is what the
    /// group it lands in runs *as*.
    ///
    /// Two things have to hold at once, and they pull in opposite directions. It must not
    /// be folded away, or a `running` row would be abandoned with nothing to complete it
    /// and a `terminal` one would sit inside a range that also claimed to cover it. But it
    /// must still absorb the open group, or the positions that group holds get dispatched
    /// on their own and fire an invocation older than one that has already happened.
    #[test]
    fn a_position_an_earlier_process_began_runs_and_ends_its_chain() {
        let mut state = LaneState::resuming(0, HashMap::new(), HashSet::from([5]), false);
        collapse(&mut state, lane("shop"), 1, 0);
        assert!(collapse(&mut state, lane("shop"), 3, 0).folded);
        assert!(
            collapse(&mut state, lane("shop"), 5, 0).folded,
            "position 5 takes over the group rather than leaving 1 and 3 to run alone"
        );
        assert!(
            !collapse(&mut state, lane("shop"), 7, 0).folded,
            "and it ends the chain: 7 may not supersede a position that has to run"
        );

        state.claim(&lane("shop"));
        assert_eq!(
            state.head(&lane("shop")),
            Some(Work {
                position: 5,
                collapsed_from: Some(1)
            }),
            "one invocation, at the position the earlier process was already running"
        );
        state.complete(&lane("shop"), 5);
        assert_eq!(
            state.head(&lane("shop")),
            runs(7),
            "7 covers only itself: a range across 5 would be claimed by two invocations"
        );
    }

    /// `record_effect_lane` is an upsert, so a lane written twice is still one row.
    /// Counting writes instead of tracking positions drifted above the number of rows, and
    /// a count that never reached zero made every mark advance issue a delete that matched
    /// nothing: the opposite of what the bookkeeping is for.
    #[test]
    fn a_lane_written_twice_is_still_one_row_to_sweep() {
        let mut state = LaneState::default();
        state.record_row(&lane("a"), 10);
        state.record_row(&lane("a"), 11);

        assert!(
            !state.rows_to_sweep(9),
            "nothing is at or below the mark yet"
        );
        assert!(state.rows_to_sweep(11));

        state.prune_rows(11);
        assert!(
            !state.rows_to_sweep(u64::MAX),
            "the row is gone, so no later advance sweeps for it again"
        );
    }

    /// A row above the mark has to keep the effect on the sweep path. Clearing on a sweep
    /// that deleted nothing stranded it the moment its lane went quiet, which is the
    /// permanent per-key row the design exists to avoid.
    #[test]
    fn a_row_above_the_mark_survives_a_sweep_that_did_not_reach_it() {
        let mut state = LaneState::default();
        state.record_row(&lane("ahead"), 100);
        state.record_row(&lane("behind"), 5);

        assert!(state.rows_to_sweep(5));
        state.prune_rows(5);
        assert!(
            state.rows_to_sweep(100),
            "the row at 100 is still owed a sweep once the mark reaches it"
        );
    }

    /// The invariant that makes "one lane, one worker" structural, over a scripted mix of
    /// every operation. A lane in two places at once would let two workers run the same
    /// aggregate out of order, which is the one thing lanes exist to prevent.
    #[test]
    fn a_lane_is_never_runnable_and_running_at_once() {
        // Two positions arrive already carrying an invocation row, as a crash leaves them,
        // so the run covers a chain being ended as well as chains that only grow.
        let mut state = LaneState::resuming(0, HashMap::new(), HashSet::from([4, 9]), false);
        let now = Instant::now();
        let lanes = [lane("a"), lane("b"), lane("c")];
        let mut position = 0u64;
        let mut scanned = 0u64;

        // Lane `a` folds and the others do not, so the run mixes groups of one with groups
        // that span a range. Its positions alternate between two arms, because a collapse
        // group is `(arm, lane)` and a single-armed lane would never ask whether the arm is
        // part of the key.
        //
        // `outstanding` is tracked here rather than asked of `LaneState`, because the
        // invariant is about *positions* and the dispatcher now holds groups: a folded
        // position is in no queue and in no in-flight entry of its own, so anything derived
        // from those could not catch the mark passing one.
        let mut outstanding: BTreeMap<u64, LaneId> = BTreeMap::new();
        for round in 0..60u64 {
            // Two quiet rounds in six, so the lanes drain and the mark is asked what it
            // reads with nothing left outstanding. Fed continuously it would never be, and
            // a group left in the in-flight set after retiring would pin the mark for ever
            // in silence.
            let feeding = round % 6 < 4;
            for (index, id) in lanes.iter().enumerate() {
                // Lane `a` takes work every feeding round, the others every other one. That
                // is what leaves a folded group outstanding across the checks below: a lane
                // fed only on the rounds it also completes on would fold and retire in the
                // same pass, and the mark would never be asked about a position in between.
                if feeding && (index == 0 || (round + index as u64).is_multiple_of(2)) {
                    position += 1;
                    if index == 0 {
                        // Two arms, alternating in runs, so consecutive admissions share one
                        // and a chain has something to fold. Alternating every round would
                        // give two chains that each grow by one and never collapse.
                        collapse(&mut state, id.clone(), position, (round / 2 % 2) as usize);
                    } else {
                        queue(&mut state, id.clone(), position);
                    }
                    outstanding.insert(position, id.clone());
                    scanned = position;
                    state.set_scanned(scanned);
                }
            }
            // What the pool does: offer every queued lane, and let each worker claim.
            let mut claimed = Vec::new();
            for id in &lanes {
                if state.claim(id) {
                    claimed.push(id.clone());
                }
            }
            // The mutual-exclusion property itself. Asking a second time is the only way to
            // observe it: a loop that visits each lane once cannot hand one out twice
            // however broken `claim` is, so a `claimed.contains` check would pass on a
            // `claim` that always answered `true`.
            for id in &claimed {
                assert!(
                    !state.claim(id),
                    "`{id}` was handed out while a worker held it"
                );
            }
            for id in claimed {
                // Lane `a` is the collapsing one: it takes work for two rounds and is
                // drained on the third, so its second admission folds into the first and a
                // worker takes a group that really spans a range.
                //
                // Left in the varied mix below it never once reached that state: parked and
                // released on the rounds it was fed, drained on the rounds it was not, so
                // the `outstanding` bookkeeping was decorative and every collapse-shaped
                // mutation passed. Measured rather than assumed: the run now completes nine
                // folded groups where it completed none. The mix still covers `b` and `c`.
                let branch = if id == lanes[0] {
                    if round % 3 == 2 { 0 } else { 3 }
                } else {
                    round % 4
                };
                match branch {
                    // Drained in a loop, as `run_lane` drains it: a worker that took one
                    // position and gave the lane back would never reach a group queued
                    // behind another, which is exactly where a fold ends up.
                    0 => {
                        while let Some(head) = state.head(&id) {
                            let owed = state.complete(&id, head.position);
                            // A row is owed exactly when the mark will not reach the
                            // position that ran, which is what the row records.
                            assert_eq!(
                                owed,
                                state.low_water() < head.position,
                                "`{id}` disagreed about owing a row at {}",
                                head.position
                            );
                            // The whole group retires, which for a collapsing lane is every
                            // position it still owed at or below the one that ran.
                            outstanding
                                .retain(|position, lane| *lane != id || *position > head.position);
                        }
                        state.release(&id);
                    }
                    // Parked past this round, so a lane is genuinely unavailable across an
                    // assertion rather than deferred and promoted in the same pass.
                    1 => state.defer(&id, now + Duration::from_millis(round + 3)),
                    // Taken and given back without finishing: the group freezes, so the
                    // next position for that arm has to start a new one.
                    2 => {
                        state.head(&id);
                        state.release(&id);
                    }
                    _ => {
                        state.release(&id);
                    }
                }
            }
            state.promote(now + Duration::from_millis(round));
            let oldest = outstanding
                .first_key_value()
                .map_or(u64::MAX, |(&position, _)| position);
            assert!(
                state.low_water() < oldest,
                "the mark reached {} with position {oldest} still outstanding",
                state.low_water()
            );
        }

        // Now stop feeding and drain everything. This is the other direction, and it needs
        // its own phase because the loop above never empties: the assertion in it catches a
        // mark that ran *ahead* of the work, while a group left in the in-flight set after
        // retiring would pin the mark for ever behind it. That costs journal retention
        // rather than correctness, so it would otherwise pass in silence.
        for _ in 0..lanes.len() * 4 {
            state.promote(now + Duration::from_secs(3600));
            for id in &lanes {
                if state.claim(id) {
                    while let Some(head) = state.head(id) {
                        state.complete(id, head.position);
                        outstanding
                            .retain(|position, lane| lane != id || *position > head.position);
                    }
                    state.release(id);
                }
            }
        }
        assert!(
            outstanding.is_empty(),
            "work survived the drain: {outstanding:?}"
        );
        assert_eq!(
            state.low_water(),
            scanned,
            "the mark stalled with nothing left outstanding"
        );
    }
}
