//! Master key material read from the environment. This lives in its own
//! integration binary, and in a single test function, because it mutates process
//! environment variables: another test reading the environment on a second thread
//! while these run would be a data race.

use std::env;
use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use kiln::crypto::{KeyStore, MasterKeys, master_keys_from_env};
use kiln::opdb::OpDb;

const PRIMARY: [u8; 32] = [1u8; 32];
const PREVIOUS_A: [u8; 32] = [2u8; 32];
const PREVIOUS_B: [u8; 32] = [3u8; 32];

fn set(name: &str, value: &str) {
    unsafe { env::set_var(name, value) };
}

fn clear(name: &str) {
    unsafe { env::remove_var(name) };
}

/// The rendered error from a read that must fail. `MasterKeys` is not
/// `Debug` (it holds key material), so `expect_err` is unavailable here.
fn env_error(label: &str) -> String {
    match master_keys_from_env() {
        Ok(_) => panic!("expected {label} to fail"),
        Err(err) => format!("{err:#}"),
    }
}

#[test]
fn master_keys_from_env_reads_the_primary_and_previous_keys() {
    clear("KILN_MASTER_KEY");
    clear("KILN_MASTER_KEY_PREVIOUS");

    // No primary: a project that uses no subjects boots with no key at all.
    assert!(
        master_keys_from_env().unwrap().is_none(),
        "an unset KILN_MASTER_KEY must not be an error"
    );

    // A stray previous list without a primary is still no key, not a failure.
    set(
        "KILN_MASTER_KEY_PREVIOUS",
        &URL_SAFE_NO_PAD.encode(PREVIOUS_A),
    );
    assert!(master_keys_from_env().unwrap().is_none());

    // The real rotation shape: mixed alphabets, padding, whitespace and an empty
    // entry, all of which a hand-edited env file produces.
    set("KILN_MASTER_KEY", &URL_SAFE_NO_PAD.encode(PRIMARY));
    let previous_list = format!(
        "{} , ,{}",
        URL_SAFE_NO_PAD.encode(PREVIOUS_A),
        STANDARD.encode(PREVIOUS_B)
    );
    set("KILN_MASTER_KEY_PREVIOUS", &previous_list);
    let masters = master_keys_from_env()
        .unwrap()
        .expect("a set KILN_MASTER_KEY yields a key set");

    // Prove each previous key really survived the parse, rather than just counting
    // them: a key silently dropped mid-rotation makes every not-yet-rewrapped
    // subject unreadable, which is indistinguishable from data loss.
    for (index, previous) in [PREVIOUS_A, PREVIOUS_B].into_iter().enumerate() {
        let db = Arc::new(Mutex::new(OpDb::open_in_memory().unwrap()));
        let subject = index.to_string();
        let under_previous = KeyStore::new(db.clone(), MasterKeys::new(previous, vec![]));
        let ciphertext = under_previous
            .encrypt_subject("customer_id", &subject, "email", "alice@example.com")
            .unwrap();

        let under_env = KeyStore::new(db, masters.clone());
        let plaintext = under_env
            .decrypt_subject("customer_id", &subject, "email", &ciphertext)
            .unwrap_or_else(|err| {
                panic!("previous key {index} was dropped by the env parse: {err:#}")
            });
        assert_eq!(
            plaintext.as_deref(),
            Some("alice@example.com"),
            "previous key {index} does not unwrap the row it wrapped"
        );
    }

    // New writes wrap under the primary, so a set holding only the primary can read
    // them back: this pins which of the three keys the env made primary.
    let db = Arc::new(Mutex::new(OpDb::open_in_memory().unwrap()));
    let ciphertext = KeyStore::new(db.clone(), masters)
        .encrypt_subject("customer_id", "9", "email", "bob@example.com")
        .unwrap();
    let primary_only = KeyStore::new(db, MasterKeys::new(PRIMARY, vec![]));
    assert_eq!(
        primary_only
            .decrypt_subject("customer_id", "9", "email", &ciphertext)
            .unwrap()
            .as_deref(),
        Some("bob@example.com"),
        "new writes must wrap under KILN_MASTER_KEY, not a previous key"
    );

    // A bad primary fails loudly, naming the variable the operator has to fix.
    set("KILN_MASTER_KEY", "nope");
    let message = env_error("an undecodable primary");
    assert!(
        message.contains("KILN_MASTER_KEY"),
        "the error must name the variable, got: {message}"
    );

    // A key of the wrong length is base64 but not a master key.
    set("KILN_MASTER_KEY", &URL_SAFE_NO_PAD.encode([1u8; 16]));
    let message = env_error("a 16-byte primary");
    assert!(
        message.contains("KILN_MASTER_KEY"),
        "the error must name the variable, got: {message}"
    );

    // A bad entry in the previous list is fatal too: booting without it would leave
    // rows wrapped under it silently unreadable.
    set("KILN_MASTER_KEY", &URL_SAFE_NO_PAD.encode(PRIMARY));
    set(
        "KILN_MASTER_KEY_PREVIOUS",
        &format!("{},nope", URL_SAFE_NO_PAD.encode(PREVIOUS_A)),
    );
    let message = env_error("an undecodable previous key");
    assert!(
        message.contains("KILN_MASTER_KEY_PREVIOUS"),
        "the error must name the variable, got: {message}"
    );

    clear("KILN_MASTER_KEY");
    clear("KILN_MASTER_KEY_PREVIOUS");
}
