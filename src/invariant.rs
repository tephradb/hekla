//! The invariants the design rests on, and what a breach of one looks like.
//!
//! Separate from the sweep that checks them because a component reports its own
//! breaches as it runs: a projector quarantines itself on a checkpoint regression
//! without waiting for `hek verify` to come round. The types are the shared vocabulary;
//! `verify.rs` is the offline sweep.

use std::fmt;

/// A broken invariant, with enough detail to act on without re-running the check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    /// A rebuilt projector disagrees with the live read model.
    RebuildMismatch {
        projector: String,
        entity: String,
        key: String,
        detail: Mismatch,
    },
    /// A sealed replay of a recorded invocation did not reproduce it.
    ReplayDivergence {
        effect: String,
        position: u64,
        detail: String,
    },
    /// A checkpoint moved backwards.
    CheckpointRegression {
        component: String,
        from: u64,
        to: u64,
    },
}

/// How a rebuilt row differs from the live one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mismatch {
    /// The live model has a row the rebuild does not produce.
    OnlyLive(String),
    /// The rebuild produces a row the live model does not have.
    OnlyRebuilt(String),
    /// Both have the row, with different contents.
    Differs { live: String, rebuilt: String },
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Violation::RebuildMismatch {
                projector,
                entity,
                key,
                detail,
            } => {
                write!(f, "projector `{projector}` entity `{entity}` key `{key}`: ")?;
                match detail {
                    Mismatch::OnlyLive(row) => {
                        write!(f, "live has a row a rebuild does not produce: {row}")
                    }
                    Mismatch::OnlyRebuilt(row) => {
                        write!(f, "a rebuild produces a row live does not have: {row}")
                    }
                    Mismatch::Differs { live, rebuilt } => {
                        write!(f, "live {live} but a rebuild gives {rebuilt}")
                    }
                }
            }
            Violation::ReplayDivergence {
                effect,
                position,
                detail,
            } => write!(
                f,
                "effect `{effect}` at position {position} does not replay faithfully: {detail}"
            ),
            Violation::CheckpointRegression {
                component,
                from,
                to,
            } => write!(
                f,
                "{component} moved its checkpoint backwards, from {from} to {to}"
            ),
        }
    }
}
