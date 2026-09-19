//! Subject-scoped deterministic encryption and the per-subject key store.
//!
//! A field marked `@subject(buyer)` is encrypted under a key scoped to the identity
//! `(subject, subject_value)`, where the subject is the **declared name** of the type
//! `buyer` has: `subject Customer(Int)` files every such key under `Customer`. The field
//! the annotation names is where the id is read from and is local to one declaration;
//! the subject is the namespace and is not. Encryption is deterministic
//! (AES-SIV, RFC 5297): the same plaintext under the same key and field yields the
//! same ciphertext, so it works as a tag the index can match on, a payload value,
//! and a read-model column all at once. Erasing a subject is deleting its key row,
//! which makes every value encrypted under it unmatchable and unreadable across the
//! log and every read model simultaneously.
//!
//! Key material never leaves this module in the clear on disk: each per-subject
//! secret is a random AES-SIV key, wrapped with AES-256-GCM under a master key held
//! only in memory (from `HEKLA_MASTER_KEY`). The wrapping master is recorded per row
//! (`master_key_id`) so masters can rotate online, rewrapping row by row without a
//! stop-the-world pass and without changing any ciphertext. Losing the master is
//! total, unrecoverable loss of every subject-scoped value.

use std::cell::RefCell;
use std::collections::HashMap;
use std::env;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use aes_gcm::Aes256Gcm;
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_siv::Aes256SivAead;
use anyhow::{Context, anyhow};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use zeroize::Zeroizing;

use crate::opdb::{OpDb, ParentRef, RewrapUpdate, SubjectKey, Wrapping};

/// The AES-256-SIV key length: two 256-bit keys (S2V + CTR), so 64 bytes.
const SIV_KEY_LEN: usize = 64;
/// The AES-256-GCM master (wrapping) key length.
const MASTER_KEY_LEN: usize = 32;
/// The AES-256-GCM nonce length, prepended to each wrapped key.
const WRAP_NONCE_LEN: usize = 12;
/// The associated-data version byte, bound into every ciphertext so a future scheme
/// change is detectable and a value cannot be reinterpreted under new rules.
const AD_VERSION: u8 = 1;

/// The reserved subject that holds the global uniqueness secret. It backs the
/// `unique` tags that must survive erasure, so it is never deletable.
pub(crate) const GLOBAL_SUBJECT: &str = "_hekla_global";
const GLOBAL_SUBJECT_VALUE: &str = "global";

/// The set of master keys the runtime holds, keyed by a fingerprint id. One is the
/// primary (new writes wrap under it); the rest are kept only so rows wrapped under
/// a previous master can still be unwrapped during a rotation.
#[derive(Clone)]
pub struct MasterKeys {
    primary_id: String,
    keys: HashMap<String, [u8; MASTER_KEY_LEN]>,
}

impl MasterKeys {
    /// Build a master set from the primary key and any previous keys still needed to
    /// unwrap not-yet-rotated rows. Each key's id is a fingerprint of its bytes, so
    /// the same key always has the same id and a genuinely new key gets a new one.
    pub fn new(primary: [u8; MASTER_KEY_LEN], previous: Vec<[u8; MASTER_KEY_LEN]>) -> MasterKeys {
        let primary_id = fingerprint(&primary);
        let mut keys = HashMap::new();
        for key in previous {
            keys.insert(fingerprint(&key), key);
        }
        keys.insert(primary_id.clone(), primary);
        MasterKeys { primary_id, keys }
    }

    fn primary(&self) -> (&str, &[u8; MASTER_KEY_LEN]) {
        (
            &self.primary_id,
            self.keys.get(&self.primary_id).expect("primary is present"),
        )
    }

    fn get(&self, id: &str) -> Option<&[u8; MASTER_KEY_LEN]> {
        self.keys.get(id)
    }
}

/// Erase a subject by deleting its key row, which shreds every value encrypted under
/// it across the log, the tag index, and every read model at once. Refuses to delete
/// the reserved global uniqueness secret. Returns whether a key was removed. No
/// master key is needed: this is a plain row delete, so the `hekla erase` CLI can call
/// it without one.
pub fn erase_subject(opdb: &OpDb, subject: &str, subject_value: &str) -> anyhow::Result<bool> {
    if subject == GLOBAL_SUBJECT {
        anyhow::bail!("the global uniqueness secret cannot be erased");
    }
    opdb.delete_subject_key(subject, subject_value)
}

/// A stable id for a master key: the full hex SHA-256 of its bytes. The full digest
/// (not a truncation) so two distinct masters cannot collide onto one id and shadow
/// each other in the key set.
fn fingerprint(key: &[u8; MASTER_KEY_LEN]) -> String {
    crate::hash::sha256_hex(key)
}

/// Read the master keys from the environment: `HEKLA_MASTER_KEY` (the primary, a
/// base64 32-byte key) and, optionally, `HEKLA_MASTER_KEY_PREVIOUS` (a comma-separated
/// list of prior keys still needed to unwrap rows not yet rotated). Returns `None`
/// when `HEKLA_MASTER_KEY` is unset, so a project that uses no subjects needs no key.
pub fn master_keys_from_env() -> anyhow::Result<Option<MasterKeys>> {
    let Ok(primary) = env::var("HEKLA_MASTER_KEY") else {
        return Ok(None);
    };
    let primary = decode_master_key(&primary).context("HEKLA_MASTER_KEY")?;
    let previous = match env::var("HEKLA_MASTER_KEY_PREVIOUS") {
        Ok(list) => list
            .split(',')
            .filter(|part| !part.trim().is_empty())
            .map(|part| decode_master_key(part).context("HEKLA_MASTER_KEY_PREVIOUS"))
            .collect::<anyhow::Result<Vec<_>>>()?,
        Err(_) => Vec::new(),
    };
    Ok(Some(MasterKeys::new(primary, previous)))
}

/// Decode a base64 (standard or url-safe) 32-byte master key.
fn decode_master_key(encoded: &str) -> anyhow::Result<[u8; MASTER_KEY_LEN]> {
    let trimmed = encoded.trim();
    let bytes = URL_SAFE_NO_PAD
        .decode(trimmed)
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(trimmed))
        .context("master key is not valid base64")?;
    bytes
        .try_into()
        .map_err(|_| anyhow!("master key must be exactly {MASTER_KEY_LEN} bytes (base64-encoded)"))
}

/// The per-subject key store and the deterministic encryption built on it.
#[derive(Clone)]
pub struct KeyStore {
    opdb: Arc<Mutex<OpDb>>,
    masters: MasterKeys,
}

impl KeyStore {
    pub fn new(opdb: Arc<Mutex<OpDb>>, masters: MasterKeys) -> KeyStore {
        KeyStore { opdb, masters }
    }

    /// Encrypt `plaintext` under the subject `(subject, subject_value)`,
    /// creating the subject's key on first use. Returns the base64url ciphertext
    /// used as the tag value, the payload value, and the read-model column.
    ///
    /// For a subject that declares no parent. One that does goes through
    /// [`KeyStore::encrypt_subject_in`], which takes the ancestor ids too.
    pub fn encrypt_subject(
        &self,
        subject: &str,
        subject_value: &str,
        field: &str,
        plaintext: &str,
    ) -> anyhow::Result<String> {
        self.encrypt_subject_in(&[(subject, subject_value)], field, plaintext)
    }

    /// Encrypt `plaintext` under the first subject in `chain`, creating its key and
    /// every ancestor's on first use.
    ///
    /// `chain` is the subject and its ancestors, nearest first: `[(\"Customer\", \"88\"),
    /// (\"Shop\", \"7\")]` mints customer 88's key wrapped under shop 7's, so deleting
    /// shop 7's row makes it unopenable. The caller supplies the whole chain because
    /// only it has the ids: they live in the event being written, which is why heklang
    /// insists every event sealing under a child carries its ancestors.
    pub fn encrypt_subject_in(
        &self,
        chain: &[(&str, &str)],
        field: &str,
        plaintext: &str,
    ) -> anyhow::Result<String> {
        let secret = self.get_or_create_secret_in(chain)?;
        encrypt_with(&secret, field, plaintext.as_bytes())
    }

    /// Encrypt `plaintext` under the global uniqueness key (created on first use).
    /// The resulting tag survives subject erasure, so a global uniqueness check
    /// still fires after the subject's own data is shredded.
    pub fn encrypt_global(&self, field: &str, plaintext: &str) -> anyhow::Result<String> {
        let secret = self.get_or_create_secret_in(&[(GLOBAL_SUBJECT, GLOBAL_SUBJECT_VALUE)])?;
        encrypt_with(&secret, field, plaintext.as_bytes())
    }

    /// Encrypt a filter value under an existing subject key, for query lowering.
    /// Returns `Ok(None)` when the subject has no key (never seen, or erased): a query
    /// is a read path and must not create or resurrect key material, so the caller
    /// makes the clause match nothing rather than minting a key.
    pub fn encrypt_subject_existing(
        &self,
        subject: &str,
        subject_value: &str,
        field: &str,
        plaintext: &str,
    ) -> anyhow::Result<Option<String>> {
        self.load_secret(subject, subject_value)?
            .map(|secret| encrypt_with(&secret, field, plaintext.as_bytes()))
            .transpose()
    }

    /// Decrypt a subject-scoped ciphertext. Returns `Ok(None)` when the value is
    /// unreadable under the current key, the erasure guarantee: either the subject's
    /// key is gone (erased, or never created), or the ciphertext will not decrypt under
    /// the present key (a stale row under a superseded key, or tampering). `Err` is
    /// reserved for a key that cannot be obtained at all (a missing/rotated-away master
    /// or a corrupt key wrapping).
    pub fn decrypt_subject(
        &self,
        subject: &str,
        subject_value: &str,
        field: &str,
        ciphertext: &str,
    ) -> anyhow::Result<Option<String>> {
        let Some(secret) = self.load_secret(subject, subject_value)? else {
            return Ok(None);
        };
        Ok(plaintext_under(&secret, field, ciphertext))
    }

    /// Erase a subject by deleting its key. Refuses to delete the reserved global
    /// secret. Returns whether a key was removed.
    pub fn erase(&self, subject: &str, subject_value: &str) -> anyhow::Result<bool> {
        erase_subject(&self.lock(), subject, subject_value)
    }

    /// Rewrap every subject key not already under the primary master, for a master
    /// rotation. Ciphertext is unaffected (only the key wrapping changes). Returns
    /// how many rows were rewrapped.
    pub fn rotate(&self) -> anyhow::Result<usize> {
        let (primary_id, primary) = self.masters.primary();
        let rows = self.lock().all_subject_keys()?;
        // Unwrap and rewrap off-lock, then commit every change in one transaction, so
        // the pass is atomic (no half-rotated store on a crash) and pays one fsync.
        let mut updates: Vec<RewrapUpdate> = Vec::new();
        for (field, value, wrapped, master_id) in rows {
            if master_id == primary_id {
                continue;
            }
            let master = self
                .masters
                .get(&master_id)
                .ok_or_else(|| anyhow!("no master `{master_id}` to unwrap subject `{field}`"))?;
            let secret = unwrap_key(master, &wrapped)?;
            let rewrapped_key = wrap_key(primary, &secret)?;
            // Carry the master id we unwrapped under as the compare-and-set guard, so a
            // concurrent erase-then-recreate (which mints a fresh secret under the
            // primary) is not overwritten by this stale rewrap.
            updates.push((
                field,
                value,
                rewrapped_key,
                primary_id.to_owned(),
                master_id,
            ));
        }
        if updates.is_empty() {
            return Ok(0);
        }
        // The returned count is the rows actually rewrapped, which is below
        // `updates.len()` when a compare-and-set skips a concurrently recreated row.
        self.lock().rewrap_subject_keys(&updates)
    }

    /// Verify every master key referenced by a stored subject row is configured, so a
    /// wrong or rotated-away `HEKLA_MASTER_KEY` fails fast at boot with a clear message
    /// rather than silently at first read. A stored id is the SHA-256 fingerprint of
    /// the master's bytes, so a matching id proves the bytes themselves are correct.
    pub fn verify_masters_present(&self) -> anyhow::Result<()> {
        let ids = self.lock().distinct_master_key_ids()?;
        let missing: Vec<String> = ids
            .into_iter()
            .filter(|id| self.masters.get(id).is_none())
            .collect();
        if !missing.is_empty() {
            let missing = missing.join(", ");
            anyhow::bail!(
                "stored subject data was wrapped under master key(s) not configured now: {missing}. Set HEKLA_MASTER_KEY (and HEKLA_MASTER_KEY_PREVIOUS, comma-separated, for masters mid-rotation) to the master(s) that wrapped this data. Losing a master is permanent, unrecoverable loss of every subject it wrapped"
            );
        }
        Ok(())
    }

    /// Load and unwrap a subject secret, or `None` if it cannot be reached.
    ///
    /// `None` covers two shapes and they mean the same thing to every caller: the row is
    /// gone (erased, or never created), or the row is there and an ancestor's is not, so
    /// nothing will ever unwrap it again. A parent's deletion is what makes the second
    /// one true, and it is the whole of what a hierarchy buys: one row delete, and every
    /// key beneath it stops being reachable at the same instant, with no walk and no
    /// second write.
    ///
    /// `Err` stays what it was: a key that cannot be *obtained*, meaning a master that is
    /// not configured or a wrapping that will not open under the right key. The split is
    /// load-bearing, because it is what lets [`KeyStore::get_or_create_secret_in`]
    /// replace an unreachable row without ever replacing a merely misconfigured one.
    fn load_secret(
        &self,
        subject: &str,
        subject_value: &str,
    ) -> anyhow::Result<Option<Zeroizing<Vec<u8>>>> {
        self.load_secret_at(subject, subject_value, 0, None)
            .map(|found| found.map(|(secret, _)| secret))
    }

    /// [`KeyStore::load_secret`] plus the wrapping the row was found under, which the
    /// mint path needs to tell "no row" from "a row nothing can open".
    fn load_secret_at(
        &self,
        subject: &str,
        subject_value: &str,
        depth: usize,
        cache: Option<&RefCell<SecretCache>>,
    ) -> anyhow::Result<Option<(Zeroizing<Vec<u8>>, Wrapping)>> {
        // The subject graph is checked acyclic at parse time and is finite, so a real
        // chain is shorter than this by a wide margin. The cap is here for a database
        // somebody edited by hand, where a cycle would otherwise recurse until the stack
        // ran out and take the process with it.
        if depth > MAX_SUBJECT_DEPTH {
            anyhow::bail!(
                "subject `{subject}` is more than {MAX_SUBJECT_DEPTH} levels deep, which a declared hierarchy cannot be: the key store has a cycle in its parent pointers"
            );
        }
        let Some(row) = self.lock().get_subject_key(subject, subject_value)? else {
            return Ok(None);
        };
        match &row.wrapping {
            Wrapping::Master(master_id) => {
                let master = self.master_for(master_id, subject)?;
                Ok(Some((unwrap_key(master, &row.wrapped)?, row.wrapping)))
            }
            Wrapping::Parent(parent_ref) => {
                let (parent_subject, parent_value) =
                    (parent_ref.subject.as_str(), parent_ref.value.as_str());
                // Through the cache, so a page of rows sharing a tenant unwraps that
                // tenant once rather than once per row. Without it the recursion undoes
                // the whole reason [`RowDecryptor`] exists, and undoes more of it the
                // deeper the hierarchy goes.
                let Some(parent) =
                    self.cached_secret(parent_subject, parent_value, depth + 1, cache)?
                else {
                    // The parent is gone, so this row's ciphertext is unrecoverable and
                    // the subject reads as erased. Exactly what deleting the parent was
                    // for.
                    return Ok(None);
                };
                // The generation first, because it is what tells a shred from tampering.
                // A parent erased and written to again has a fresh secret, so nothing
                // wrapped under the old one can ever open: this row is unreadable for the
                // same reason an erased subject is, and reads the same way. Reporting
                // `Err` for it would 500 every read of a legitimately shredded row and
                // wedge its write path for good, because `get_or_create_secret_in`
                // replaces a row that reads absent and propagates one that errors.
                if parent_ref.fingerprint != parent_fingerprint(&parent) {
                    tracing::debug!(
                        "subject `{subject}` = `{subject_value}` was wrapped under an earlier `{parent_subject}` = `{parent_value}`, which has since been erased and recreated; reading as erased"
                    );
                    return Ok(None);
                }
                // The generation matches, so the key this was wrapped under has not
                // moved and a wrapping that still will not open was altered outside
                // hekla. That is the "cannot be obtained" case a root reports too, and it
                // has to stay an error: reporting it as an erasure would tell an operator
                // their shred worked when what actually happened is that somebody wrote
                // to the key store.
                let aad = child_aad(subject, subject_value, parent_subject, parent_value);
                let secret = unwrap_key_under(&child_kek(&parent), &row.wrapped, &aad)?;
                Ok(Some((secret, row.wrapping)))
            }
            Wrapping::Neither => anyhow::bail!(
                "subject `{subject}` = `{subject_value}` is wrapped under neither a master nor a parent, which the schema forbids: the row has been edited outside hekla"
            ),
        }
    }

    /// [`KeyStore::load_secret_at`] through a cache, so a walk up the same chain from
    /// many different children pays for each ancestor once.
    ///
    /// Caches absence too, which matters more here than for a leaf: an erased tenant is
    /// the case a scan of its members hits on every single row.
    fn cached_secret(
        &self,
        subject: &str,
        subject_value: &str,
        depth: usize,
        cache: Option<&RefCell<SecretCache>>,
    ) -> anyhow::Result<Option<Zeroizing<Vec<u8>>>> {
        let Some(cache) = cache else {
            return Ok(self
                .load_secret_at(subject, subject_value, depth, None)?
                .map(|(secret, _)| secret));
        };
        let key = (subject.to_owned(), subject_value.to_owned());
        if let Some(cached) = cache.borrow().get(&key) {
            return Ok(cached.clone());
        }
        let loaded = self
            .load_secret_at(subject, subject_value, depth, Some(cache))?
            .map(|(secret, _)| secret);
        cache.borrow_mut().insert(key, loaded.clone());
        Ok(loaded)
    }

    /// The configured master that wrapped a stored subject key, or an error naming
    /// the master that is missing: without it the subject cannot be read at all.
    fn master_for(&self, master_id: &str, subject: &str) -> anyhow::Result<&[u8; MASTER_KEY_LEN]> {
        self.masters.get(master_id).ok_or_else(|| {
            anyhow!(
                "cannot unwrap subject `{subject}`: master key `{master_id}` is not configured (was HEKLA_MASTER_KEY rotated away without keeping the previous key?)"
            )
        })
    }

    /// Get the secret for the first subject in `chain`, creating it and every ancestor
    /// it needs on first use.
    ///
    /// `chain` is the subject and its ancestors, nearest first: `[(Customer, 88),
    /// (Shop, 7)]`. It is passed whole rather than looked up, because only the caller has
    /// it: a parent's *id* lives in the event being written and nowhere else, which is
    /// why heklang insists every event sealing under a child carries its ancestors.
    ///
    /// Concurrency-safe in the same way it was: a creating thread that loses the insert
    /// race uses the secret that actually persisted, never its own discarded one, which
    /// would produce permanently unrecoverable ciphertext.
    fn get_or_create_secret_in(
        &self,
        chain: &[(&str, &str)],
    ) -> anyhow::Result<Zeroizing<Vec<u8>>> {
        // Bounded retry, because one attempt can legitimately end with nothing to return:
        // another writer wins the insert and its row is itself unreachable by the time
        // this thread reads it back, which is what happens when an erase lands in between.
        // That is a state this function already knows how to handle, observed one step too
        // late, so the answer is to look again rather than to invent a key or to fail.
        //
        // Bounded rather than a `loop`: a store being erased in a tight cycle would
        // otherwise hang the request, and a write that cannot find a stable key after this
        // many tries is better off failing loudly.
        for _ in 0..MINT_ATTEMPTS {
            if let Some(secret) = self.try_mint_in(chain)? {
                return Ok(secret);
            }
        }
        let (subject, subject_value) = chain[0];
        anyhow::bail!(
            "subject `{subject}` = `{subject_value}` is being erased and recreated faster than a key can be minted for it"
        )
    }

    /// One attempt at [`KeyStore::get_or_create_secret_in`].
    ///
    /// `Ok(None)` means the store moved underneath this thread and the caller should look
    /// again: it is not an absence, and no caller outside that retry sees it.
    fn try_mint_in(&self, chain: &[(&str, &str)]) -> anyhow::Result<Option<Zeroizing<Vec<u8>>>> {
        let Some(((subject, subject_value), ancestors)) = chain.split_first() else {
            anyhow::bail!("a subject chain cannot be empty");
        };
        let (subject, subject_value) = (*subject, *subject_value);
        if let Some((secret, _)) = self.load_secret_at(subject, subject_value, 0, None)? {
            return Ok(Some(secret));
        }
        // A row may be present and unreachable, which happens when this subject outlived
        // an ancestor's erasure. Its ciphertext is already unrecoverable, so the row reads
        // as absent and is replaced rather than hard-failing on the unwrap. Safe precisely
        // because `load_secret_at` said `Ok(None)` rather than `Err`: a master that is
        // merely missing never reaches here.
        let stale = self.lock().get_subject_key(subject, subject_value)?;
        let fresh = random_secret()?;
        // The parent's secret is kept, not just used: unwrapping the row that actually
        // persists needs it again, and reaching for it a second time through
        // `load_secret` is the redundant walk this exists to avoid.
        let (candidate, parent) = match ancestors.split_first() {
            None => {
                let (primary_id, primary) = self.masters.primary();
                let candidate = SubjectKey {
                    wrapped: wrap_key(primary, &fresh)?,
                    wrapping: Wrapping::Master(primary_id.to_owned()),
                };
                (candidate, None)
            }
            Some(((parent_subject, parent_value), _)) => {
                // Minting the parent first, recursively, because wrapping under it needs
                // its secret and it may not exist yet. An ancestor erased and written to
                // again gets a fresh key here, which is the same point-in-time shred every
                // subject has always had, applied one level up.
                let parent = self.get_or_create_secret_in(ancestors)?;
                let aad = child_aad(subject, subject_value, parent_subject, parent_value);
                let candidate = SubjectKey {
                    wrapped: wrap_key_under(&child_kek(&parent), &fresh, &aad)?,
                    wrapping: Wrapping::Parent(ParentRef {
                        subject: (*parent_subject).to_owned(),
                        value: (*parent_value).to_owned(),
                        fingerprint: parent_fingerprint(&parent),
                    }),
                };
                (candidate, Some(parent))
            }
        };
        // Insert-if-absent and re-read atomically under one lock, so the persisted row
        // (this thread's or a racing thread's) is always the one used, and a racing erase
        // cannot leave us with nothing.
        let persisted = self.lock().get_or_insert_subject_key(
            subject,
            subject_value,
            &candidate,
            stale.as_ref(),
        )?;
        // Whatever row persisted is the one to encrypt under, this thread's or a racing
        // writer's. Using the discarded `fresh` when another row won would write
        // ciphertext nothing could ever read back, which is the failure the insert-race
        // handling exists for.
        //
        // Deliberately not short-circuiting on "the row is mine". Doing so would hand back
        // a secret whose parent an eraser may have just deleted, committing a write whose
        // content is unreadable the moment it lands. Opening the row instead means an
        // erase that raced this write sends it round the retry, where the parent is minted
        // again and the value is readable: the documented rule that a write after an erase
        // gets a fresh key, applied to a write that merely finished after one.
        match (&persisted.wrapping, &parent) {
            (Wrapping::Master(master_id), _) => {
                unwrap_key(self.master_for(master_id, subject)?, &persisted.wrapped).map(Some)
            }
            (Wrapping::Parent(parent_ref), Some(parent))
                if (parent_ref.subject.as_str(), parent_ref.value.as_str()) == ancestors[0]
                    && parent_ref.fingerprint == parent_fingerprint(parent) =>
            {
                let aad = child_aad(
                    subject,
                    subject_value,
                    &parent_ref.subject,
                    &parent_ref.value,
                );
                unwrap_key_under(&child_kek(parent), &persisted.wrapped, &aad).map(Some)
            }
            // The winner's row hangs from an ancestor, or a generation of one, that this
            // write did not name, so its parent has to be loaded to open it. Reachable two
            // ways: two events disagreeing about a subject's parent, which
            // `docs/effects.md` says outright that nothing can check, and an erase landing
            // between that writer's insert and this read.
            _ => Ok(self.load_secret(subject, subject_value)?),
        }
    }

    fn lock(&self) -> MutexGuard<'_, OpDb> {
        self.opdb.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The masters this store wraps subject keys under.
    ///
    /// For building a second store over a *different* connection, which is the one
    /// reason to want them: [`KeyStore::encrypt_subject_existing`] takes this store's
    /// opdb mutex on every sealed write, and that mutex is shared with every effect's
    /// hot path. A reader folding a whole log through it would stall live work, so it
    /// opens its own connection and rewraps under the same masters instead.
    pub(crate) fn masters(&self) -> &MasterKeys {
        &self.masters
    }

    /// A decryptor that caches unwrapped subject secrets for the life of one request,
    /// so a scan of many rows sharing a subject unwraps that key once, not per row.
    /// Bounded to a request and dropped after, so it never outlives an erasure.
    pub fn row_decryptor(&self) -> RowDecryptor<'_> {
        RowDecryptor {
            keystore: self,
            secrets: RefCell::new(HashMap::new()),
        }
    }
}

/// Unwrapped subject secrets cached by `(subject, subject_value)`; `None`
/// records an absent (erased or never-created) key so it is not re-loaded.
type SecretCache = HashMap<(String, String), Option<Zeroizing<Vec<u8>>>>;

/// A short-lived, per-request decrypt cache over a [`KeyStore`]. Caches the unwrapped
/// secret per subject (and its absence), so a page of rows sharing a subject pays the
/// opdb lock plus AES-GCM unwrap once rather than per row.
pub struct RowDecryptor<'a> {
    keystore: &'a KeyStore,
    secrets: RefCell<SecretCache>,
}

impl RowDecryptor<'_> {
    /// Whether the subject had a key the last time this decryptor loaded one, or
    /// `None` if it never looked.
    ///
    /// Reads the cache a preceding [`RowDecryptor::decrypt`] already filled, so it
    /// costs no database work. It exists because `decrypt` returns `Ok(None)` for two
    /// different situations, and a caller reporting to a human has to separate them:
    /// the key is gone (erasure, irreversible) or the key is present and this
    /// particular ciphertext will not decrypt under it (written under a superseded
    /// key, or corrupt).
    pub fn key_present(&self, subject: &str, subject_value: &str) -> Option<bool> {
        let cache_key = (subject.to_owned(), subject_value.to_owned());
        self.secrets.borrow().get(&cache_key).map(Option::is_some)
    }

    /// Decrypt a subject-scoped ciphertext, reusing a cached secret. `Ok(None)` when
    /// the value is unreadable under the current key: the subject's key is gone (erased
    /// or never created), or the ciphertext will not decrypt under the present key (a
    /// stale row under a superseded key, or tampering). `Err` is reserved for a key that
    /// cannot be obtained at all (a missing master or a corrupt key wrapping).
    pub fn decrypt(
        &self,
        subject: &str,
        subject_value: &str,
        field: &str,
        ciphertext: &str,
    ) -> anyhow::Result<Option<String>> {
        // Through the store's own cached walk rather than a lookup here, so every ancestor
        // on the way up lands in the same map. A page of one tenant's members unwraps the
        // tenant once; doing the memoisation at this level only would unwrap it per row.
        let secret = self
            .keystore
            .cached_secret(subject, subject_value, 0, Some(&self.secrets))?;
        match secret {
            Some(secret) => Ok(plaintext_under(&secret, field, ciphertext)),
            None => Ok(None),
        }
    }
}

/// Turn a loaded secret into plaintext, or `None` when the ciphertext will not decrypt
/// under it. A present secret that fails to decrypt means the value is unrecoverable
/// under the current key: a stale row left under a superseded key (the subject was
/// erased then recreated), or a corrupt/tampered ciphertext. Both read as absent (the
/// erasure guarantee) with a debug log, rather than surfacing as an error, which is
/// reserved for a key that cannot be obtained at all (a missing or rotated-away master,
/// handled by the caller before this point).
fn plaintext_under(secret: &[u8], field: &str, ciphertext: &str) -> Option<String> {
    match decrypt_with(secret, field, ciphertext) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => Some(text),
            Err(_) => {
                tracing::debug!(
                    "subject field `{field}` did not decode as UTF-8; reading as absent"
                );
                None
            }
        },
        Err(err) => {
            tracing::debug!(
                "subject field `{field}` did not decrypt under its current key: {err:#}; reading as absent"
            );
            None
        }
    }
}

/// The associated data bound into a ciphertext: the version byte and the field name,
/// so a value cannot be reinterpreted under a different field or scheme version.
fn associated_data(field: &str) -> Vec<u8> {
    let mut ad = Vec::with_capacity(1 + field.len());
    ad.push(AD_VERSION);
    ad.extend_from_slice(field.as_bytes());
    ad
}

/// Deterministically encrypt `plaintext` under a 64-byte SIV key, returning the
/// base64url ciphertext.
fn encrypt_with(secret: &[u8], field: &str, plaintext: &[u8]) -> anyhow::Result<String> {
    let cipher =
        Aes256SivAead::new_from_slice(secret).map_err(|_| anyhow!("invalid SIV key length"))?;
    let ad = associated_data(field);
    let ciphertext = cipher
        .encrypt(
            &Default::default(),
            Payload {
                msg: plaintext,
                aad: &ad,
            },
        )
        .map_err(|_| anyhow!("subject encryption failed"))?;
    Ok(URL_SAFE_NO_PAD.encode(ciphertext))
}

/// Decrypt a base64url SIV ciphertext under a 64-byte SIV key.
fn decrypt_with(secret: &[u8], field: &str, ciphertext: &str) -> anyhow::Result<Vec<u8>> {
    let cipher =
        Aes256SivAead::new_from_slice(secret).map_err(|_| anyhow!("invalid SIV key length"))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(ciphertext)
        .context("ciphertext is not valid base64url")?;
    let ad = associated_data(field);
    cipher
        .decrypt(
            &Default::default(),
            Payload {
                msg: &bytes,
                aad: &ad,
            },
        )
        .map_err(|_| anyhow!("subject decryption failed (tampered ciphertext or scheme mismatch)"))
}

/// A fresh random 64-byte AES-SIV key from OS entropy.
fn random_secret() -> anyhow::Result<Zeroizing<Vec<u8>>> {
    let mut secret = Zeroizing::new(vec![0u8; SIV_KEY_LEN]);
    getrandom::fill(&mut secret).context("gathering entropy for a subject key")?;
    Ok(secret)
}

/// Wrap a subject secret under a master key with AES-256-GCM, returning
/// `nonce || ciphertext`. A root's wrapping, which binds no associated data.
fn wrap_key(master: &[u8; MASTER_KEY_LEN], secret: &[u8]) -> anyhow::Result<Vec<u8>> {
    wrap_key_under(master, secret, &[])
}

/// Unwrap a subject secret produced by [`wrap_key`].
fn unwrap_key(master: &[u8; MASTER_KEY_LEN], wrapped: &[u8]) -> anyhow::Result<Zeroizing<Vec<u8>>> {
    unwrap_key_under(master, wrapped, &[])
}

/// Wrap a secret under any 32-byte AES-256-GCM key, binding `aad` into the result.
///
/// The key is a master for a root and [`child_kek`]'s derivation for a child; the `aad`
/// is empty for the first and both identities for the second.
fn wrap_key_under(
    key: &[u8; MASTER_KEY_LEN],
    secret: &[u8],
    aad: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| anyhow!("invalid wrapping key"))?;
    let mut nonce = [0u8; WRAP_NONCE_LEN];
    getrandom::fill(&mut nonce).context("gathering entropy for key wrapping")?;
    let ciphertext = cipher
        .encrypt(&nonce.into(), Payload { msg: secret, aad })
        .map_err(|_| anyhow!("wrapping a subject key failed"))?;
    let mut out = Vec::with_capacity(WRAP_NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Unwrap a secret produced by [`wrap_key_under`] with the same key and `aad`.
fn unwrap_key_under(
    key: &[u8; MASTER_KEY_LEN],
    wrapped: &[u8],
    aad: &[u8],
) -> anyhow::Result<Zeroizing<Vec<u8>>> {
    if wrapped.len() <= WRAP_NONCE_LEN {
        anyhow::bail!("wrapped key is too short");
    }
    let (nonce, ciphertext) = wrapped.split_at(WRAP_NONCE_LEN);
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| anyhow!("invalid wrapping key"))?;
    let nonce: [u8; WRAP_NONCE_LEN] = nonce.try_into().expect("checked length");
    let secret = cipher
        .decrypt(
            &nonce.into(),
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map_err(|_| anyhow!("unwrapping a subject key failed (wrong key?)"))?;
    Ok(Zeroizing::new(secret))
}

/// The domain separator for a child's wrapping key, so the bytes derived here cannot be
/// confused with the parent secret they come from or with anything derived from it later.
const KEK_DOMAIN: &[u8] = b"hekla:subject-kek:v1";

/// The domain separator for a parent generation's fingerprint. Distinct from
/// [`KEK_DOMAIN`] so the value recorded in a column cannot be the key that opens anything,
/// even though both are derived from the same secret.
const PARENT_FP_DOMAIN: &[u8] = b"hekla:subject-parent-fp:v1";

/// The version byte bound into a child's key wrapping, beside the identities. Separate
/// from [`AD_VERSION`], which versions the *data* scheme: these two can move apart.
const WRAP_AD_VERSION: u8 = 1;

/// How deep a parent chain may go before hekla calls it a cycle. The declared graph is
/// over type names, finite and checked acyclic at parse time, so no real hierarchy is
/// near this; it is a backstop against a key store edited outside hekla.
const MAX_SUBJECT_DEPTH: usize = 32;

/// How many times a mint looks again after losing a race to a row that is itself already
/// unreachable. Each attempt is one read plus one insert, and only a subject being erased
/// concurrently costs more than the first.
const MINT_ATTEMPTS: usize = 4;

/// The 32-byte AES-GCM key a child's secret is wrapped under, derived from its parent's
/// 64-byte AES-SIV secret.
///
/// A plain domain-separated SHA-256 rather than HKDF, and that is a judgement rather than
/// a shortcut: the input is already a uniformly random 64 bytes from the OS, which is
/// exactly the case HKDF-Extract exists to handle and therefore the case where it adds
/// nothing. The domain separator is the part that matters. A subject secret is an AES-SIV
/// key in its own right, and a key that both encrypts data and wraps other keys is the
/// mistake this prevents: nothing derived here can be fed back into `encrypt_with`.
///
/// Deleting the parent's row destroys the only copy of the parent secret, so this key
/// becomes underivable and every child wrapped under it becomes unopenable, at once and
/// without touching a single child row. That is the whole feature.
fn child_kek(parent_secret: &[u8]) -> [u8; MASTER_KEY_LEN] {
    let mut input = Zeroizing::new(Vec::with_capacity(KEK_DOMAIN.len() + parent_secret.len()));
    input.extend_from_slice(KEK_DOMAIN);
    input.extend_from_slice(parent_secret);
    crate::hash::sha256(&input)
}

/// Which generation of a parent a child was wrapped under: a domain-separated digest of
/// the parent's secret.
///
/// Recorded on the child so a failed unwrap can be explained rather than guessed at. The
/// secret is what the wrapping key is derived from, so a parent erased and recreated has a
/// different one and every key beneath the old one is unopenable: that is a shred, and the
/// fingerprint says so. A wrapping that will not open while the fingerprint still matches
/// is not a shred, because the key it was wrapped under has not moved.
///
/// Safe to store beside the ciphertext it describes: it is a preimage-resistant digest, and
/// it is domain-separated from the wrapping key derived from the same input, so holding it
/// gives no way to derive that key.
fn parent_fingerprint(parent_secret: &[u8]) -> String {
    let mut input = Zeroizing::new(Vec::with_capacity(
        PARENT_FP_DOMAIN.len() + parent_secret.len(),
    ));
    input.extend_from_slice(PARENT_FP_DOMAIN);
    input.extend_from_slice(parent_secret);
    crate::hash::sha256_hex(&input)
}

/// The associated data bound into a child's key wrapping: the version, and both
/// identities, length-prefixed so no two different pairs can render the same bytes.
///
/// It stops a wrapped child key from being moved to another row. Without it, copying one
/// row's `wrapped_key` onto another child of the same parent would hand that subject the
/// first one's key, and the store would never notice. Roots carry no such binding today
/// and are left alone: retrofitting one would make every existing row unopenable, and it
/// buys less there, since a root's wrapping is not derived from anything an attacker with
/// write access to this table could also reach.
fn child_aad(
    subject: &str,
    subject_value: &str,
    parent_subject: &str,
    parent_value: &str,
) -> Vec<u8> {
    let parts = [subject, subject_value, parent_subject, parent_value];
    let mut aad = Vec::with_capacity(1 + parts.iter().map(|part| part.len() + 4).sum::<usize>());
    aad.push(WRAP_AD_VERSION);
    for part in parts {
        aad.extend_from_slice(&(part.len() as u32).to_be_bytes());
        aad.extend_from_slice(part.as_bytes());
    }
    aad
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> KeyStore {
        let opdb = Arc::new(Mutex::new(OpDb::open_in_memory().unwrap()));
        KeyStore::new(opdb, MasterKeys::new([7u8; 32], vec![]))
    }

    #[test]
    fn round_trips_a_subject_value() {
        let ks = store();
        let ct = ks
            .encrypt_subject("customer_id", "42", "email", "a@b.c")
            .unwrap();
        let back = ks
            .decrypt_subject("customer_id", "42", "email", &ct)
            .unwrap();
        assert_eq!(back.as_deref(), Some("a@b.c"));
    }

    #[test]
    fn encryption_is_deterministic() {
        let ks = store();
        let a = ks
            .encrypt_subject("customer_id", "42", "email", "a@b.c")
            .unwrap();
        let b = ks
            .encrypt_subject("customer_id", "42", "email", "a@b.c")
            .unwrap();
        assert_eq!(
            a, b,
            "same subject + field + plaintext must match for tagging"
        );
    }

    #[test]
    fn associated_data_separates_fields_and_subjects() {
        let ks = store();
        let email = ks
            .encrypt_subject("customer_id", "42", "email", "same")
            .unwrap();
        let other_field = ks
            .encrypt_subject("customer_id", "42", "recovery", "same")
            .unwrap();
        let other_subject = ks
            .encrypt_subject("customer_id", "99", "email", "same")
            .unwrap();
        assert_ne!(
            email, other_field,
            "field name is bound into the ciphertext"
        );
        assert_ne!(email, other_subject, "each subject has its own key");
    }

    #[test]
    fn erasing_a_subject_makes_values_unreadable() {
        let ks = store();
        let ct = ks
            .encrypt_subject("customer_id", "42", "email", "a@b.c")
            .unwrap();
        assert!(ks.erase("customer_id", "42").unwrap());
        // The key is gone: decrypt yields None (shredded), not an error.
        let back = ks
            .decrypt_subject("customer_id", "42", "email", &ct)
            .unwrap();
        assert_eq!(back, None);
    }

    #[test]
    fn decrypting_an_unknown_subject_is_none() {
        let ks = store();
        // Well-formed ciphertext shape, but no key row: shredded / never existed.
        let ct = ks
            .encrypt_subject("customer_id", "42", "email", "x")
            .unwrap();
        let back = ks
            .decrypt_subject("customer_id", "999", "email", &ct)
            .unwrap();
        assert_eq!(back, None);
    }

    #[test]
    fn global_key_survives_subject_erasure() {
        let ks = store();
        let scoped = ks
            .encrypt_subject("customer_id", "42", "email", "a@b.c")
            .unwrap();
        let global = ks.encrypt_global("email", "a@b.c").unwrap();
        ks.erase("customer_id", "42").unwrap();
        // Scoped value is shredded; the global uniqueness token still decrypts.
        assert_eq!(
            ks.decrypt_subject("customer_id", "42", "email", &scoped)
                .unwrap(),
            None
        );
        assert_eq!(
            ks.decrypt_subject(GLOBAL_SUBJECT, GLOBAL_SUBJECT_VALUE, "email", &global)
                .unwrap()
                .as_deref(),
            Some("a@b.c")
        );
    }

    #[test]
    fn the_global_secret_cannot_be_erased() {
        let ks = store();
        ks.encrypt_global("email", "x").unwrap();
        assert!(ks.erase(GLOBAL_SUBJECT, GLOBAL_SUBJECT_VALUE).is_err());
    }

    #[test]
    fn concurrent_creation_agrees_on_one_secret() {
        // Two stores over one opdb race to create the same subject. Whoever wins the
        // insert, both must encrypt under the persisted secret and both must decrypt
        // each other's ciphertext.
        let opdb = Arc::new(Mutex::new(OpDb::open_in_memory().unwrap()));
        let masters = MasterKeys::new([3u8; 32], vec![]);
        let a = KeyStore::new(opdb.clone(), masters.clone());
        let b = KeyStore::new(opdb, masters);
        let from_a = a
            .encrypt_subject("customer_id", "42", "email", "v")
            .unwrap();
        let from_b = b
            .encrypt_subject("customer_id", "42", "email", "v")
            .unwrap();
        assert_eq!(from_a, from_b, "both must use the persisted secret");
        assert_eq!(
            b.decrypt_subject("customer_id", "42", "email", &from_a)
                .unwrap()
                .as_deref(),
            Some("v")
        );
    }

    #[test]
    fn rotation_rewraps_without_changing_ciphertext() {
        let opdb = Arc::new(Mutex::new(OpDb::open_in_memory().unwrap()));
        let old_master = [1u8; 32];
        let old = KeyStore::new(opdb.clone(), MasterKeys::new(old_master, vec![]));
        let ct = old
            .encrypt_subject("customer_id", "42", "email", "a@b.c")
            .unwrap();

        // Rotate: new primary, old kept so existing rows still unwrap.
        let rotated = KeyStore::new(opdb, MasterKeys::new([2u8; 32], vec![old_master]));
        assert_eq!(rotated.rotate().unwrap(), 1);
        assert_eq!(rotated.rotate().unwrap(), 0, "second rotate is a no-op");
        // Ciphertext is unchanged and still decrypts under the rotated store.
        assert_eq!(
            rotated
                .decrypt_subject("customer_id", "42", "email", &ct)
                .unwrap()
                .as_deref(),
            Some("a@b.c")
        );
        assert_eq!(
            rotated
                .encrypt_subject("customer_id", "42", "email", "a@b.c")
                .unwrap(),
            ct,
            "the subject secret (hence the ciphertext) is unchanged by rotation"
        );
    }

    #[test]
    fn verify_masters_present_catches_a_wrong_master() {
        let opdb = Arc::new(Mutex::new(OpDb::open_in_memory().unwrap()));
        let right = KeyStore::new(opdb.clone(), MasterKeys::new([1u8; 32], vec![]));
        right
            .encrypt_subject("customer_id", "42", "email", "a@b.c")
            .unwrap();
        // The correct master (or one keeping it as a previous) passes.
        assert!(right.verify_masters_present().is_ok());
        let with_prev = KeyStore::new(opdb.clone(), MasterKeys::new([2u8; 32], vec![[1u8; 32]]));
        assert!(with_prev.verify_masters_present().is_ok());
        // A store configured with only the wrong master fails fast, rather than serving
        // and silently blanking every subject column at read time.
        let wrong = KeyStore::new(opdb, MasterKeys::new([2u8; 32], vec![]));
        assert!(wrong.verify_masters_present().is_err());
    }

    #[test]
    fn a_stale_ciphertext_under_a_superseded_key_reads_as_none() {
        let ks = store();
        let stale = ks
            .encrypt_subject("customer_id", "42", "email", "a@b.c")
            .unwrap();
        // Erase then recreate the subject: a new key is minted, so the pre-erasure
        // ciphertext no longer decrypts under the current key. That is unrecoverable
        // data, not a misconfiguration, so it reads as absent (Ok(None)), never Err.
        assert!(ks.erase("customer_id", "42").unwrap());
        ks.encrypt_subject("customer_id", "42", "email", "new@b.c")
            .unwrap();
        let back = ks.decrypt_subject("customer_id", "42", "email", &stale);
        assert!(matches!(back, Ok(None)), "got {back:?}");
    }

    #[test]
    fn decode_master_key_requires_32_bytes() {
        let good = URL_SAFE_NO_PAD.encode([9u8; 32]);
        assert_eq!(decode_master_key(&good).unwrap(), [9u8; 32]);
        let short = URL_SAFE_NO_PAD.encode([9u8; 16]);
        assert!(decode_master_key(&short).is_err());
        assert!(decode_master_key("not base64!!!").is_err());
    }
}
