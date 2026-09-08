//! Deployment credentials: what a `secret NAME` declaration resolves to.
//!
//! heklang decides *where* a credential may be read (an effect arm, an effect-local
//! `fn`, and nowhere else) and guarantees it reaches a url, a header value or a request
//! body and nothing observable. It decides nothing about where the value comes from,
//! which is this module: a `[secrets]` table in `hekla.toml` naming an environment
//! variable or a file, and `HEKLA_SECRET_<NAME>` for a project that configured nothing.
//!
//! Resolution happens once, at startup, and never fails on absence: an unset credential
//! is a fact for the runtime to refuse on (`Runtime::open`) or for `hekla plan` to
//! report, in the same shape `crypto::master_keys_from_env` already uses for the master
//! key. Only a source that is present and unreadable is an error here.
//!
//! **A `secret NAME?` that is unset is never an error.** That is the whole reason the
//! language has the form: a webhook this deployment does not configure is a branch the
//! program takes, not a boot failure.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::{env, fs, io};

use anyhow::Context;
use heklang::{Program, SecretDef};
use zeroize::Zeroizing;

use crate::config::{Config, SecretSource};
use crate::hash::sha256_hex;

/// The convention a project that configures nothing gets: `secret STRIPE_KEY` reads
/// `HEKLA_SECRET_STRIPE_KEY`. It joins `HEKLA_MASTER_KEY` and `HEKLA_UI_DIR` in the
/// runtime's namespace rather than inventing a second one.
pub const ENV_PREFIX: &str = "HEKLA_SECRET_";

/// How much of a fingerprint is shown. Enough to tell staging from production at a
/// glance, which is the whole job: an operator comparing two deployments, never a
/// reader recovering anything.
const FINGERPRINT_LEN: usize = 8;

/// The shortest credential [`SecretStore::redact`] will scan for. Below this a value
/// occurs inside ordinary words, and a scrubber that redacted half of every message
/// would cost an operator more than the leak it prevents.
const MIN_SCRUB_LEN: usize = 8;

/// What a deployment supplies, resolved once and shared for the life of the process.
///
/// This is the only thing that holds a plaintext credential on hekla's side. It is
/// deliberately not `Debug`: a struct that prints its own contents is one
/// `tracing::debug!` away from publishing every credential the process holds.
pub struct SecretStore {
    /// The value is itself an `Option` so that "this deployment set it" and "this
    /// deployment declared it unset" are one entry rather than two collections with an
    /// invariant between them. heklang's `Harness` models it the same way and for the
    /// same reason.
    values: BTreeMap<String, Option<Arc<str>>>,
    /// Whether an unmentioned name answers `secret:NAME` instead of nothing.
    ///
    /// True only for `hekla test`, matching heklang's in-memory harness, so a test of an
    /// effect that reads a credential needs no setup and `hek test` gives the same answer
    /// against both worlds. False everywhere a real deployment runs: a stand-in there
    /// would turn "nobody set this" into a request sent to a made-up address.
    stand_in: bool,
}

impl SecretStore {
    /// The value behind a declared name, or `None` when this deployment did not set it.
    ///
    /// `None` is only reachable for a `secret NAME?`, because `Runtime::open` refuses to
    /// start with a required one unresolved. heklang's backstop if that ever fails is
    /// `ErrorKind::MissingSecret`, which wedges the invocation rather than panicking.
    pub fn get(&self, name: &str) -> Option<Arc<str>> {
        match self.values.get(name) {
            Some(supplied) => supplied.clone(),
            None if self.stand_in => Some(Arc::from(format!("secret:{name}"))),
            None => None,
        }
    }

    /// A store that supplies nothing, for a world that has no credentials to give: a
    /// command's host, and the bare appender tests seed a log through.
    pub fn empty() -> SecretStore {
        SecretStore {
            values: BTreeMap::new(),
            stand_in: false,
        }
    }

    /// The world `hekla test` runs in: whatever a `secret NAME = "..."` directive
    /// supplied, and `secret:NAME` for everything else.
    pub fn for_tests(values: BTreeMap<String, Option<Arc<str>>>) -> SecretStore {
        SecretStore {
            values,
            stand_in: true,
        }
    }

    /// Every credential this deployment holds, replaced by its name, wherever it appears
    /// in `text`.
    ///
    /// **A backstop, not the mechanism.** rule 16's two renderings are what keep a
    /// credential out of a journal key, an error and every `Display`, and they work
    /// because the redaction is a property of the value. This exists for the one channel
    /// that is below that seam: a transport error is written by `ureq` from whatever it
    /// chose to put in the message, so substituting the url hekla handed it only helps
    /// if `ureq` echoed it verbatim. Scanning for the value itself catches a rendering
    /// that normalised, percent-encoded or truncated the url around it.
    ///
    /// Skips anything shorter than [`MIN_SCRUB_LEN`]: a two-character credential appears
    /// inside ordinary words, and a scrubber that redacted half of every message would
    /// cost an operator more than the leak it prevents.
    pub fn redact(&self, text: &str) -> String {
        let mut out = text.to_owned();
        for (name, value) in &self.values {
            let Some(value) = value else { continue };
            if value.len() < MIN_SCRUB_LEN {
                continue;
            }
            if out.contains(value.as_ref()) {
                out = out.replace(value.as_ref(), &format!("{{SECRET:{name}}}"));
            }
        }
        out
    }
}

/// One declared credential and what this deployment does about it.
///
/// Carries no value, by construction rather than by discipline: every reporting surface
/// takes one of these, so none of them is able to print a credential even by mistake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub name: String,
    /// `secret NAME?`. An unset optional is reported, never refused.
    pub optional: bool,
    /// The file the declaration was written in, as heklang labels it.
    pub module: Option<String>,
    /// Where this deployment was told to look, whether or not anything was there:
    /// `env HEKLA_SECRET_STRIPE_KEY`, or `file /run/secrets/stripe_key`.
    pub source: String,
    /// `env` or `file`, for a reader that parses rather than reads.
    pub kind: &'static str,
    /// Present exactly when the credential resolved. See [`fingerprint`].
    pub fingerprint: Option<String>,
    /// Why the source could not be read, when it was there and unreadable.
    ///
    /// Distinct from simply absent, and reported rather than raised, because the two
    /// want different answers from different callers. A mount an operator cannot read is
    /// a broken deployment and [`refusal`] treats it as missing; but `hekla plan` runs
    /// as whatever user the pipeline uses, and failing the whole plan over one
    /// root-only `/run/secrets` file would report no declaration diff at all for the run
    /// that most needs one.
    pub error: Option<String>,
}

impl Resolution {
    pub fn resolved(&self) -> bool {
        self.fingerprint.is_some()
    }

    /// Whether this one stops a deploy: required, and nothing supplied it.
    ///
    /// A source that is present and unreadable counts, and says so: a deployment that
    /// meant to supply a credential and could not is not the same as one that chose not
    /// to, and serving on it would wedge at the first call.
    pub fn missing(&self) -> bool {
        !self.optional && !self.resolved()
    }
}

/// A short, stable id for a credential's value, so an operator can tell two deployments
/// apart without seeing either.
///
/// Domain-separated by the declaration's name, so the same value under two names
/// fingerprints differently and a fingerprint published from one project says nothing
/// about the same credential in another. Truncated because the job is comparison, not
/// identity: the full digest of a credential is a stronger oracle than anyone needs.
fn fingerprint(name: &str, value: &str) -> String {
    // `Zeroizing`, because this is a second copy of the credential and the whole reason
    // `read` returns one is that a plaintext copy must not outlive its use. An ordinary
    // `Vec` here would leave the value in freed heap for the life of the process and
    // make `SecretStore`'s claim to be the only thing holding one untrue.
    let mut material = Zeroizing::new(Vec::with_capacity(name.len() + 1 + value.len()));
    material.extend_from_slice(name.as_bytes());
    material.push(0);
    material.extend_from_slice(value.as_bytes());
    let mut digest = sha256_hex(&material);
    digest.truncate(FINGERPRINT_LEN);
    digest
}

/// Where one declared name is read from, given what the config says about it.
///
/// A relative `file` is reported as the path that will actually be opened, not as it was
/// written: resolving against the project root is the rule, and a report naming
/// `file notify-url` leaves out the one thing an operator needs to find it.
fn source_of(name: &str, config: &Config, root: &Path) -> (String, &'static str, SecretSource) {
    match config.secrets.get(name) {
        Some(SecretSource::Env { env }) => (format!("env {env}"), "env", SecretSource::env(env)),
        Some(SecretSource::File { file }) => (
            format!("file {}", root.join(file).display()),
            "file",
            SecretSource::file(file),
        ),
        None => {
            let var = format!("{ENV_PREFIX}{name}");
            let source = format!("env {var}");
            (source, "env", SecretSource::env(var))
        }
    }
}

/// Read one source. Absence is `Ok(None)`; a source that is there and unreadable is an
/// error, because that is a deployment that meant to supply something and failed.
///
/// A relative `file` resolves against the project root, not the working directory. An
/// absolute path is what a host mount produces (`/run/secrets/...`) and a relative one is
/// what a container image or a checkout produces, and neither should depend on where
/// `hekla` happened to be invoked from: `hekla serve ./app` and `cd app && hekla serve`
/// must resolve the same credential.
fn read(
    name: &str,
    source: &SecretSource,
    root: &Path,
) -> anyhow::Result<Option<Zeroizing<String>>> {
    match source {
        SecretSource::Env { env: var } => match env::var(var) {
            Ok(value) => Ok(Some(Zeroizing::new(value))),
            Err(env::VarError::NotPresent) => Ok(None),
            Err(err) => Err(err).with_context(|| var.clone()),
        },
        SecretSource::File { file } => {
            let path = root.join(file);
            match fs::read_to_string(&path) {
                Ok(value) => Ok(Some(Zeroizing::new(trim_one_newline(value)))),
                Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(err) => Err(err)
                    .with_context(|| format!("reading secret `{name}` from {}", path.display())),
            }
        }
    }
}

/// Drop one trailing line ending, and only one.
///
/// Every `echo x > secret` writes one and every orchestrator that mounts a credential as
/// a file produces one, so a store that kept it would send `Bearer sk_live_abc\n` and
/// leave an operator staring at a 401 with a value that looks right. One, not all, so a
/// credential whose own last character is a newline is still expressible.
fn trim_one_newline(mut value: String) -> String {
    if value.ends_with('\n') {
        value.pop();
        if value.ends_with('\r') {
            value.pop();
        }
    }
    value
}

/// Resolve every `secret` the program declares.
///
/// Returns the store the host reads and a report per declaration, in declaration order.
///
/// **Infallible.** A source that is present and unreadable lands in the report as an
/// unresolved credential carrying its reason, rather than as an error out of here.
/// Nothing here decides what that costs: `Runtime::open` and `Runtime::open_quiescent`
/// refuse over it, `Runtime::open_following` and `hekla plan` report it. Raising instead
/// would take that choice away from all four, and would make `hekla plan` against a
/// deployment whose `/run/secrets` mount this user cannot read produce no plan at all.
pub fn resolve(program: &Program, config: &Config, root: &Path) -> (SecretStore, Vec<Resolution>) {
    let mut values = BTreeMap::new();
    let mut report = Vec::with_capacity(program.secrets.len());
    for def in &program.secrets {
        let SecretDef {
            name,
            module,
            optional,
            ..
        } = def;
        let (source, kind, from) = source_of(name, config, root);
        let (value, error) = match read(name, &from, root) {
            Ok(value) => (value, None),
            Err(err) => (None, Some(format!("{err:#}"))),
        };
        let fingerprint = value.as_ref().map(|value| fingerprint(name, value));
        values.insert(name.clone(), value.map(|value| Arc::from(value.as_str())));
        report.push(Resolution {
            name: name.clone(),
            optional: *optional,
            module: module.clone(),
            source,
            kind,
            fingerprint,
            error,
        });
    }
    let store = SecretStore {
        values,
        stand_in: false,
    };
    (store, report)
}

/// The message a runtime refuses to start with, or `None` when nothing is missing.
///
/// Every missing name at once rather than the first, matching
/// `KeyStore::verify_masters_present`: an operator fixing one credential per restart is
/// the failure mode that shape exists to avoid. A source that failed to read names why,
/// because "not set" would send an operator looking in the wrong place for a file that
/// is right there and unreadable.
pub fn refusal(report: &[Resolution], verb: &str) -> Option<String> {
    let missing: Vec<&Resolution> = report.iter().filter(|one| one.missing()).collect();
    if missing.is_empty() {
        return None;
    }
    let named = missing
        .iter()
        .map(|one| match &one.error {
            Some(why) => format!("`{}` ({}: {why})", one.name, one.source),
            None => format!("`{}` ({})", one.name, one.source),
        })
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "this project declares deployment credential(s) this deployment has not set, so it cannot {verb}: {named}. Set each one, or name a different source for it under [secrets] in hekla.toml. A credential that is genuinely optional is declared `secret NAME?`, and an unset one is then a branch the program takes rather than a refusal"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_trailing_newline_goes_and_no_more() {
        assert_eq!(trim_one_newline("token\n".to_owned()), "token");
        assert_eq!(trim_one_newline("token\r\n".to_owned()), "token");
        assert_eq!(trim_one_newline("token\n\n".to_owned()), "token\n");
        assert_eq!(trim_one_newline("token".to_owned()), "token");
        assert_eq!(trim_one_newline(String::new()), "");
    }

    #[test]
    fn a_fingerprint_is_short_and_domain_separated() {
        let one = fingerprint("A", "value");
        let other = fingerprint("B", "value");
        assert_eq!(one.len(), FINGERPRINT_LEN);
        assert_ne!(one, other, "the same value under two names must differ");
        assert_eq!(one, fingerprint("A", "value"), "and it must be stable");
    }

    #[test]
    fn an_unconfigured_name_falls_back_to_the_convention() {
        let (source, kind, _) = source_of("STRIPE_KEY", &Config::default(), Path::new("."));
        assert_eq!(source, "env HEKLA_SECRET_STRIPE_KEY");
        assert_eq!(kind, "env");
    }

    #[test]
    fn a_missing_optional_is_not_a_refusal() {
        let report = vec![Resolution {
            name: "SENTRY_DSN".to_owned(),
            optional: true,
            module: None,
            source: "env HEKLA_SECRET_SENTRY_DSN".to_owned(),
            kind: "env",
            fingerprint: None,
            error: None,
        }];
        assert!(!report[0].missing());
        assert_eq!(refusal(&report, "serve"), None);
    }

    #[test]
    fn a_refusal_names_every_missing_credential_at_once() {
        let missing = |name: &str| Resolution {
            name: name.to_owned(),
            optional: false,
            module: None,
            source: format!("env HEKLA_SECRET_{name}"),
            kind: "env",
            fingerprint: None,
            error: None,
        };
        let report = vec![missing("ONE"), missing("TWO")];
        let message = refusal(&report, "serve").expect("two required credentials are unset");
        assert!(message.contains("`ONE`"), "{message}");
        assert!(message.contains("`TWO`"), "{message}");
        assert!(message.contains("serve"), "{message}");
    }
}
