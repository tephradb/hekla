//! The progress line `hekla project` draws while it scans.
//!
//! A projection over a small log finishes before a human could read anything, and one
//! over a large log can run for minutes with nothing to show for it. Both are the same
//! command, so this draws nothing at all until `FIRST_PAINT` has passed and then
//! redraws one line in place, which costs a fast run nothing and gives a slow one the
//! only two facts an operator wants: how far along it is, and whether it is finding
//! anything.
//!
//! **Only a terminal ever sees it.** The line is drawn with a carriage return and erased
//! before the result prints, which is meaningless in a pipe, a file or a CI log, so a
//! stderr that is not a terminal disables it outright.
//!
//! It is deliberately not gated on `--no-color` or `NO_COLOR`, which [`crate::cli`]
//! answers for the log: the line carries no ANSI colour at all, only `\r`, so a colour
//! preference has nothing to say about it. `--no-progress` is the opt-out. Note also that
//! the colour decision asks about *stdout* and this asks about stderr, because that is
//! where it draws: `hekla project ... --json > out.json` still gets a progress line, and
//! `out.json` is still only the document.
//!
//! **The percentage is position over the log's tip, not events over events.** How many
//! events a projector will match is not knowable before the scan, so a bar scaled to
//! matches would rescale itself as it learned. Positions are dense and the tip is fixed
//! the moment the follower opens, so this one only ever moves forward.

use std::cell::Cell;
use std::fmt::Write as _;
use std::io::{self, IsTerminal, Write as _};
use std::time::{Duration, Instant};

/// Nothing is drawn before this much has elapsed, so the common case, a projection that
/// finishes in a few hundred milliseconds, prints nothing at all.
const FIRST_PAINT: Duration = Duration::from_millis(250);

/// The shortest gap between two paints. Ten a second reads as smooth and costs nothing
/// beside the scan it is reporting on.
const REDRAW: Duration = Duration::from_millis(100);

/// How long a scan must have run before an estimate is offered. One extrapolated from
/// the first fraction of a second is noise, and a confidently wrong number is worse than
/// no number.
const ETA_AFTER: Duration = Duration::from_secs(2);

/// A single line redrawn on stderr while a scan runs.
///
/// Every method takes `&self` so the scan can hold it behind a shared reference and
/// still report; the interior state is two `Cell`s and nothing crosses a thread.
pub struct Progress {
    enabled: bool,
    started: Instant,
    /// When the line was last painted, or `None` if it never has been. Doubles as what
    /// says whether [`Progress::clear`] has anything to erase.
    painted: Cell<Option<Instant>>,
    /// The width of the last line drawn, so `clear` erases exactly it and not a fixed
    /// guess that would leave a tail behind on a narrow line.
    width: Cell<usize>,
}

impl Progress {
    /// A progress line on stderr, if stderr is a terminal and the operator wants one.
    pub fn stderr(wanted: bool) -> Progress {
        Progress::new(wanted && io::stderr().is_terminal())
    }

    /// The same with the terminal decision already made, for a test or a caller that
    /// draws somewhere else.
    pub fn new(enabled: bool) -> Progress {
        Progress {
            enabled,
            started: Instant::now(),
            painted: Cell::new(None),
            width: Cell::new(0),
        }
    }

    /// Report where the scan has reached.
    ///
    /// Cheap to call often: it returns before formatting anything until a paint is due,
    /// so a caller does not have to rate-limit on its own.
    pub fn tick(&self, position: u64, head: u64, matched: usize) {
        if !self.enabled {
            return;
        }
        let elapsed = self.started.elapsed();
        if elapsed < FIRST_PAINT {
            return;
        }
        if self.painted.get().is_some_and(|at| at.elapsed() < REDRAW) {
            return;
        }
        let text = line(position, head, matched, elapsed);
        // Padded to the widest line drawn so far, because a carriage return moves the
        // cursor and erases nothing. The line does shrink: the estimate loses digits as
        // it counts down and disappears entirely at the tip, so an unpadded redraw would
        // leave the tail of the longer line on screen for `clear` to miss.
        let width = text.chars().count().max(self.width.get());
        self.width.set(width);
        self.painted.set(Some(Instant::now()));
        let mut err = io::stderr().lock();
        let _ = write!(err, "\r{text:<width$}");
        let _ = err.flush();
    }

    /// Erase the line, if one was drawn.
    ///
    /// Every path out of a scan calls this before writing anything of its own, the error
    /// paths included: half a progress line left under a diagnostic is how a diagnostic
    /// gets misread.
    pub fn clear(&self) {
        if self.painted.get().is_none() {
            return;
        }
        let width = self.width.get();
        let mut err = io::stderr().lock();
        let _ = write!(err, "\r{:width$}\r", "");
        let _ = err.flush();
        self.painted.set(None);
    }
}

/// The line itself, as a pure function of what the scan knows.
///
/// Separate from [`Progress`] so it can be asserted on without a terminal, which is the
/// same split `cli::use_ansi` makes for the colour decision.
pub fn line(position: u64, head: u64, matched: usize, elapsed: Duration) -> String {
    // An empty log is complete rather than a division by zero. A scan over one visits
    // nothing and this is drawn only if it somehow took long enough to paint.
    let percent = match head {
        0 => 100,
        head => position.min(head) * 100 / head,
    };
    let mut out = format!(
        "  scanned {} of {} ({percent}%)  {} matched",
        thousands(position),
        thousands(head),
        thousands(matched as u64)
    );
    if let Some(left) = remaining(position, head, elapsed) {
        let _ = write!(out, "  {left} left");
    }
    out
}

/// How much longer the scan has, extrapolated from the rate it has managed so far.
///
/// `None` until there is enough of a run to extrapolate from, and `None` at the end,
/// where "0s left" beside a scan that has stopped moving reads as a stall.
fn remaining(position: u64, head: u64, elapsed: Duration) -> Option<String> {
    if elapsed < ETA_AFTER || position == 0 || position >= head {
        return None;
    }
    let elapsed = elapsed.as_secs_f64();
    let total = elapsed * head as f64 / position as f64;
    Some(short_duration(total - elapsed))
}

/// A duration for a line that is glanced at: whole seconds under a minute, minutes and
/// seconds under an hour, hours and minutes beyond it.
fn short_duration(seconds: f64) -> String {
    let seconds = seconds.max(0.0) as u64;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h{:02}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

/// A count with thousands separators.
///
/// The one place hekla groups digits, and the counts here earn it: a log position runs
/// to seven figures and this line is glanced at rather than read. Every other number the
/// CLI prints is a declaration or an invocation count, small enough to read bare.
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.char_indices() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_groups_from_the_right() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(7), "7");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(1_203_556), "1,203,556");
        assert_eq!(thousands(u64::MAX), "18,446,744,073,709,551,615");
    }

    #[test]
    fn a_line_reports_position_over_the_tip() {
        let text = line(412_880, 1_203_556, 12_418, Duration::from_millis(500));
        assert_eq!(text, "  scanned 412,880 of 1,203,556 (34%)  12,418 matched");
    }

    /// The estimate needs a run to extrapolate from, and one taken from the first
    /// moments would be a confident guess at a number nobody knows yet.
    #[test]
    fn a_line_offers_no_estimate_before_it_can_make_one() {
        let text = line(10, 100, 1, Duration::from_millis(500));
        assert!(!text.contains("left"), "{text}");
    }

    /// "0s left" beside a scan that has reached the tip reads as a stall rather than as
    /// a finish, and the summary is about to say it finished anyway.
    #[test]
    fn a_line_offers_no_estimate_at_the_end() {
        let text = line(100, 100, 50, Duration::from_secs(30));
        assert!(!text.contains("left"), "{text}");
    }

    #[test]
    fn a_line_estimates_from_the_rate_so_far() {
        // A quarter of the log in ten seconds is thirty seconds left.
        let text = line(25, 100, 5, Duration::from_secs(10));
        assert!(text.ends_with("  30s left"), "{text}");
    }

    #[test]
    fn an_empty_log_is_complete_rather_than_a_division_by_zero() {
        let text = line(0, 0, 0, Duration::from_secs(10));
        assert!(text.contains("(100%)"), "{text}");
    }

    /// A position past the tip cannot happen through a follower, whose reads stop at the
    /// prefix it pinned, but a percentage over one hundred would be a worse way to find
    /// that out than a clamped bar.
    #[test]
    fn a_position_past_the_tip_clamps() {
        let text = line(200, 100, 1, Duration::from_millis(500));
        assert!(text.contains("(100%)"), "{text}");
    }

    #[test]
    fn short_durations_read_at_a_glance() {
        assert_eq!(short_duration(0.4), "0s");
        assert_eq!(short_duration(47.0), "47s");
        assert_eq!(short_duration(192.0), "3m12s");
        assert_eq!(short_duration(7_320.0), "2h02m");
    }

    #[test]
    fn a_disabled_progress_draws_and_erases_nothing() {
        let progress = Progress::new(false);
        progress.tick(1, 2, 1);
        progress.clear();
        assert!(progress.painted.get().is_none());
    }
}
