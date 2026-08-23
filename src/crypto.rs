//! Subject-scoped deterministic encryption and the per-subject key store.
//!
//! A field marked `subject = "sibling"` is encrypted under a key scoped to that
//! subject's identity `(subject_field, subject_value)`. Encryption is deterministic
//! (AES-SIV, RFC 5297): the same plaintext under the same key and field yields the
//! same ciphertext, so it works as a tag the index can match on, a payload value,
//! and a read-model column all at once. Erasing a subject is deleting its key row,
//! which makes every value encrypted under it unmatchable and unreadable across the
//! log and every read model simultaneously.
//!
//! Key material never leaves this module in the clear on disk: each per-subject
//! secret is a random AES-SIV key, wrapped with AES-256-GCM under a master key held
//! only in memory (from `KILN_MASTER_KEY`). The wrapping master is recorded per row
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

use crate::opdb::{OpDb, RewrapUpdate};

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
const GLOBAL_SUBJECT_FIELD: &str = "_kiln_global";
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
/// master key is needed: this is a plain row delete, so the `kiln erase` CLI can call
/// it without one.
pub fn erase_subject(
    opdb: &OpDb,
    subject_field: &str,
    subject_value: &str,
) -> anyhow::Result<bool> {
    if subject_field == GLOBAL_SUBJECT_FIELD {
        anyhow::bail!("the global uniqueness secret cannot be erased");
    }
    opdb.delete_subject_key(subject_field, subject_value)
}

/// A stable id for a master key: the full hex SHA-256 of its bytes. The full digest
/// (not a truncation) so two distinct masters cannot collide onto one id and shadow
/// each other in the key set.
fn fingerprint(key: &[u8; MASTER_KEY_LEN]) -> String {
    crate::hash::sha256_hex(key)
}

/// Read the master keys from the environment: `KILN_MASTER_KEY` (the primary, a
/// base64 32-byte key) and, optionally, `KILN_MASTER_KEY_PREVIOUS` (a comma-separated
/// list of prior keys still needed to unwrap rows not yet rotated). Returns `None`
/// when `KILN_MASTER_KEY` is unset, so a project that uses no subjects needs no key.
pub fn master_keys_from_env() -> anyhow::Result<Option<MasterKeys>> {
    let Ok(primary) = env::var("KILN_MASTER_KEY") else {
        return Ok(None);
    };
    let primary = decode_master_key(&primary).context("KILN_MASTER_KEY")?;
    let previous = match env::var("KILN_MASTER_KEY_PREVIOUS") {
        Ok(list) => list
            .split(',')
            .filter(|part| !part.trim().is_empty())
            .map(|part| decode_master_key(part).context("KILN_MASTER_KEY_PREVIOUS"))
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

    /// Encrypt `plaintext` under the subject `(subject_field, subject_value)`,
    /// creating the subject's key on first use. Returns the base64url ciphertext
    /// used as the tag value, the payload value, and the read-model column.
    pub fn encrypt_subject(
        &self,
        subject_field: &str,
        subject_value: &str,
        field: &str,
        plaintext: &str,
    ) -> anyhow::Result<String> {
        let secret = self.get_or_create_secret(subject_field, subject_value)?;
        encrypt_with(&secret, field, plaintext.as_bytes())
    }

    /// Encrypt `plaintext` under the global uniqueness key (created on first use).
    /// The resulting tag survives subject erasure, so a global uniqueness check
    /// still fires after the subject's own data is shredded.
    pub fn encrypt_global(&self, field: &str, plaintext: &str) -> anyhow::Result<String> {
        let secret = self.get_or_create_secret(GLOBAL_SUBJECT_FIELD, GLOBAL_SUBJECT_VALUE)?;
        encrypt_with(&secret, field, plaintext.as_bytes())
    }

    /// Encrypt a filter value under an existing subject key, for query lowering.
    /// Returns `Ok(None)` when the subject has no key (never seen, or erased): a query
    /// is a read path and must not create or resurrect key material, so the caller
    /// makes the clause match nothing rather than minting a key.
    pub fn encrypt_subject_existing(
        &self,
        subject_field: &str,
        subject_value: &str,
        field: &str,
        plaintext: &str,
    ) -> anyhow::Result<Option<String>> {
        self.load_secret(subject_field, subject_value)?
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
        subject_field: &str,
        subject_value: &str,
        field: &str,
        ciphertext: &str,
    ) -> anyhow::Result<Option<String>> {
        let Some(secret) = self.load_secret(subject_field, subject_value)? else {
            return Ok(None);
        };
        Ok(plaintext_under(&secret, field, ciphertext))
    }

    /// Erase a subject by deleting its key. Refuses to delete the reserved global
    /// secret. Returns whether a key was removed.
    pub fn erase(&self, subject_field: &str, subject_value: &str) -> anyhow::Result<bool> {
        erase_subject(&self.lock(), subject_field, subject_value)
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
    /// wrong or rotated-away `KILN_MASTER_KEY` fails fast at boot with a clear message
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
                "stored subject data was wrapped under master key(s) not configured now: {missing}. Set KILN_MASTER_KEY (and KILN_MASTER_KEY_PREVIOUS, comma-separated, for masters mid-rotation) to the master(s) that wrapped this data. Losing a master is permanent, unrecoverable loss of every subject it wrapped"
            );
        }
        Ok(())
    }

    /// Load and unwrap a subject secret, or `None` if the subject has no key row.
    fn load_secret(
        &self,
        subject_field: &str,
        subject_value: &str,
    ) -> anyhow::Result<Option<Zeroizing<Vec<u8>>>> {
        let Some((wrapped, master_id)) =
            self.lock().get_subject_key(subject_field, subject_value)?
        else {
            return Ok(None);
        };
        let master = self.master_for(&master_id, subject_field)?;
        Ok(Some(unwrap_key(master, &wrapped)?))
    }

    /// The configured master that wrapped a stored subject key, or an error naming
    /// the master that is missing: without it the subject cannot be read at all.
    fn master_for(
        &self,
        master_id: &str,
        subject_field: &str,
    ) -> anyhow::Result<&[u8; MASTER_KEY_LEN]> {
        self.masters.get(master_id).ok_or_else(|| {
            anyhow!(
                "cannot unwrap subject `{subject_field}`: master key `{master_id}` is not configured (was KILN_MASTER_KEY rotated away without keeping the previous key?)"
            )
        })
    }

    /// Get the subject's secret, creating it on first use. Concurrency-safe: a
    /// creating thread that loses the insert race re-reads and returns the secret
    /// that actually persisted, never its own discarded one (which would produce
    /// permanently unrecoverable ciphertext).
    fn get_or_create_secret(
        &self,
        subject_field: &str,
        subject_value: &str,
    ) -> anyhow::Result<Zeroizing<Vec<u8>>> {
        if let Some(secret) = self.load_secret(subject_field, subject_value)? {
            return Ok(secret);
        }
        let fresh = random_secret()?;
        let (primary_id, primary) = self.masters.primary();
        let wrapped = wrap_key(primary, &fresh)?;
        // Insert-if-absent and re-read atomically under one lock, so the persisted row
        // (this thread's or a racing thread's) is always the one used, and a racing
        // erase cannot leave us with nothing.
        let (persisted, master_id) = self.lock().get_or_insert_subject_key(
            subject_field,
            subject_value,
            &wrapped,
            primary_id,
        )?;
        let master = self.masters.get(&master_id).ok_or_else(|| {
            anyhow!("cannot unwrap subject `{subject_field}`: master key `{master_id}` is not configured")
        })?;
        unwrap_key(master, &persisted)
    }

    fn lock(&self) -> MutexGuard<'_, OpDb> {
        self.opdb.lock().unwrap_or_else(PoisonError::into_inner)
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

/// Unwrapped subject secrets cached by `(subject_field, subject_value)`; `None`
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
    /// Decrypt a subject-scoped ciphertext, reusing a cached secret. `Ok(None)` when
    /// the value is unreadable under the current key: the subject's key is gone (erased
    /// or never created), or the ciphertext will not decrypt under the present key (a
    /// stale row under a superseded key, or tampering). `Err` is reserved for a key that
    /// cannot be obtained at all (a missing master or a corrupt key wrapping).
    pub fn decrypt(
        &self,
        subject_field: &str,
        subject_value: &str,
        field: &str,
        ciphertext: &str,
    ) -> anyhow::Result<Option<String>> {
        let cache_key = (subject_field.to_owned(), subject_value.to_owned());
        let secret = {
            let mut cache = self.secrets.borrow_mut();
            match cache.get(&cache_key) {
                Some(cached) => cached.clone(),
                None => {
                    let loaded = self.keystore.load_secret(subject_field, subject_value)?;
                    cache.insert(cache_key, loaded.clone());
                    loaded
                }
            }
        };
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
/// `nonce || ciphertext`.
fn wrap_key(master: &[u8; MASTER_KEY_LEN], secret: &[u8]) -> anyhow::Result<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(master).map_err(|_| anyhow!("invalid master key"))?;
    let mut nonce = [0u8; WRAP_NONCE_LEN];
    getrandom::fill(&mut nonce).context("gathering entropy for key wrapping")?;
    let ciphertext = cipher
        .encrypt(&nonce.into(), secret)
        .map_err(|_| anyhow!("wrapping a subject key failed"))?;
    let mut out = Vec::with_capacity(WRAP_NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Unwrap a subject secret produced by [`wrap_key`].
fn unwrap_key(master: &[u8; MASTER_KEY_LEN], wrapped: &[u8]) -> anyhow::Result<Zeroizing<Vec<u8>>> {
    if wrapped.len() <= WRAP_NONCE_LEN {
        anyhow::bail!("wrapped key is too short");
    }
    let (nonce, ciphertext) = wrapped.split_at(WRAP_NONCE_LEN);
    let cipher = Aes256Gcm::new_from_slice(master).map_err(|_| anyhow!("invalid master key"))?;
    let nonce: [u8; WRAP_NONCE_LEN] = nonce.try_into().expect("checked length");
    let secret = cipher
        .decrypt(&nonce.into(), ciphertext)
        .map_err(|_| anyhow!("unwrapping a subject key failed (wrong master key?)"))?;
    Ok(Zeroizing::new(secret))
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
            ks.decrypt_subject(GLOBAL_SUBJECT_FIELD, GLOBAL_SUBJECT_VALUE, "email", &global)
                .unwrap()
                .as_deref(),
            Some("a@b.c")
        );
    }

    #[test]
    fn the_global_secret_cannot_be_erased() {
        let ks = store();
        ks.encrypt_global("email", "x").unwrap();
        assert!(
            ks.erase(GLOBAL_SUBJECT_FIELD, GLOBAL_SUBJECT_VALUE)
                .is_err()
        );
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
