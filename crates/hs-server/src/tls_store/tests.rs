//! Retained critical-recovery safeguard. Only generated throwaway TLS material.
#![allow(clippy::unwrap_used)]

use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind};

use super::*;

thread_local! {
    static HOOK: Cell<Option<(usize, bool)>> = const { Cell::new(None) };
    static STEPS: Cell<usize> = const { Cell::new(0) };
}
pub(super) fn before(_: &str) -> io::Result<()> {
    if HOOK.get() == Some((0, true)) {
        return Err(io::Error::other("injected filesystem failure"));
    }
    Ok(())
}
pub(super) fn after(_: &str) {
    STEPS.set(STEPS.get() + 1);
    if let Some((remaining, fail)) = HOOK.get() {
        if remaining == 0 {
            HOOK.set(None);
            panic!("simulated termination after filesystem transition");
        }
        HOOK.set(Some((remaining - 1, fail)));
    }
}
fn pair() -> TlsPair {
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec!["localhost".to_owned()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    TlsPair::checked(cert.pem(), Secret::new(key.serialize_pem())).unwrap()
}
fn restart(state: &Path, old: Option<&TlsPair>, new: &TlsPair) {
    HOOK.set(None);
    let startup = bootstrap(state, None);
    let dir = state.join("tls");
    if startup.is_err() {
        assert!(old.is_none());
        assert!(selected(&dir).unwrap().is_none());
        return;
    }
    if let Some(generation) = selected(&dir).unwrap() {
        let selected = read_pair(&dir.join(&generation.name)).unwrap();
        assert!(selected.cert == new.cert || old.is_some_and(|pair| pair.cert == selected.cert));
        assert_eq!(
            generation.path("cert.pem").parent(),
            generation.path("key.pem").parent()
        );
        for file in [generation.path("key.pem"), generation.path("cert.pem")] {
            assert_eq!(fs::metadata(file).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(
            fs::metadata(dir.join(generation.name)).unwrap().permissions().mode() & 0o777,
            0o700
        );
    } else {
        assert!(old.is_none(), "an old valid selection must survive");
        // With no durable selection Caddy's launcher waits: it cannot serve a
        // partial generation or infer activation from file existence.
        assert!(!dir.join("current").exists());
    }
}

#[test]
fn interruption_and_io_failure_at_each_rotation_publication_keep_a_matching_pair() {
    let old = pair();
    let new = pair();
    for previous in [false, true] {
        for fail in [false, true] {
            // Two private-directory durability steps plus 13 generation/selector steps.
            for transition in 0..15 {
                let state = tempfile::tempdir().unwrap();
                bootstrap(state.path(), previous.then_some(&old)).unwrap();
                HOOK.set(Some((transition, fail)));
                let _ = catch_unwind(AssertUnwindSafe(|| rotate(&state.path().join("tls"), &new)));
                restart(state.path(), previous.then_some(&old), &new);
            }
        }
    }
}
#[test]
fn interruption_during_rollback_and_restart_after_success_keep_complete_generations() {
    let old = pair();
    let new = pair();
    for transition in 0..3 {
        let state = tempfile::tempdir().unwrap();
        bootstrap(state.path(), Some(&old)).unwrap();
        let rotation = rotate(&state.path().join("tls"), &new).unwrap();
        HOOK.set(Some((transition, false)));
        let _ = catch_unwind(AssertUnwindSafe(|| {
            select(&state.path().join("tls"), rotation.previous.as_ref())
        }));
        restart(state.path(), Some(&old), &new);
    }
    let state = tempfile::tempdir().unwrap();
    bootstrap(state.path(), Some(&old)).unwrap();
    rotate(&state.path().join("tls"), &new).unwrap();
    // A stale environment pair must not undo a successful rotation.
    bootstrap(state.path(), Some(&old)).unwrap();
    let active = selected(&state.path().join("tls")).unwrap().unwrap();
    assert_eq!(
        read_pair(&state.path().join("tls").join(active.name)).unwrap().cert,
        new.cert
    );
}
#[test]
fn mismatches_and_unsafe_paths_fail_closed() {
    let old = pair();
    let new = pair();
    assert!(TlsPair::checked(old.cert.clone(), new.key.clone()).is_err());
    let state = tempfile::tempdir().unwrap();
    bootstrap(state.path(), Some(&old)).unwrap();
    let dir = state.path().join("tls");
    let generation = selected(&dir).unwrap().unwrap();
    fs::remove_file(generation.path("key.pem")).unwrap();
    symlink("cert.pem", generation.path("key.pem")).unwrap();
    assert!(bootstrap(state.path(), Some(&new)).is_err());
    fs::remove_file(dir.join("current")).unwrap();
    symlink("../../outside", dir.join("current")).unwrap();
    assert!(bootstrap(state.path(), Some(&new)).is_err());
    let other = tempfile::tempdir().unwrap();
    symlink(state.path().join("tls"), other.path().join("tls")).unwrap();
    assert!(bootstrap(other.path(), Some(&new)).is_err());
}

#[test]
fn initial_bootstrap_interruption_includes_directory_creation_and_durability() {
    let pair = pair();
    let baseline = tempfile::tempdir().unwrap();
    let before = STEPS.get();
    bootstrap(baseline.path(), Some(&pair)).unwrap();
    let count = STEPS.get() - before;
    for transition in 0..count {
        let state = tempfile::tempdir().unwrap();
        HOOK.set(Some((transition, false)));
        let _ = catch_unwind(AssertUnwindSafe(|| bootstrap(state.path(), Some(&pair))));
        restart(state.path(), None, &pair);
    }
}

fn legacy(state: &Path, pair: &TlsPair) {
    let dir = state.join("tls");
    private_directory(&dir).unwrap();
    for (name, bytes) in [
        ("cert.pem", pair.cert.as_bytes()),
        ("key.pem", pair.key.expose().as_bytes()),
    ] {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join(name))
            .unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
    }
    sync(&dir).unwrap();
}

#[test]
fn legacy_pair_publication_recovers_after_every_interruption_and_write_failure() {
    let old = pair();
    let environment = pair();
    let baseline = tempfile::tempdir().unwrap();
    legacy(baseline.path(), &old);
    let before = STEPS.get();
    bootstrap(baseline.path(), Some(&environment)).unwrap();
    let count = STEPS.get() - before;
    for fail in [false, true] {
        for transition in 0..count {
            let state = tempfile::tempdir().unwrap();
            legacy(state.path(), &old);
            HOOK.set(Some((transition, fail)));
            let _ = catch_unwind(AssertUnwindSafe(|| bootstrap(state.path(), Some(&environment))));
            HOOK.set(None);
            // Startup uses disk only. Complete original files remain a safe
            // source until selection is durable; orphan staging is ignored.
            bootstrap(state.path(), None).unwrap();
            let dir = state.path().join("tls");
            let active = selected(&dir).unwrap().unwrap();
            assert_eq!(read_generation(&dir.join(active.name)).unwrap().cert, old.cert);
            assert_eq!(read_private(&dir.join("cert.pem")).unwrap(), old.cert);
            assert_eq!(read_private(&dir.join("key.pem")).unwrap(), old.key.expose());
            restart(state.path(), Some(&old), &environment);
        }
    }
}

#[test]
fn invalid_or_unsafe_legacy_pair_is_never_replaced_by_bootstrap_input() {
    let old = pair();
    let other = pair();
    for damage in ["missing-key", "mismatched-key", "symlink-key", "public-key"] {
        let state = tempfile::tempdir().unwrap();
        legacy(state.path(), &old);
        let key = state.path().join("tls/key.pem");
        match damage {
            "missing-key" => fs::remove_file(&key).unwrap(),
            "mismatched-key" => fs::write(&key, other.key.expose()).unwrap(),
            "symlink-key" => {
                fs::remove_file(&key).unwrap();
                symlink("cert.pem", &key).unwrap();
            }
            "public-key" => fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap(),
            _ => unreachable!(),
        }
        assert!(bootstrap(state.path(), Some(&other)).is_err(), "{damage}");
        assert!(selected(&state.path().join("tls")).unwrap().is_none());
        assert_eq!(read_private(&state.path().join("tls/cert.pem")).unwrap(), old.cert);
    }
}
