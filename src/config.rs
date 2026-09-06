//! Project configuration (`hekla.toml`).
//!
//! A small, optional file for operational knobs that are not code: the effect worker-pool
//! size and the effect-journal retention window. Defaults are sensible, so a project runs
//! with no config. The retention window drives the sweeper; the pool size bounds how many
//! effect lanes run at once. Validating here means a malformed `hekla.toml` fails at load,
//! not at the moment the sweeper first reaches for a setting.

use std::path::Path;
use std::{fs, io};

use anyhow::Context;
use serde::Deserialize;

/// The config file name, resolved relative to the project root.
pub const FILE_NAME: &str = "hekla.toml";

/// The largest retention window we accept, in days (100 years). Bounds the
/// sweeper's date arithmetic and turns an absurd typo into a clear error.
const MAX_RETENTION_DAYS: u32 = 36_500;

/// The largest effect pool we accept. Every worker is an OS thread spawned at boot, and
/// well before this the single operational-database mutex is the bottleneck rather than
/// the thread count.
const MAX_POOL_SIZE: u32 = 1_024;

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub effects: Effects,
    pub retention: Retention,
    pub projectors: Projectors,
    pub verify: Verify,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Effects {
    /// How many effect lanes run at once, across every effect.
    ///
    /// One pool for the process rather than one per effect: the contended resource is the
    /// single operational-database mutex every journaled call goes through, so the bound
    /// that matters is a process-wide one. It also lets an effect with one hot lane and an
    /// effect with a thousand share capacity rather than being allotted it equally.
    ///
    /// **`1` is not "lanes off".** A single worker still gives every lane mutual exclusion
    /// and still lets a wedged lane step aside for a healthy one, because what fixes a
    /// stall is that a failure parks the lane and releases the worker, not the
    /// parallelism. Raising this buys throughput on slow APIs, nothing else.
    pub pool_size: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Retention {
    /// How long a completed effect invocation's journal is kept before the
    /// sweeper reclaims it. Sweeping is lazy GC, so this only bounds disk use.
    pub effect_journal_days: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Projectors {
    /// Whether a projector rebuilds automatically when what it does changes: its
    /// subscription, its entity shapes, or its handler bodies. On by default; a large
    /// deployment can turn it off to schedule rebuilds by hand.
    ///
    /// The handler bodies count because the definition is heklang's digest hash, which
    /// covers behaviour and ignores layout. A reformat therefore rebuilds nothing, and
    /// a corrected handler no longer leaves rows the old logic built in place.
    pub auto_rebuild: bool,
}

/// The continuous invariant check. Off by default, and `serve --verify` turns it on
/// without editing the file: it is not free (a second handler run per completed
/// invocation), so running it is a decision.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Verify {
    /// Replay every completed effect invocation as it happens, quarantining one that
    /// does not reproduce itself. Rebuild equivalence is not checked here: it needs
    /// the data directory to itself, so `hekla verify` is where that one lives.
    pub enabled: bool,
}
impl Default for Projectors {
    fn default() -> Projectors {
        Projectors { auto_rebuild: true }
    }
}

impl Default for Effects {
    fn default() -> Effects {
        Effects { pool_size: 16 }
    }
}

impl Default for Retention {
    fn default() -> Retention {
        Retention {
            effect_journal_days: 7,
        }
    }
}

impl Config {
    /// Load `<root>/hekla.toml`, falling back to defaults when it is absent.
    pub fn load(root: &Path) -> anyhow::Result<Config> {
        let path = root.join(FILE_NAME);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(err) => {
                return Err(err).with_context(|| format!("reading {}", path.display()));
            }
        };
        Config::parse(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// Parse config from TOML text, then validate the values.
    pub fn parse(text: &str) -> anyhow::Result<Config> {
        let config: Config = toml::from_str(text).context("invalid hekla.toml")?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.effects.pool_size == 0 {
            anyhow::bail!("effects.pool_size must be at least 1");
        }
        // The pool spawns this many OS threads at boot, so a typo is a resource
        // commitment rather than a number nobody reads. It was reserved before lanes.
        if self.effects.pool_size > MAX_POOL_SIZE {
            anyhow::bail!("effects.pool_size must be at most {MAX_POOL_SIZE}");
        }
        if self.retention.effect_journal_days > MAX_RETENTION_DAYS {
            anyhow::bail!(
                "retention.effect_journal_days must be at most {MAX_RETENTION_DAYS} (100 years)"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sensible() {
        let config = Config::default();
        assert_eq!(config.effects.pool_size, 16);
        assert_eq!(config.retention.effect_journal_days, 7);
    }

    #[test]
    fn partial_config_keeps_other_defaults() {
        let config = Config::parse("[retention]\neffect_journal_days = 3\n").unwrap();
        assert_eq!(config.retention.effect_journal_days, 3);
        assert_eq!(config.effects.pool_size, 16);
    }

    #[test]
    fn zero_pool_size_is_rejected() {
        assert!(Config::parse("[effects]\npool_size = 0\n").is_err());
    }

    #[test]
    fn absurd_retention_window_is_rejected() {
        assert!(Config::parse("[retention]\neffect_journal_days = 4000000000\n").is_err());
    }

    #[test]
    fn unknown_field_is_rejected() {
        assert!(Config::parse("[effects]\nnope = 1\n").is_err());
    }

    #[test]
    fn missing_file_is_default() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Config::load(dir.path()).unwrap(), Config::default());
    }
}
