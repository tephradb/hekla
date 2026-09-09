//! The Prometheus surface: `GET /metrics`.
//!
//! One file knows every metric name and every label, and the rest of the tree reaches it
//! through the plain functions below. That is deliberate: a call site that imported
//! `metrics::counter!` could spell a label, and the whole safety argument here rests on
//! which strings may become one.
//!
//! # Every label is a declaration
//!
//! A command, projector, entity, effect or event name; a refusal code, which is derived
//! from the name of a `refusal` the command declared; a fixed outcome or state word; or a
//! digest hash, which moves only when the code does. Nothing here is computed from a
//! request or read out of an event payload, so the series count is bounded by the project
//! rather than by traffic.
//!
//! **A lane key is never a label, and that rule is about erasure rather than cardinality.**
//! A `LaneId` is a partition key, so it is routinely a customer or a shop id.
//! `/status` and `/admin/effects` report the pinning one, and they may: that JSON is a
//! live view, so an erased subject stops appearing in it. A scrape is a *copy*, taken into
//! a time-series database that replicates and retains it, and `hekla erase` cannot reach
//! there. So `hekla_effect_wedged_lanes` reports a count, the operator follows the alert to
//! `/admin/effects/{name}` to learn which lane, and the split is the point rather than an
//! omission. The same goes for a subject value and for anything else off an event.
//!
//! # There is no collector task
//!
//! umari runs a ticker to snapshot its gauges because its state lives behind actors and
//! reaching it is an async, fallible ask. Every gauge here is an atomic load on a handle
//! or `Store::head`, which `/status` already does synchronously in an async handler, so
//! [`refresh`] runs at scrape time instead: nothing to configure, no missed tick, and no
//! staleness between a number and the scrape carrying it.
//!
//! Two things follow. There is no `idle_timeout` and so no `run_upkeep`, because hekla's
//! module set is fixed at load and no series ever needs expiring. And [`refresh`] also
//! zeroes every counter whose labels it can enumerate, so a command that has never run
//! reads `0` rather than being absent and `rate()` works from the first scrape.

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use metrics::{counter, describe_counter, describe_gauge, gauge};
use metrics_exporter_prometheus::PrometheusBuilder;
pub use metrics_exporter_prometheus::PrometheusHandle;

use crate::effect::EFFECT_STATES;
use crate::projector::Readiness;
use crate::runtime::Runtime;
use crate::schema::EmittedEvent;

/// What `/metrics` answers with. The version is the Prometheus text exposition format,
/// not hekla's.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4";

const BUILD_INFO: &str = "hekla_build_info";
const UPTIME: &str = "hekla_uptime_seconds";
const LOG_HEAD: &str = "hekla_log_head_position";
const EVENTS_APPENDED: &str = "hekla_events_appended_total";
const MODULE_INFO: &str = "hekla_module_info";

const COMMANDS: &str = "hekla_commands_total";
const COMMAND_REFUSALS: &str = "hekla_command_refusals_total";
const COMMAND_CONFLICT_RETRIES: &str = "hekla_command_conflict_retries_total";

const PROJECTOR_UP: &str = "hekla_projector_up";
const PROJECTOR_POSITION: &str = "hekla_projector_position";
const PROJECTOR_LAG: &str = "hekla_projector_lag";
const PROJECTOR_READINESS: &str = "hekla_projector_readiness";
const PROJECTOR_EVENTS: &str = "hekla_projector_events_total";
const PROJECTOR_REBUILDS: &str = "hekla_projector_rebuilds_total";
const PROJECTOR_PROGRESS: &str = "hekla_projector_last_progress_timestamp_seconds";

const EFFECT_UP: &str = "hekla_effect_up";
const EFFECT_POSITION: &str = "hekla_effect_position";
const EFFECT_LAG: &str = "hekla_effect_lag";
const EFFECT_STATE: &str = "hekla_effect_state";
const WEDGED_LANES: &str = "hekla_effect_wedged_lanes";
const EFFECT_FAILURES: &str = "hekla_effect_consecutive_failures";
const EFFECT_BACKOFF: &str = "hekla_effect_retry_backoff_seconds";
const EFFECT_INVOCATIONS: &str = "hekla_effect_invocations_total";
const EFFECT_RESTARTS: &str = "hekla_effect_restarts_total";
const EFFECT_TERMINAL_SKIPS: &str = "hekla_effect_terminal_skips_total";
const EFFECT_LIVE_SUPPRESSED: &str = "hekla_effect_live_suppressed_total";
const EFFECT_COLLAPSED: &str = "hekla_effect_collapsed_total";
const EFFECT_PROGRESS: &str = "hekla_effect_last_progress_timestamp_seconds";

const EFFECT_HTTP: &str = "hekla_effect_http_requests_total";
const READS: &str = "hekla_reads_total";
const READ_WAITS: &str = "hekla_read_waits_total";

/// Every `CommandOutcome` variant, plus the `error` the 500 paths report. Enumerated so
/// [`refresh`] can zero them; the recorder takes whichever one happened.
const COMMAND_OUTCOMES: [&str; 7] = [
    "committed",
    "already_committed",
    "rejected",
    "invalid_input",
    "conflict",
    "unavailable",
    "error",
];

// The two state sets are `EFFECT_STATES` and `Readiness::ALL`, and both live beside the
// function that produces the word rather than here. A state set is the one shape that
// fails quietly when its list falls behind: every series reads 0, nothing errors, and an
// `== 1` alert just stops firing. Keeping the list next to the `match` that has to grow
// with it is what turns that into a compile error instead.

/// The outcomes `attempt` can reach. `ignored` is deliberately absent: heklang's `Done`
/// and `Ignored` both settle an invocation and `try_invocation` folds them into one
/// `Ok(())`, so telling them apart here would mean widening that return for a label.
const EFFECT_OUTCOMES: [&str; 4] = ["completed", "failed", "terminal", "skipped"];

const REBUILD_OUTCOMES: [&str; 2] = ["completed", "failed"];

const HTTP_OUTCOMES: [&str; 5] = ["2xx", "3xx", "4xx", "5xx", "error"];

/// What a read can end as. `invalid_input` covers every 400 the read surface answers (an
/// unparseable limit or cursor, an unindexed filter, a malformed wait), which are the
/// caller's mistake rather than hekla's and would otherwise be the one part of the read
/// path with no series at all.
const READ_OUTCOMES: [&str; 5] = ["ok", "not_found", "not_ready", "invalid_input", "error"];

const WAIT_OUTCOMES: [&str; 2] = ["served", "timeout"];

static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// Install the recorder, once per process, and return the handle `/metrics` renders.
///
/// Idempotent, so calling it from both `cli::serve` and a test harness is safe. **Order
/// matters at the one call site that has a choice**: a `counter!` recorded before a
/// recorder exists is dropped on the floor, so `serve` installs before anything is
/// loaded or spawned.
pub fn install() -> PrometheusHandle {
    HANDLE
        .get_or_init(|| {
            // Stock apart from having no exporter of its own: `default-features = false`
            // left the http listener out, so this builds a recorder and nothing else.
            // No `idle_timeout`, and therefore no `run_upkeep` to forget: see the module
            // docs for why nothing here ever needs expiring.
            let recorder = PrometheusBuilder::new().build_recorder();
            let handle = recorder.handle();
            // Built and then installed, rather than `install_recorder`, so that losing
            // the global slot is survivable. An embedder using hekla as a library may
            // have installed its own recorder first, and `/metrics` calls this: panicking
            // there would take the scrape down permanently, since `get_or_init` leaves
            // the cell empty when its closure unwinds and the next scrape would panic
            // again. This way hekla's own numbers go to the other recorder and this
            // handle renders an honest empty document.
            if let Err(err) = metrics::set_global_recorder(recorder) {
                tracing::warn!(
                    "a metrics recorder was already installed, so /metrics will be empty: {err}"
                );
            }
            describe();
            handle
        })
        .clone()
}

/// One `# HELP` line per metric. Units are carried by the name suffix (`_seconds`,
/// `_total`, `_position`), which is the Prometheus convention and the one umari uses.
fn describe() {
    describe_gauge!(BUILD_INFO, "Always 1, carrying this build's hekla version.");
    describe_gauge!(UPTIME, "Seconds since this runtime opened.");
    describe_gauge!(LOG_HEAD, "The event log's head position.");
    describe_counter!(EVENTS_APPENDED, "Events appended, by declared event type.");
    describe_gauge!(
        MODULE_INFO,
        "Always 1, carrying each loaded module's kind, name and digest hash. The hash \
         moves when what the module does moves, so two replicas disagreeing here are \
         running different code."
    );

    describe_counter!(COMMANDS, "Command attempts, by command and outcome.");
    describe_counter!(
        COMMAND_REFUSALS,
        "Refusals, by command and declared refusal code. A code appears on its first \
         refusal rather than at boot, so alert on it with `or vector(0)`."
    );
    describe_counter!(
        COMMAND_CONFLICT_RETRIES,
        "DCB boundary conflicts retried inside a command's retry budget. Distinct from \
         the `conflict` outcome, which is the budget having run out: this is contention, \
         that is contention the caller sees."
    );

    describe_gauge!(
        PROJECTOR_UP,
        "1 while a projector's thread is running and unfailed."
    );
    describe_gauge!(
        PROJECTOR_POSITION,
        "The log position a projector has committed."
    );
    describe_gauge!(
        PROJECTOR_LAG,
        "Events between a projector and the log head."
    );
    describe_gauge!(
        PROJECTOR_READINESS,
        "A state set over a projector's readiness: 1 on the current state, 0 on the rest."
    );
    describe_counter!(
        PROJECTOR_EVENTS,
        "Events applied into a projector's read model."
    );
    describe_counter!(PROJECTOR_REBUILDS, "Projector rebuilds, by outcome.");
    describe_gauge!(
        PROJECTOR_PROGRESS,
        "Unix seconds at which a projector last committed a batch. Subtract from `time()` \
         for staleness."
    );

    describe_gauge!(EFFECT_UP, "1 while an effect's reader thread is running.");
    describe_gauge!(
        EFFECT_POSITION,
        "An effect's mark: every position at or below is terminal."
    );
    describe_gauge!(
        EFFECT_LAG,
        "Events between an effect's mark and the log head."
    );
    describe_gauge!(
        EFFECT_STATE,
        "A state set over an effect's health: 1 on the current state, 0 on the rest."
    );
    describe_gauge!(
        WEDGED_LANES,
        "How many of an effect's lanes are failing. A count and never a key: a lane key \
         is a partition key, and a scrape outlives an erasure."
    );
    describe_gauge!(
        EFFECT_FAILURES,
        "Consecutive failures on the lane holding an effect's mark down."
    );
    describe_gauge!(
        EFFECT_BACKOFF,
        "Seconds until the pinning lane's next retry, or 0 when nothing is waiting."
    );
    describe_counter!(
        EFFECT_INVOCATIONS,
        "Effect invocations, by effect and outcome."
    );
    describe_counter!(
        EFFECT_RESTARTS,
        "Times an effect's reader was restarted by its supervisor."
    );
    describe_counter!(
        EFFECT_TERMINAL_SKIPS,
        "Positions abandoned to an unrecoverable failure. These advance rather than wedge."
    );
    describe_counter!(
        EFFECT_LIVE_SUPPRESSED,
        "Positions an `on live` arm declined for being below its boundary."
    );
    describe_counter!(
        EFFECT_COLLAPSED,
        "Positions an `on latest` arm folded into a later invocation."
    );
    describe_gauge!(
        EFFECT_PROGRESS,
        "Unix seconds at which an effect last advanced its mark."
    );

    describe_counter!(
        EFFECT_HTTP,
        "Outbound HTTP requests made by effects, by response status class. Carries no \
         effect name and no url: the transport does not know the one and the other is \
         unbounded."
    );
    describe_counter!(
        READS,
        "Read API requests, by projector, entity and outcome."
    );
    describe_counter!(
        READ_WAITS,
        "Read-your-writes waits, by projector and whether the projector arrived in time."
    );
}

/// Snapshot every gauge from the runtime's handles, and zero every counter whose labels
/// are enumerable. Called by the `/metrics` handler, immediately before rendering.
pub fn refresh(runtime: &Runtime) {
    let head = runtime.log_head();
    gauge!(BUILD_INFO, "version" => env!("CARGO_PKG_VERSION")).set(1.0);
    gauge!(UPTIME).set(runtime.uptime_seconds() as f64);
    gauge!(LOG_HEAD).set(head as f64);

    for event in runtime.events_map().keys() {
        counter!(EVENTS_APPENDED, "event" => event.clone()).increment(0);
    }

    for unit in runtime.command_units() {
        let name = unit.def.name();
        gauge!(
            MODULE_INFO,
            "kind" => "command",
            "name" => name.to_owned(),
            "hash" => unit.digest_hash.clone(),
        )
        .set(1.0);
        for outcome in COMMAND_OUTCOMES {
            counter!(COMMANDS, "command" => name.to_owned(), "outcome" => outcome).increment(0);
        }
        counter!(COMMAND_CONFLICT_RETRIES, "command" => name.to_owned()).increment(0);
    }

    for handle in runtime.projector_handles() {
        let name = &handle.name;
        let position = handle.position();
        gauge!(
            MODULE_INFO,
            "kind" => "projector",
            "name" => name.clone(),
            "hash" => handle.digest_hash.clone(),
        )
        .set(1.0);
        gauge!(PROJECTOR_UP, "name" => name.clone()).set(bit(handle.running() && !handle.failed()));
        gauge!(PROJECTOR_POSITION, "name" => name.clone()).set(position as f64);
        gauge!(PROJECTOR_LAG, "name" => name.clone()).set(head.saturating_sub(position) as f64);
        let readiness = handle.readiness();
        for state in Readiness::ALL {
            gauge!(PROJECTOR_READINESS, "name" => name.clone(), "state" => state.label())
                .set(bit(state == readiness));
        }
        counter!(PROJECTOR_EVENTS, "name" => name.clone()).increment(0);
        for outcome in REBUILD_OUTCOMES {
            counter!(PROJECTOR_REBUILDS, "name" => name.clone(), "outcome" => outcome).increment(0);
        }
        for outcome in WAIT_OUTCOMES {
            counter!(READ_WAITS, "projector" => name.clone(), "outcome" => outcome).increment(0);
        }
        // The entities are on the handle, so the read labels are as enumerable as every
        // other one here and there is no reason for these two to be the exception that
        // only appears once someone has read from them.
        for entity in handle.entities.iter() {
            for outcome in READ_OUTCOMES {
                counter!(
                    READS,
                    "projector" => name.clone(),
                    "entity" => entity.name.clone(),
                    "outcome" => outcome,
                )
                .increment(0);
            }
        }
    }

    for handle in runtime.effect_handles() {
        let name = &handle.name;
        let position = handle.position();
        gauge!(
            MODULE_INFO,
            "kind" => "effect",
            "name" => name.clone(),
            "hash" => handle.digest_hash.clone(),
        )
        .set(1.0);
        gauge!(EFFECT_UP, "name" => name.clone()).set(bit(handle.running()));
        gauge!(EFFECT_POSITION, "name" => name.clone()).set(position as f64);
        gauge!(EFFECT_LAG, "name" => name.clone()).set(head.saturating_sub(position) as f64);
        let current = handle.state(head);
        for state in EFFECT_STATES {
            gauge!(EFFECT_STATE, "name" => name.clone(), "state" => state)
                .set(bit(state == current));
        }
        gauge!(WEDGED_LANES, "name" => name.clone()).set(handle.wedged_lanes() as f64);
        gauge!(EFFECT_FAILURES, "name" => name.clone()).set(handle.consecutive_failures() as f64);
        // Published as a remaining duration rather than a deadline for the same reason
        // `/admin/effects` reports one: the reader's clock is a different machine's.
        gauge!(EFFECT_BACKOFF, "name" => name.clone())
            .set(handle.retry_in_ms().unwrap_or(0) as f64 / 1000.0);
        // Cumulative atomics that already exist on the handle. Absolute rather than
        // incremented at the source, so a counter cannot drift from what `/status` says.
        counter!(EFFECT_TERMINAL_SKIPS, "name" => name.clone()).absolute(handle.terminal_skips());
        counter!(EFFECT_LIVE_SUPPRESSED, "name" => name.clone()).absolute(handle.live_suppressed());
        counter!(EFFECT_COLLAPSED, "name" => name.clone()).absolute(handle.latest_collapsed());
        counter!(EFFECT_RESTARTS, "name" => name.clone()).increment(0);
        for outcome in EFFECT_OUTCOMES {
            counter!(EFFECT_INVOCATIONS, "name" => name.clone(), "outcome" => outcome).increment(0);
        }
    }

    for outcome in HTTP_OUTCOMES {
        counter!(EFFECT_HTTP, "outcome" => outcome).increment(0);
    }
}

/// `1.0` or `0.0`, for a state set and for the `_up` gauges.
fn bit(set: bool) -> f64 {
    if set { 1.0 } else { 0.0 }
}

/// Unix seconds, for the two progress gauges. Before the epoch reads as 0 rather than
/// panicking: a clock that far wrong is not worth a crash in an observability path.
fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs_f64())
        .unwrap_or(0.0)
}

// --- recorders: the only way the rest of the tree touches a metric --------------------

pub fn command_outcome(command: &str, outcome: &'static str) {
    counter!(COMMANDS, "command" => command.to_owned(), "outcome" => outcome).increment(1);
}

/// The code is derived from the name of a `refusal` the command declared, so the label
/// set is fixed by the source. heklang inlines a refusal before a program exists, though,
/// so hekla cannot enumerate the codes and [`refresh`] cannot zero them: a series here
/// appears on its first refusal.
pub fn command_refusal(command: &str, code: &str) {
    counter!(COMMAND_REFUSALS, "command" => command.to_owned(), "code" => code.to_owned())
        .increment(1);
}

pub fn command_conflict_retry(command: &str) {
    counter!(COMMAND_CONFLICT_RETRIES, "command" => command.to_owned()).increment(1);
}

pub fn events_appended(events: &[EmittedEvent]) {
    for event in events {
        counter!(EVENTS_APPENDED, "event" => event.event_type.clone()).increment(1);
    }
}

pub fn projector_batch(name: &str, events: usize) {
    counter!(PROJECTOR_EVENTS, "name" => name.to_owned()).increment(events as u64);
    gauge!(PROJECTOR_PROGRESS, "name" => name.to_owned()).set(unix_now());
}

pub fn projector_rebuild(name: &str, ok: bool) {
    let outcome = if ok { "completed" } else { "failed" };
    counter!(PROJECTOR_REBUILDS, "name" => name.to_owned(), "outcome" => outcome).increment(1);
}

pub fn effect_invocation(name: &str, outcome: &'static str) {
    counter!(EFFECT_INVOCATIONS, "name" => name.to_owned(), "outcome" => outcome).increment(1);
}

pub fn effect_restart(name: &str) {
    counter!(EFFECT_RESTARTS, "name" => name.to_owned()).increment(1);
}

pub fn effect_progress(name: &str) {
    gauge!(EFFECT_PROGRESS, "name" => name.to_owned()).set(unix_now());
}

/// `None` is a transport failure: no response arrived, so there is no status to class.
pub fn effect_http(status: Option<u16>) {
    counter!(EFFECT_HTTP, "outcome" => status_class(status)).increment(1);
}

fn status_class(status: Option<u16>) -> &'static str {
    match status {
        Some(200..=299) => "2xx",
        Some(300..=399) => "3xx",
        Some(400..=499) => "4xx",
        Some(500..=599) => "5xx",
        // A status outside those ranges is not a class anyone alerts on, and inventing a
        // sixth label for it would be a series nobody reads.
        _ => "error",
    }
}

pub fn read(projector: &str, entity: &str, outcome: &'static str) {
    counter!(
        READS,
        "projector" => projector.to_owned(),
        "entity" => entity.to_owned(),
        "outcome" => outcome,
    )
    .increment(1);
}

pub fn read_wait(projector: &str, outcome: &'static str) {
    counter!(READ_WAITS, "projector" => projector.to_owned(), "outcome" => outcome).increment(1);
}

#[cfg(test)]
mod tests {
    use metrics::with_local_recorder;
    use metrics_exporter_prometheus::PrometheusRecorder;

    use super::*;

    /// A recorder of this test's own, rather than the process-wide one [`install`]
    /// holds. Two tests running in parallel against one registry could not assert an
    /// exact count, and an exact count is the whole point of testing a counter.
    fn rendered(record: impl FnOnce()) -> String {
        let recorder: PrometheusRecorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        with_local_recorder(&recorder, || {
            describe();
            record();
        });
        handle.render()
    }

    /// Every line of the render for one metric family, so an assertion cannot pass by
    /// matching a longer metric name that happens to share the prefix.
    fn series<'a>(render: &'a str, metric: &str) -> Vec<&'a str> {
        render
            .lines()
            .filter(|line| line.split(['{', ' ']).next() == Some(metric) && !line.starts_with('#'))
            .collect()
    }

    #[test]
    fn an_outcome_carries_its_command_and_its_outcome() {
        let render = rendered(|| {
            command_outcome("PlaceOrder", "committed");
            command_outcome("PlaceOrder", "committed");
            command_outcome("PlaceOrder", "conflict");
        });
        assert!(
            render.contains(r#"hekla_commands_total{command="PlaceOrder",outcome="committed"} 2"#),
            "{render}"
        );
        assert!(
            render.contains(r#"hekla_commands_total{command="PlaceOrder",outcome="conflict"} 1"#),
            "{render}"
        );
    }

    #[test]
    fn a_described_metric_carries_its_help_text() {
        let render = rendered(|| command_outcome("PlaceOrder", "committed"));
        assert!(render.contains("# HELP hekla_commands_total"), "{render}");
        assert!(
            render.contains("# TYPE hekla_commands_total counter"),
            "{render}"
        );
    }

    /// The rule the module doc states, as a check on the render rather than on the
    /// intent: a refusal code reaches a label and nothing else about the refusal does.
    #[test]
    fn a_refusal_carries_its_declared_code_only() {
        let render = rendered(|| command_refusal("PlaceOrder", "SoldOut"));
        assert!(
            render
                .contains(r#"hekla_command_refusals_total{command="PlaceOrder",code="SoldOut"} 1"#),
            "{render}"
        );
    }

    #[test]
    fn a_status_class_is_the_only_thing_an_outbound_call_reports() {
        let render = rendered(|| {
            effect_http(Some(204));
            effect_http(Some(503));
            effect_http(None);
        });
        assert!(render.contains(r#"hekla_effect_http_requests_total{outcome="2xx"} 1"#));
        assert!(render.contains(r#"hekla_effect_http_requests_total{outcome="5xx"} 1"#));
        assert!(render.contains(r#"hekla_effect_http_requests_total{outcome="error"} 1"#));
    }

    #[test]
    fn a_status_outside_the_ranges_is_an_error_rather_than_a_sixth_class() {
        assert_eq!(status_class(Some(200)), "2xx");
        assert_eq!(status_class(Some(599)), "5xx");
        assert_eq!(status_class(Some(600)), "error");
        assert_eq!(status_class(Some(99)), "error");
        assert_eq!(status_class(None), "error");
    }

    #[test]
    fn a_batch_counts_its_events_and_stamps_its_progress() {
        let render = rendered(|| {
            projector_batch("orders", 3);
            projector_batch("orders", 4);
        });
        assert!(
            render.contains(r#"hekla_projector_events_total{name="orders"} 7"#),
            "{render}"
        );
        // The value is a wall clock, so assert the series exists rather than its
        // reading: what a staleness alert needs is that it is present and moving.
        assert_eq!(
            series(&render, "hekla_projector_last_progress_timestamp_seconds").len(),
            1,
            "{render}"
        );
    }

    #[test]
    fn a_rebuild_outcome_is_a_label_rather_than_two_metrics() {
        let render = rendered(|| {
            projector_rebuild("orders", true);
            projector_rebuild("orders", false);
        });
        assert!(
            render
                .contains(r#"hekla_projector_rebuilds_total{name="orders",outcome="completed"} 1"#),
            "{render}"
        );
        assert!(
            render.contains(r#"hekla_projector_rebuilds_total{name="orders",outcome="failed"} 1"#),
            "{render}"
        );
    }

    #[test]
    fn an_append_counts_once_per_event() {
        let events = vec![
            EmittedEvent {
                event_type: "order.placed".to_owned(),
                data: serde_json::Value::Null,
                tags: Vec::new(),
            },
            EmittedEvent {
                event_type: "order.placed".to_owned(),
                data: serde_json::Value::Null,
                tags: Vec::new(),
            },
        ];
        let render = rendered(|| events_appended(&events));
        assert!(
            render.contains(r#"hekla_events_appended_total{event="order.placed"} 2"#),
            "{render}"
        );
    }

    #[test]
    fn a_bit_is_one_or_zero() {
        assert_eq!(bit(true), 1.0);
        assert_eq!(bit(false), 0.0);
    }
}
