//! Ports of `python/tests/test_fido.py`: choosing the security key a
//! workspace SSH master may see (I2).

mod common;

use std::sync::{Arc, Mutex};

use common::{FakeSystem, TestResult, strings};
use hs_cli::fido::{self, Device};
use hs_cli::master::Master;
use hs_cli::system::Output;
use hs_cli::touch::{self, FailureKind};

const LISTING: &str = "/dev/hidraw8: vendor=0x1050, product=0x0407 (Yubico YubiKey OTP+FIDO+CCID)\n\
/dev/hidraw9: vendor=0x1209, product=0x53c1 (SatoshiLabs TREZOR)\n\
/dev/hidraw11: vendor=0x1050, product=0x0407 (Yubico YubiKey OTP+FIDO+CCID)\n";

fn devices() -> Vec<Device> {
    fido::parse_listing(LISTING)
}

fn of_vendor(vendor: &str) -> Vec<Device> {
    devices().into_iter().filter(|d| d.vendor == vendor).collect()
}

fn paths(devices: &[Device]) -> Vec<&str> {
    devices.iter().map(|d| d.path.as_str()).collect()
}

/// Port of `SelectionTests::test_listing_is_parsed_with_normalized_ids`.
#[test]
fn listing_is_parsed_with_normalized_ids() {
    let found = fido::parse_listing(&format!(
        "{LISTING}/dev/hidraw3: vendor=0x20a0, product=0x42b2 (Nitrokey)\n"
    ));
    let first: Vec<(&str, &str, &str)> = found[..3]
        .iter()
        .map(|d| (d.path.as_str(), d.vendor.as_str(), d.product.as_str()))
        .collect();
    assert_eq!(
        first,
        [
            ("/dev/hidraw8", "1050", "0407"),
            ("/dev/hidraw9", "1209", "53c1"),
            ("/dev/hidraw11", "1050", "0407")
        ]
    );
    assert_eq!(found[3].vendor, "20a0");
    assert!(fido::parse_listing("").is_empty());
}

/// Port of `SelectionTests::test_preference_order_wins_and_all_devices_of_a_kind_are_kept`.
#[test]
fn preference_order_wins_and_all_devices_of_a_kind_are_kept() -> TestResult {
    let all = devices();
    let (entry, found) = fido::select(&strings(&["yubikey", "trezor"]), &all)?;
    assert_eq!(
        (entry.as_str(), paths(&found)),
        ("yubikey", vec!["/dev/hidraw8", "/dev/hidraw11"])
    );
    let (entry, found) = fido::select(&strings(&["trezor", "yubikey"]), &all)?;
    assert_eq!((entry.as_str(), paths(&found)), ("trezor", vec!["/dev/hidraw9"]));
    let (entry, _) = fido::select(&strings(&["yubikey", "trezor"]), &of_vendor("1209"))?;
    assert_eq!(entry, "trezor");
    assert_eq!(fido::select(&strings(&["1050:0407"]), &all)?.0, "1050:0407");
    assert_eq!(paths(&fido::select(&strings(&["1209"]), &all)?.1), ["/dev/hidraw9"]);
    Ok(())
}

/// Port of `SelectionTests::test_nothing_connected_gives_a_plain_message`.
#[test]
fn nothing_connected_gives_a_plain_message() {
    let error = fido::select(&strings(&["yubikey", "trezor"]), &[])
        .err()
        .map(|e| e.to_string());
    assert!(
        error
            .as_deref()
            .is_some_and(|e| e.starts_with("No configured security key is connected (YubiKey, Trezor)")),
        "{error:?}"
    );
}

/// Port of `SelectionTests::test_entries_are_validated_and_normalized`.
#[test]
fn entries_are_validated_and_normalized() -> TestResult {
    assert_eq!(fido::validate_entry(" YubiKey ")?, "yubikey");
    assert_eq!(fido::validate_entry("1050:0407")?, "1050:0407");
    assert_eq!(fido::validate_entry("1209")?, "1209");
    for bad in ["usb", "12345", "1050:", ""] {
        assert!(fido::validate_entry(bad).is_err(), "{bad:?}");
    }
    Ok(())
}

/// Port of `SelectionTests::test_sandbox_exposes_only_the_selected_devices`.
#[test]
fn sandbox_exposes_only_the_selected_devices() -> TestResult {
    let system = FakeSystem::new().which(|_| Some("/usr/bin/bwrap".to_owned()));
    let prefix = fido::sandbox_prefix(&system, &of_vendor("1050"))?;
    assert_eq!(
        prefix[..6],
        strings(&["/usr/bin/bwrap", "--bind", "/", "/", "--dev", "/dev"])
    );
    let binds: Vec<&str> = prefix
        .windows(2)
        .filter(|pair| pair[0] == "--dev-bind")
        .map(|pair| pair[1].as_str())
        .collect();
    assert_eq!(binds, ["/dev/hidraw8", "/dev/hidraw11"]);
    assert!(!prefix.iter().any(|part| part == "/dev/hidraw9"));
    let missing = FakeSystem::new().which(|_| None);
    let error = fido::sandbox_prefix(&missing, &devices()).err().map(|e| e.to_string());
    assert!(error.is_some_and(|e| e.contains("bwrap")));
    Ok(())
}

/// Port of `SelectionTests::test_advice_only_when_several_kinds_are_attached`.
#[test]
fn advice_only_when_several_kinds_are_attached() {
    assert!(fido::advice(&devices()).is_some_and(|text| text.contains("Trezor, YubiKey")));
    assert_eq!(fido::advice(&of_vendor("1050")), None);
    assert_eq!(fido::advice(&[]), None);
}

/// Port of `SelectionTests::test_listing_uses_fido2_token`.
#[test]
fn listing_uses_fido2_token() -> TestResult {
    let system = FakeSystem::new().run(|_| Ok(Output::stdout(LISTING)));
    assert_eq!(fido::list_devices(&system)?.len(), 3);
    assert_eq!(system.runs(), [strings(&["fido2-token", "-L"])]);
    let missing = FakeSystem::new().which(|_| None);
    let error = fido::list_devices(&missing).err().map(|e| e.to_string());
    assert!(error.is_some_and(|e| e.contains("fido2-token")));
    Ok(())
}

/// The master opened with `system`; the master's argv and the error.
fn enter(master: &mut Master, system: &FakeSystem) -> (Vec<Vec<String>>, Option<String>) {
    let home = tempfile::tempdir().ok();
    let home = home
        .as_ref()
        .map_or(std::path::Path::new("/nonexistent"), |dir| dir.path());
    let error = master.open(system, home).err().map(|e| e.to_string());
    master.forget();
    (system.masters(), error)
}

fn sandboxed(devices: Vec<Device>, listing: &'static str) -> FakeSystem {
    FakeSystem::new()
        .which(|_| Some("/usr/bin/bwrap".to_owned()))
        .run(move |argv| {
            Ok(if argv.first().is_some_and(|a| a == "fido2-token") {
                Output::stdout(&devices_listing(&devices))
            } else {
                Output::stdout(listing)
            })
        })
}

fn devices_listing(devices: &[Device]) -> String {
    devices
        .iter()
        .map(|d| format!("{}: vendor=0x{}, product=0x{} (x)\n", d.path, d.vendor, d.product))
        .collect()
}

/// Port of `MasterTests::test_master_runs_in_a_sandbox_that_shows_only_the_preferred_key`.
#[test]
fn master_runs_in_a_sandbox_that_shows_only_the_preferred_key() {
    let mut master = Master::new("user@192.0.2.1");
    master.fido_devices = strings(&["yubikey", "trezor"]);
    let (calls, error) = enter(&mut master, &sandboxed(devices(), ""));
    assert_eq!(error, None);
    let argv = &calls[0];
    assert_eq!(argv[0], "/usr/bin/bwrap");
    assert!(argv.iter().any(|a| a == "/dev/hidraw8"));
    assert!(!argv.iter().any(|a| a == "/dev/hidraw9"));
    let ssh = argv.iter().position(|a| a == "ssh");
    let bind = argv.iter().position(|a| a == "--dev-bind");
    assert!(bind < ssh && ssh.is_some());
    assert_eq!(ssh.and_then(|i| argv.get(i + 1)).map(String::as_str), Some("-F"));
}

/// Port of `MasterTests::test_sandboxed_master_does_not_hand_signing_to_an_agent`.
#[test]
fn sandboxed_master_does_not_hand_signing_to_an_agent() {
    for (options, expected) in [(vec![], 1), (strings(&["-o", "IdentityAgent=/tmp/own.sock"]), 0)] {
        let mut master = Master::new("user@192.0.2.1");
        master.options = options;
        master.fido_devices = strings(&["yubikey"]);
        let (calls, error) = enter(&mut master, &sandboxed(devices(), ""));
        assert_eq!(error, None);
        assert_eq!(calls[0].iter().filter(|a| *a == "IdentityAgent=none").count(), expected);
    }
}

/// Port of `MasterTests::test_unsandboxed_master_keeps_the_agent`.
#[test]
fn unsandboxed_master_keeps_the_agent() {
    let mut master = Master::new("user@192.0.2.1");
    let (calls, error) = enter(&mut master, &FakeSystem::new());
    assert_eq!(error, None);
    assert!(!calls[0].iter().any(|a| a == "IdentityAgent=none"));
}

/// Port of `MasterTests::test_sandboxed_master_reads_the_users_config_or_none`.
#[test]
fn sandboxed_master_reads_the_users_config_or_none() -> TestResult {
    let home = tempfile::tempdir()?;
    std::fs::create_dir(home.path().join(".ssh"))?;
    std::fs::write(home.path().join(".ssh/config"), "Host x\n")?;
    assert_eq!(
        Master::user_ssh_config(home.path()),
        home.path().join(".ssh/config").display().to_string()
    );
    std::fs::remove_file(home.path().join(".ssh/config"))?;
    assert_eq!(Master::user_ssh_config(home.path()), "/dev/null");

    let mut master = Master::new("user@192.0.2.1");
    master.options = strings(&["-F", "/tmp/own"]);
    master.fido_devices = strings(&["yubikey"]);
    let (calls, _) = enter(&mut master, &sandboxed(devices(), ""));
    assert_eq!(calls[0].iter().filter(|a| *a == "-F").count(), 1);
    assert!(calls[0].iter().any(|a| a == "/tmp/own"));
    Ok(())
}

/// Port of `MasterTests::test_no_connected_key_stops_before_ssh_with_a_plain_message`.
#[test]
fn no_connected_key_stops_before_ssh_with_a_plain_message() {
    let mut master = Master::new("user@192.0.2.1");
    master.fido_devices = strings(&["yubikey"]);
    let (calls, error) = enter(&mut master, &sandboxed(vec![], ""));
    assert!(calls.is_empty());
    assert!(error.is_some_and(|e| e.contains("No configured security key is connected")));
}

/// Port of `MasterTests::test_connected_key_unknown_to_the_host_stops_before_ssh_asks_the_device`.
#[test]
fn connected_key_unknown_to_the_host_stops_before_ssh_asks_the_device() {
    let listing = "host gpu\nidentitiesonly yes\nidentityfile ~/.ssh/id_ed25519_sk_yubikey1\n\
identityfile ~/.ssh/id_ed25519_sk_yubikey2\nidentityfile ~/.ssh/id_ed25519_sk_yubikey3\n";
    let mut master = Master::new("user@192.0.2.7");
    master.fido_devices = strings(&["yubikey", "trezor"]);
    master.name = "gpu".to_owned();
    master.address = Some("192.0.2.7".to_owned());
    let (calls, error) = enter(&mut master, &sandboxed(of_vendor("1209"), listing));
    assert!(calls.is_empty());
    let error = error.unwrap_or_default();
    assert_eq!(
        error.lines().collect::<Vec<_>>(),
        [
            "gpu (192.0.2.7) has no Trezor key configured.",
            "Only YubiKey keys are set up there.",
            "Connect a YubiKey or add a Trezor key to the host."
        ]
    );
    assert!(!error.contains("user@"));
}

/// Port of `MasterTests::test_master_offers_only_the_keys_of_the_connected_kind`.
#[test]
fn master_offers_only_the_keys_of_the_connected_kind() {
    let options = strings(&[
        "-o",
        "IdentitiesOnly=yes",
        "-i",
        "/k/id_ed25519_sk_yubikey1",
        "-i",
        "/k/id_ecdsa_sk_trezor_pve",
        "-i",
        "/k/id_work",
    ]);
    let listing =
        "identityfile /k/id_ed25519_sk_yubikey1\nidentityfile /k/id_ecdsa_sk_trezor_pve\nidentityfile /k/id_work\n";
    for (present, wanted, dropped) in [
        ("1209", "/k/id_ecdsa_sk_trezor_pve", "yubikey1"),
        ("1050", "/k/id_ed25519_sk_yubikey1", "trezor"),
    ] {
        let mut master = Master::new("user@192.0.2.7");
        master.options = options.clone();
        master.fido_devices = strings(&["yubikey", "trezor"]);
        let (calls, error) = enter(&mut master, &sandboxed(of_vendor(present), listing));
        assert_eq!(error, None);
        let argv = &calls[0];
        assert!(argv.iter().any(|a| a == wanted));
        assert!(argv.iter().any(|a| a == "/k/id_work"));
        assert!(!argv.iter().any(|a| a.contains(dropped)));
    }
}

/// Port of `MasterTests::test_identity_filter_keeps_other_options_and_unknown_kinds`.
#[test]
fn identity_filter_keeps_other_options_and_unknown_kinds() {
    let options = strings(&[
        "-o",
        "LogLevel=ERROR",
        "-i",
        "/k/trezor",
        "-i",
        "/k/yubikey1",
        "-i",
        "/k/other",
        "-i",
    ]);
    assert_eq!(
        fido::filter_identity_options("trezor", &options),
        strings(&["-o", "LogLevel=ERROR", "-i", "/k/trezor", "-i", "/k/other", "-i"])
    );
    assert_eq!(fido::filter_identity_options("1050:0407", &options), options);
}

/// Port of `MasterTests::test_host_key_check_passes_when_the_kind_is_listed_or_names_are_unknown`.
#[test]
fn host_key_check_passes_when_the_kind_is_listed_or_names_are_unknown() {
    let listed = strings(&["~/.ssh/id_ed25519_sk_yubikey1", "~/.ssh/id_ecdsa_sk_trezor_pve"]);
    assert_eq!(fido::missing_key_message("trezor", &listed, "pve1", None), None);
    assert_eq!(fido::missing_key_message("yubikey", &listed, "pve1", None), None);
    assert_eq!(
        fido::missing_key_message("trezor", &strings(&["~/.ssh/id_work"]), "x", None),
        None
    );
    assert_eq!(fido::missing_key_message("trezor", &[], "x", None), None);
    assert_eq!(fido::missing_key_message("1050:0407", &listed[..1], "x", None), None);
    assert!(
        fido::missing_key_message("trezor", &listed[..1], "gpu", None).is_some_and(|m| m.contains("Connect a YubiKey"))
    );
}

/// Port of `MasterTests::test_failures_are_reported_as_short_messages_not_raw_ssh_output`.
#[test]
fn failures_are_reported_as_short_messages_not_raw_ssh_output() {
    let failing = |stderr: &'static str, clock: &[f64]| {
        FakeSystem::new()
            .master(move |_| Ok(Output::failed(255, stderr)))
            .clock(clock)
    };
    let mut master = Master::new("user@192.0.2.1");
    let (_, error) = enter(
        &mut master,
        &failing(
            "sign_and_send_pubkey: signing failed for ECDSA-SK \"k\": device not found\nuser@192.0.2.1: Permission denied (publickey).\n",
            &[0.0, 0.1],
        ),
    );
    assert_eq!(error.as_deref(), Some(touch::message(FailureKind::Absent)));
    let mut master = Master::new("user@192.0.2.1");
    let (_, error) = enter(
        &mut master,
        &failing("signing failed for ED25519-SK \"k\": invalid format\n", &[0.0, 35.8]),
    );
    assert_eq!(error.as_deref(), Some(touch::message(FailureKind::NotConfirmed)));
    let mut master = Master::new("user@192.0.2.1");
    let (_, error) = enter(
        &mut master,
        &failing(
            "ssh: connect to host 192.0.2.1 port 22: Connection refused\n",
            &[0.0, 0.2],
        ),
    );
    let error = error.unwrap_or_default();
    assert!(error.starts_with(touch::message(FailureKind::Failed)), "{error}");
    assert!(error.contains("Connection refused"));
}

/// Port of `WiringTests::test_master_connection_plays_failure_cue_and_stays_silent_when_instant`
/// (`test_touch.py`).
#[test]
fn master_connection_plays_failure_cue_and_stays_silent_when_instant() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let player = {
        let events = Arc::clone(&events);
        Arc::new(move |_: &touch::Sounds, event: touch::Event| {
            if let Ok(mut events) = events.lock() {
                events.push(event);
            }
        })
    };
    let loud = touch::Sounds {
        initial_delay: 5.0,
        ..touch::Sounds::default()
    };
    let mut master = Master::new("user@192.0.2.1");
    master.sounds = loud.clone();
    master.player = player.clone();
    let (_, error) = enter(&mut master, &FakeSystem::new().master(|_| Ok(Output::failed(255, ""))));
    assert!(error.is_some());
    assert_eq!(
        events.lock().map(|e| e.clone()).unwrap_or_default(),
        [touch::Event::Fail]
    );

    if let Ok(mut events) = events.lock() {
        events.clear();
    }
    let mut master = Master::new("user@192.0.2.1");
    master.sounds = loud;
    master.player = player;
    let (_, error) = enter(&mut master, &FakeSystem::new());
    assert_eq!(error, None);
    assert!(events.lock().map(|e| e.is_empty()).unwrap_or(false));
}

/// Port of `WiringTests::test_default_workspace_ssh_is_silent` (`test_touch.py`).
#[test]
fn default_workspace_ssh_is_silent() {
    assert!(!Master::new("user@192.0.2.1").sounds.enabled);
}
