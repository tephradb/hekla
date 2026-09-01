#!/usr/bin/env bash
#
# The mutation experiment: plant a one-line fault, run the tests that are supposed to
# catch it, and require a failure.
#
# This is the part that decides whether the property and model tests are worth having. A
# test that never fails is indistinguishable from one that works, and the suite plants
# what it can from outside the runtime (direct SQLite writes, deleted journal rows,
# scripted responses). What is left are faults that can only be planted by editing
# hekla, so they are planted here.
#
# Not in CI, deliberately: it edits `src/` and puts it back, which is fine for a
# developer and wrong on a shared runner. Run it when a property is added or changed.
#
# The revert is a file copy taken before the edit, never `git checkout`: `src/` normally
# has uncommitted work in it, and reverting to HEAD would take that with it.
#
# A mutant nothing fails against means either the property is vacuous or the change is
# not a bug, and either answer is worth having before the test is committed.
#
# Usage: scripts/mutants.sh [name-substring]

set -uo pipefail

cd "$(dirname "$0")/.." || exit 1

backup=$(mktemp -d)
trap 'rm -rf "$backup"' EXIT

# Seeds proptest persists are part of the repository, and a mutant run makes property
# tests fail on purpose: left alone it files the counterexamples as though they were
# real, in a file whose whole meaning is "these once failed against working code".
if [[ -d proptest-regressions ]]; then
    cp -r proptest-regressions "$backup/seeds"
    trap 'rm -rf proptest-regressions; [[ -d "$backup/seeds" ]] && cp -r "$backup/seeds" proptest-regressions; rm -rf "$backup"' EXIT
fi

filter="${1:-}"
caught=0
survived=0

# Apply one mutant, run its tests, and require them to fail.
#
# The perl program is read from stdin so the patterns are written once, unescaped. It
# must `die` when it does not match: a stale pattern that silently applied nothing would
# report a working test against a mutant that was never planted, which is the one
# failure mode this script exists to rule out.
mutant() {
    local name="$1" file="$2" targets="$3"
    local program
    program=$(cat)

    if [[ -n "$filter" && "$name" != *"$filter"* ]]; then
        return
    fi

    printf '\n=== %s (%s) ===\n' "$name" "$file"
    local saved="$backup/$(basename "$file")"
    cp "$file" "$saved"
    if ! perl -0777 -i -pe "$program" "$file"; then
        echo "NOT PLANTED: the pattern for '$name' no longer matches $file" >&2
        cp "$saved" "$file"
        survived=$((survived + 1))
        return
    fi

    # shellcheck disable=SC2086
    if cargo test $targets >/dev/null 2>&1; then
        echo "SURVIVED: nothing failed. Either the property is vacuous or this is not a bug."
        survived=$((survived + 1))
    else
        echo "caught by: cargo test $targets"
        caught=$((caught + 1))
    fi
    cp "$saved" "$file"
}

# --- the sealed replay ----------------------------------------------------
# `verify_replay` runs a real effect arm against a real store. Blocking the transport is
# not enough: heklang performs a journal miss for real, so an unsealed `invoke` appends
# and an unsealed `erase` shreds a live key. A check that can cause the fault it looks
# for is worse than no check.

mutant sealed-append src/heklang_host.rs "--test verify" <<'PERL'
s/        if self\.sealed \{\n            return Err\(host_error\("a sealed replay tried to append to the log"\)\);\n        \}\n//
    or die "the append seal has moved\n";
PERL

mutant sealed-erase src/heklang_host.rs "--test verify" <<'PERL'
s/        if self\.sealed \{\n            return Err\(host_error\("a sealed replay tried to erase a subject key"\)\);\n        \}\n//
    or die "the erase seal has moved\n";
PERL

# --- erasure --------------------------------------------------------------

mutant erased-column-kept src/read_api.rs "--test model --test tickets" <<'PERL'
s/            None => \{\n                obj\.remove\(name\);\n            \}/            None => {}/
    or die "the read API's absent-column arm has moved\n";
PERL

mutant every-sealed-column-dropped src/verify.rs "--test verify" <<'PERL'
s/        if !readable \&\& let Some\(target\) = row\.as_object_mut\(\) \{/        if let Some(target) = row.as_object_mut() {/
    or die "drop_shredded's readable guard has moved\n";
PERL

mutant unreadable-column-kept src/verify.rs "--test verify" <<'PERL'
s/        if !readable \&\& let Some\(target\) = row\.as_object_mut\(\) \{/        if false \&\& let Some(target) = row.as_object_mut() {/
    or die "drop_shredded's readable guard has moved\n";
PERL

# --- the projector --------------------------------------------------------

mutant row-read-misses src/heklang_host.rs "--test model" <<'PERL'
s/(    fn row\(&self, entity: &str, key: &heklang::Key\) -> Result<Option<heklang::Row>, Error> \{\n)/$1        return Ok(None);\n/
    or die "RowWriter::row has moved\n";
PERL

mutant rows-differ-is-fine src/verify.rs "--test verify" <<'PERL'
s/                if live_row != rebuilt_row \{/                if false {/
    or die "compare_entity's row comparison has moved\n";
PERL

mutant key-order-is-arbitrary src/verify.rs "--test verify" <<'PERL'
s/    keyed\.sort_by\(\|\(left, _\), \(right, _\)\| left\.cmp\(right\)\);\n//
    or die "the key sort in verify::keyed has moved\n";
PERL

mutant checkpoint-goes-backwards src/projector.rs "--lib" <<'PERL'
s/        let from = self\.position\.load\(Ordering::Acquire\);\n        if to < from \{/        let from = self.position.load(Ordering::Acquire);\n        if false {/
    or die "advance_position's guard has moved\n";
PERL

# --- the sweep's coverage counters ----------------------------------------
# `Report::is_clean` is only `violations.is_empty()`, so a sweep that skipped everything
# reads exactly like a clean one. These two are why the counters are asserted as
# equalities against what the shadow delivered rather than as floors.

mutant sweep-skips-everything src/verify.rs "--test model --test tickets" <<'PERL'
s/        if script_hash != unit\.source_hash \{/        if true {/
    or die "sweep_effect's script-hash guard has moved\n";
PERL

mutant sweep-checks-everything src/verify.rs "--test tickets" <<'PERL'
s/        if runtime\.journal_keys\(name, position\)\?\.is_empty\(\) \{/        if false {/
    or die "sweep_effect's empty-journal guard has moved\n";
PERL

mutant replay-reads-the-wrong-event src/verify.rs "--test model --test tickets" <<'PERL'
s/Position::new\(position\.saturating_sub\(1\)\)/Position::new(position)/
    or die "read_at's cursor has moved\n";
PERL

# --- the conversion tables ------------------------------------------------
# Two near-duplicate tables kept apart by which producer they invert. Each mutant has to
# be caught by both a round-trip property and something that stores a row, because a
# table that only one of them exercises has a hole on the other side.

mutant column-form-timestamp src/heklang_host.rs "--lib --test tickets" <<'PERL'
s/        \(FieldKind::Timestamp, serde_json::Value::Number\(micros\)\) => micros/        (FieldKind::Bool, serde_json::Value::Number(micros)) => micros/
    or die "column_form's Timestamp arm has moved\n";
PERL

mutant wire-form-timestamp src/heklang_host.rs "--lib --test model" <<'PERL'
s/        \(FieldKind::Timestamp, serde_json::Value::String\(text\)\) => heklang::value::timestamp\(text\)/        (FieldKind::Bool, serde_json::Value::String(text)) => heklang::value::timestamp(text)/
    or die "wire_form's Timestamp arm has moved\n";
PERL

mutant typed-read-back-is-text src/read_api.rs "--lib --test tickets" <<'PERL'
s/(pub\(crate\) fn typed_from_string\(kind: &FieldKind, text: String\) -> Value \{\n)/$1    if true {\n        return Value::String(text);\n    }\n/
    or die "typed_from_string has moved\n";
PERL

mutant sealed-json-loses-its-quotes src/heklang_host.rs "--lib" <<'PERL'
s/    if matches!\(kind\.base\(\), FieldKind::Json\) \{\n        return json\.to_string\(\);\n    \}\n//
    or die "seal_text's Json arm has moved\n";
PERL

printf '\n%d mutant(s) caught, %d survived\n' "$caught" "$survived"
[[ "$survived" -eq 0 ]]
