//! Ports of `python/tests/test_touch.py`: the cues played while a security
//! key waits for a touch, and how a failed key login is named (I2).

mod common;

use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::TestResult;
use hs_cli::touch::{self, Event, FailureKind, Player, Sounds, TouchWatch};

#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<Event>>>);

impl Recorder {
    fn player(&self) -> Player {
        let events = Arc::clone(&self.0);
        Arc::new(move |_: &Sounds, event: Event| {
            if let Ok(mut events) = events.lock() {
                events.push(event);
            }
        })
    }

    fn events(&self) -> Vec<Event> {
        self.0.lock().map(|events| events.clone()).unwrap_or_default()
    }

    fn count(&self, event: Event) -> usize {
        self.events().iter().filter(|e| **e == event).count()
    }
}

fn sounds(repeat: f64, initial: f64) -> Sounds {
    Sounds {
        repeat_seconds: repeat,
        initial_delay: initial,
        ..Sounds::default()
    }
}

fn sleep(seconds: f64) {
    std::thread::sleep(Duration::from_secs_f64(seconds));
}

/// Port of `TouchWatchTests::test_wait_cue_repeats_until_finished_then_plays_success`.
#[test]
fn wait_cue_repeats_until_finished_then_plays_success() {
    let rec = Recorder::default();
    let watch = TouchWatch::start("key", sounds(0.05, 0.02), rec.player(), 5.0);
    sleep(0.22);
    watch.finish(true);
    assert!(rec.count(Event::Wait) >= 3, "{:?}", rec.events());
    assert_eq!(rec.events().last(), Some(&Event::Ok));
    assert_eq!(rec.count(Event::Fail), 0);
    let count = rec.events().len();
    sleep(0.12);
    assert_eq!(rec.events().len(), count, "no cues after finish");
}

/// Port of `TouchWatchTests::test_instant_success_is_silent_but_failure_always_cues`.
#[test]
fn instant_success_is_silent_but_failure_always_cues() {
    let rec = Recorder::default();
    TouchWatch::start("key", sounds(8.0, 1.0), rec.player(), 300.0).finish(true);
    assert!(rec.events().is_empty());
    let rec = Recorder::default();
    TouchWatch::start("key", sounds(8.0, 1.0), rec.player(), 300.0).finish(false);
    assert_eq!(rec.events(), [Event::Fail]);
}

/// Port of `TouchWatchTests::test_repeats_continue_until_safety_cap_and_finish_is_idempotent`.
#[test]
fn repeats_continue_until_safety_cap_and_finish_is_idempotent() {
    let rec = Recorder::default();
    let watch = TouchWatch::start("key", sounds(0.03, 0.01), rec.player(), 0.1);
    sleep(0.3);
    let waits = rec.count(Event::Wait);
    assert!((1..=4).contains(&waits), "{waits}");
    let rec2 = Recorder::default();
    let long = TouchWatch::start("key", sounds(0.03, 0.01), rec2.player(), 300.0);
    sleep(0.2);
    long.finish(true);
    assert!(rec2.count(Event::Wait) >= 4, "keeps repeating past the old 30 s window");
    watch.finish(false);
    watch.finish(true);
    assert_eq!(rec.count(Event::Fail), 1);
    assert_eq!(rec.count(Event::Ok), 0);
}

/// Port of `TouchWatchTests::test_context_manager_plays_failure_on_exception`.
#[test]
fn context_manager_plays_failure_on_exception() {
    let rec = Recorder::default();
    let failed: Result<(), &str> = touch::touch_wait("key", &sounds(8.0, 1.0), rec.player(), |_| Err("boom"));
    assert_eq!(failed, Err("boom"));
    assert_eq!(rec.events(), [Event::Fail]);
    let rec = Recorder::default();
    let done: Result<(), &str> = touch::touch_wait("key", &sounds(8.0, 1.0), rec.player(), |_| Ok(()));
    assert_eq!(done, Ok(()));
    assert!(rec.events().is_empty());
}

fn fake_player(dir: &std::path::Path) -> Result<String, std::io::Error> {
    let player = dir.join("fakeplay");
    std::fs::write(&player, "#!/bin/sh\n")?;
    std::fs::set_permissions(&player, std::fs::Permissions::from_mode(0o755))?;
    Ok(player.display().to_string())
}

/// Port of `PlayTests::test_disabled_plays_nothing_and_missing_player_rings_bell`.
#[test]
fn disabled_plays_nothing_and_missing_player_rings_bell() {
    let bells = Mutex::new(0);
    let spawned = Mutex::new(Vec::<Vec<String>>::new());
    let spawn = |argv: &[String]| {
        if let Ok(mut spawned) = spawned.lock() {
            spawned.push(argv.to_vec());
        }
        Ok(())
    };
    let bell = || {
        if let Ok(mut bells) = bells.lock() {
            *bells += 1;
        }
    };
    let disabled = Sounds {
        enabled: false,
        ..Sounds::default()
    };
    touch::play(&disabled, Event::Wait, &spawn, &bell);
    assert_eq!(
        (bells.lock().map_or(9, |b| *b), spawned.lock().map_or(9, |s| s.len())),
        (0, 0)
    );
    let mut missing = Sounds::default();
    missing.files.insert("wait".to_owned(), "/nonexistent.oga".to_owned());
    touch::play(&missing, Event::Wait, &spawn, &bell);
    assert_eq!(
        (bells.lock().map_or(9, |b| *b), spawned.lock().map_or(9, |s| s.len())),
        (1, 0)
    );
}

/// Port of `PlayTests::test_command_uses_configured_player_and_file`.
#[test]
fn command_uses_configured_player_and_file() -> TestResult {
    let dir = tempfile::tempdir()?;
    let sound = dir.path().join("tick.oga");
    std::fs::write(&sound, b"x")?;
    let player = fake_player(dir.path())?;
    let mut cfg = Sounds {
        player: vec![player.clone()],
        ..Sounds::default()
    };
    cfg.files = [("wait".to_owned(), sound.display().to_string())].into_iter().collect();
    assert_eq!(
        cfg.command(Event::Wait),
        Some(vec![player.clone(), sound.display().to_string()])
    );
    assert_eq!(cfg.command(Event::Ok), None);
    let calls = Mutex::new(Vec::<Vec<String>>::new());
    touch::play(
        &cfg,
        Event::Wait,
        &|argv: &[String]| {
            if let Ok(mut calls) = calls.lock() {
                calls.push(argv.to_vec());
            }
            Ok(())
        },
        &|| {},
    );
    assert_eq!(
        calls.lock().map(|c| c.clone()).unwrap_or_default(),
        [vec![player, sound.display().to_string()]]
    );
    Ok(())
}

/// Port of `PlayTests::test_spawn_failure_falls_back_to_bell`.
#[test]
fn spawn_failure_falls_back_to_bell() -> TestResult {
    let dir = tempfile::tempdir()?;
    let sound = dir.path().join("a.oga");
    std::fs::write(&sound, b"x")?;
    let mut cfg = Sounds {
        player: vec![fake_player(dir.path())?],
        ..Sounds::default()
    };
    cfg.files.insert("wait".to_owned(), sound.display().to_string());
    let bells = Mutex::new(0);
    touch::play(
        &cfg,
        Event::Wait,
        &|_: &[String]| Err(std::io::Error::other("no player")),
        &|| {
            if let Ok(mut bells) = bells.lock() {
                *bells += 1;
            }
        },
    );
    assert_eq!(bells.lock().map_or(0, |b| *b), 1);
    Ok(())
}

/// Port of `ClassifyTests::test_failure_classes_follow_measured_ssh_behaviour`.
#[test]
fn failure_classes_follow_measured_ssh_behaviour() {
    let c = touch::classify_ssh_failure;
    assert_eq!(
        c(
            255,
            0.4,
            "sign_and_send_pubkey: signing failed for ED25519-SK \"k\": device not found"
        ),
        FailureKind::Absent
    );
    assert_eq!(
        c(
            255,
            29.4,
            "signing failed for ED25519-SK \"k\": invalid format\nPermission denied (publickey)."
        ),
        FailureKind::NotConfirmed
    );
    assert_eq!(c(255, 0.6, "Permission denied (publickey)."), FailureKind::Denied);
    let mixed =
        "signing failed for ED25519-SK \"a\": device not found\nsigning failed for ED25519-SK \"b\": invalid format";
    assert_eq!(c(255, 35.8, mixed), FailureKind::NotConfirmed);
    assert_eq!(c(255, 0.5, mixed), FailureKind::Absent);
    assert_eq!(c(255, 1.0, "Connection refused"), FailureKind::Failed);
    for kind in [
        FailureKind::Absent,
        FailureKind::NotConfirmed,
        FailureKind::Denied,
        FailureKind::Failed,
    ] {
        assert!(!touch::message(kind).is_empty());
    }
}

/// Port of `SoundsConfigTests::test_section_is_optional_validated_and_round_trips`:
/// `[sounds]` of `client.toml`, validated by `hs-config` and read here.
#[test]
fn section_is_optional_validated_and_round_trips() -> TestResult {
    assert_eq!(Sounds::from_table(None), Sounds::default());
    let base = "version = 1\n[workspace_ssh]\nuser = \"u\"\nidentity_files = [\"~/.ssh/k\"]\n\
identities_only = true\nlog_level = \"FATAL\"\n";
    let client = hs_config::client::ClientConfig::parse(&format!(
        "{base}[sounds]\nenabled = true\nplayer = \"pw-play\"\nwait = \"bell.oga\"\nrepeat_seconds = 10\n"
    ))?;
    let custom = Sounds::from_table(client.sounds.as_ref());
    assert_eq!(custom.player, ["pw-play"]);
    assert_eq!(custom.files.get("wait").map(String::as_str), Some("bell.oga"));
    assert_eq!(custom.files.get("ok"), Sounds::default().files.get("ok"));
    assert!((custom.repeat_seconds - 10.0).abs() < f64::EPSILON);
    let again = hs_config::client::ClientConfig::parse(&client.to_toml()?)?;
    assert_eq!(Sounds::from_table(again.sounds.as_ref()), custom);
    for bad in [
        "enabled = \"yes\"",
        "player = 5",
        "wait = 1",
        "repeat_seconds = 0",
        "repeat_seconds = true",
        "volume = 3",
    ] {
        assert!(
            hs_config::client::ClientConfig::parse(&format!("{base}[sounds]\n{bad}\n")).is_err(),
            "{bad}"
        );
    }
    assert!(hs_config::client::ClientConfig::parse(&format!("{base}sounds = \"text\"\n")).is_err());
    Ok(())
}

/// Port of `TouchCueTests::test_herdr_registration_plays_failure_cue_on_nonzero_exit`.
#[test]
fn herdr_registration_plays_failure_cue_on_nonzero_exit() -> TestResult {
    let loud = sounds(8.0, 5.0);
    let rec = Recorder::default();
    let result = hs_cli::setup::herdr::register_desktop_with(&["false".to_owned()], &loud, rec.player())?;
    assert_eq!(result.code, 1);
    assert_eq!(rec.events(), [Event::Fail]);
    let rec = Recorder::default();
    hs_cli::setup::herdr::register_desktop_with(&["true".to_owned()], &loud, rec.player())?;
    assert!(rec.events().is_empty());
    Ok(())
}
