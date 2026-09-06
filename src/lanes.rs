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

/// One effect's scheduling state. Held behind a mutex by the dispatcher; every method
/// here is short and touches no I/O, so that lock is never held across a side effect.
#[derive(Debug, Default)]
pub struct LaneState {
    /// Positions queued per lane, ascending. The head is the position that lane is
    /// working on, and it is popped only once that position is terminal.
    queues: HashMap<LaneId, VecDeque<u64>>,
    /// Where each lane with work is. A lane with none has no entry, which is the
    /// mutual-exclusion invariant in one map: a lane is offered, owned or parked, never two
    /// of those.
    place: HashMap<LaneId, Place>,
    /// Ordered by deadline. Keyed by `(instant, lane)` rather than by instant alone
    /// because two lanes can back off to the same instant and a map keyed on the
    /// instant would silently drop one of them.
    deferred: BTreeSet<(Instant, LaneId)>,
    /// Every admitted position that is not yet terminal, and the lane it belongs to.
    /// Ordered, because its first entry is both the low-water mark and the lane an
    /// operator has to act on.
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
    /// `(lane, position)` pairs an operator skip has already pulled forward, so a skip
    /// whose completion keeps failing does not re-promote the lane on every reader pass.
    /// Cleared when the position retires.
    skip_promoted: HashSet<(LaneId, u64)>,
}

impl LaneState {
    /// A dispatcher resuming at `scanned`, with `rows` read back from `effect_lane`.
    pub fn resuming(scanned: u64, rows: HashMap<LaneId, u64>) -> LaneState {
        LaneState {
            scanned,
            rows,
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

    /// Whether an earlier process already finished this position in this lane.
    ///
    /// Safe to answer `false` at any time (the position is then dispatched and
    /// `begin_invocation` reports it already terminal), but never safe to answer `true`
    /// wrongly, which is why `effect_lane` rows are written only after a completion is
    /// durable.
    pub fn already_done(&self, lane: &LaneId, position: u64) -> bool {
        self.rows.get(lane).is_some_and(|&done| done >= position)
    }

    /// Queue `position` in `lane`. Returns `true` when the lane became runnable, which
    /// is the caller's signal to offer it to the pool.
    pub fn admit(&mut self, lane: LaneId, position: u64) -> bool {
        // A driver that lost the log and re-subscribed resumes from the persisted mark,
        // which is at or below anything still in flight, so it re-delivers positions this
        // dispatcher is already working on. Queueing them twice would run one position
        // twice in its own lane.
        if self.inflight.contains_key(&position) {
            return false;
        }
        self.queues
            .entry(lane.clone())
            .or_default()
            .push_back(position);
        self.inflight.insert(position, lane.clone());
        if self.place.contains_key(&lane) {
            return false;
        }
        self.place.insert(lane, Place::Queued);
        true
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
    pub fn pinned_by(&self) -> Option<(LaneId, u64)> {
        self.inflight
            .first_key_value()
            .map(|(&position, lane)| (lane.clone(), position))
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

    /// The position a claimed lane should run next, without removing it.
    pub fn head(&self, lane: &LaneId) -> Option<u64> {
        self.queues
            .get(lane)
            .and_then(|queue| queue.front())
            .copied()
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
        if let Some(queue) = self.queues.get_mut(lane)
            && queue.front() == Some(&position)
        {
            queue.pop_front();
            if queue.is_empty() {
                self.queues.remove(lane);
            }
        }
        self.inflight.remove(&position);
        self.skip_promoted.remove(&(lane.clone(), position));
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
                    .is_some_and(|position| {
                        !promoted.contains(&(lane.clone(), *position)) && wanted(*position)
                    })
            })
            .cloned()
            .collect();
        let mut woken = Vec::with_capacity(due.len());
        for entry in due {
            self.deferred.remove(&entry);
            let (deadline, lane) = entry;
            if self.place.get(&lane) == Some(&Place::Deferred(deadline)) {
                if let Some(position) = self.queues.get(&lane).and_then(|queue| queue.front()) {
                    self.skip_promoted.insert((lane.clone(), *position));
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn lane(name: &str) -> LaneId {
        LaneId::from(name)
    }

    /// What the pool does: it holds the queue, and the worker takes ownership on arrival.
    /// Answers `None` when the lane is not waiting to be claimed.
    fn claim(state: &mut LaneState, name: &str) -> Option<LaneId> {
        let lane = lane(name);
        state.claim(&lane).then_some(lane)
    }

    #[test]
    fn an_empty_dispatcher_is_caught_up_at_its_cursor() {
        let mut state = LaneState::resuming(42, HashMap::new());
        assert_eq!(state.low_water(), 42);
        assert_eq!(state.pinned_by(), None);
        state.set_scanned(50);
        assert_eq!(state.low_water(), 50);
    }

    /// The cursor only moves forward. A late batch reporting an older position must not
    /// drag the mark back over work already reported done.
    #[test]
    fn the_cursor_never_moves_backwards() {
        let mut state = LaneState::resuming(42, HashMap::new());
        state.set_scanned(10);
        assert_eq!(state.low_water(), 42);
    }

    #[test]
    fn one_lane_at_a_time_reaches_a_worker() {
        let mut state = LaneState::default();
        assert!(state.admit(lane("a"), 1));
        assert!(
            !state.admit(lane("a"), 2),
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
        state.admit(lane("a"), 1);
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
        state.admit(lane("a"), 1);
        state.admit(lane("b"), 2);
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
        state.admit(lane("stuck"), 5);
        state.admit(lane("fast"), 6);
        state.admit(lane("fast"), 7);
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
        state.admit(lane("a"), 1);
        state.set_scanned(1);
        assert!(!state.complete(&lane("a"), 1), "nothing older is in flight");

        state.admit(lane("old"), 2);
        state.admit(lane("new"), 3);
        state.set_scanned(3);
        assert!(
            state.complete(&lane("new"), 3),
            "position 2 still pins the mark"
        );
    }

    #[test]
    fn a_deferred_lane_keeps_its_position_at_the_head() {
        let mut state = LaneState::default();
        state.admit(lane("a"), 1);
        state.admit(lane("a"), 2);
        claim(&mut state, "a");
        assert_eq!(state.head(&lane("a")), Some(1));

        let due = Instant::now() + Duration::from_millis(50);
        state.defer(&lane("a"), due);
        assert_eq!(
            claim(&mut state, "a"),
            None,
            "a parked lane is not claimable"
        );
        assert_eq!(
            state.head(&lane("a")),
            Some(1),
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
        state.admit(lane("a"), 1);
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
        state.admit(lane("wedged"), 7);
        state.admit(lane("waiting"), 9);
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
        state.admit(lane("a"), 1);
        state.admit(lane("a"), 2);
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
        let state = LaneState::resuming(10, HashMap::from([(lane("a"), 20)]));
        assert!(state.already_done(&lane("a"), 20));
        assert!(!state.already_done(&lane("a"), 21));
        assert!(
            !state.already_done(&lane("b"), 1),
            "another lane is not covered"
        );
    }

    #[test]
    fn resume_hints_are_dropped_once_the_mark_passes_them() {
        let mut state = LaneState::resuming(0, HashMap::from([(lane("a"), 5), (lane("b"), 50)]));
        state.prune_rows(10);
        assert!(!state.already_done(&lane("a"), 5));
        assert!(state.already_done(&lane("b"), 50));
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
        let mut state = LaneState::default();
        let now = Instant::now();
        let lanes = [lane("a"), lane("b"), lane("c")];
        let mut position = 0u64;

        for round in 0..40u64 {
            for (index, id) in lanes.iter().enumerate() {
                if (round + index as u64).is_multiple_of(2) {
                    position += 1;
                    state.admit(id.clone(), position);
                    state.set_scanned(position);
                }
            }
            // What the pool does: offer every queued lane, and let each worker claim.
            let mut claimed = Vec::new();
            for id in &lanes {
                if state.claim(id) {
                    assert!(
                        !claimed.contains(id),
                        "`{id}` was handed out twice in one pass"
                    );
                    claimed.push(id.clone());
                }
            }
            for id in claimed {
                match round % 3 {
                    0 => {
                        if let Some(head) = state.head(&id) {
                            state.complete(&id, head);
                        }
                        state.release(&id);
                    }
                    1 => state.defer(&id, now + Duration::from_millis(round)),
                    _ => {
                        state.release(&id);
                    }
                }
            }
            state.promote(now + Duration::from_millis(round));
            assert!(
                state.low_water() <= state.pinned_by().map_or(u64::MAX, |(_, at)| at - 1),
                "the mark passed a position still in flight"
            );
        }
    }
}
