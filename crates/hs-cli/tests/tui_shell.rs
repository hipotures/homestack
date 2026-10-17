//! The ssh button (phase 13b, step 4): `s` on a running workspace gives the
//! terminal to ssh, through the desktop's entry when there is one.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod tui_fixture;

use crossterm::event::KeyCode;
use hs_cli::tui::draw::render;
use hs_cli::tui::shell;
use hs_cli::tui::state::{App, Request};
use hs_cli::tui::theme::Palette;
use tui_fixture::{app, position, press};

fn select(app: &mut App, vmid: u32) {
    press(app, KeyCode::Home);
    for _ in 0..20 {
        if app.selected == Some(vmid) {
            return;
        }
        press(app, KeyCode::Down);
    }
    panic!("{vmid} not in the list");
}

#[test]
fn s_on_a_running_workspace_asks_for_ssh() {
    let mut fixture = app();
    select(&mut fixture.app, 201);
    assert_eq!(press(&mut fixture.app, KeyCode::Char('s')), Some(Request::Ssh(201)));
    select(&mut fixture.app, 204);
    assert_eq!(press(&mut fixture.app, KeyCode::Char('s')), Some(Request::Ssh(204)));
}

#[test]
fn a_stopped_workspace_has_no_ssh() {
    let mut fixture = app();
    select(&mut fixture.app, 205);
    assert_eq!(press(&mut fixture.app, KeyCode::Char('s')), None);
    let palette = Palette::named("HomeStack", false).unwrap();
    let (x, y) = position(&render(&fixture.app, 120, 36), "ssh");
    assert_eq!(render(&fixture.app, 120, 36)[(x, y)].fg, palette.muted);
    select(&mut fixture.app, 204);
    let buf = render(&fixture.app, 120, 36);
    let (x, y) = position(&buf, "ssh");
    assert_eq!(buf[(x + 1, y)].fg, palette.foreground);
}

#[test]
fn stopped_lab_gold_opens_its_managed_root_shell_from_the_application() {
    let mut fixture = app();
    fixture.app.lab_gold_vmid = Some(112);
    select(&mut fixture.app, 112);
    assert_eq!(press(&mut fixture.app, KeyCode::Char('s')), Some(Request::Ssh(112)));
    assert_eq!(shell::prepare(&mut fixture.app, 112).as_deref(), Some("root@gold-e2e"));
    assert!(fixture.app.overlay.is_none());
    position(&render(&fixture.app, 120, 36), "ssh root");
}

#[test]
fn running_gold_uses_root_and_its_ssh_alias() {
    let mut fixture = app();
    fixture.app.status.as_mut().unwrap().gold.as_mut().unwrap().status = "running".to_owned();
    select(&mut fixture.app, 112);
    assert_eq!(shell::prepare(&mut fixture.app, 112).as_deref(), Some("root@hs-gold"));
}

#[test]
fn production_workspace_uses_its_name_without_managed_files_or_an_api_detail_read() {
    let mut fixture = app();
    fixture.app.status.as_mut().unwrap().workspaces[0].name = Some("rt".to_owned());
    select(&mut fixture.app, 201);
    assert_eq!(shell::prepare(&mut fixture.app, 201).as_deref(), Some("rt"));
    assert_eq!(fixture.app.details_target(), None);
    assert!(fixture.app.overlay.is_none());
}

#[test]
fn a_custom_ssh_filename_keeps_openssh_host_and_identity_rules() {
    let home = tempfile::tempdir().unwrap();
    let directory = home.path().join("config.d");
    std::fs::create_dir(&directory).unwrap();
    std::fs::write(
        home.path().join("config"),
        format!("Include {}/config.d/*.conf\n", home.path().display()),
    )
    .unwrap();
    std::fs::write(directory.join("rt.conf"), "Host rt\n  HostName 192.0.2.114\n  User developer\n  IdentitiesOnly yes\nMatch originalhost rt\n  IdentityFile /test/rt-security-key\n").unwrap();
    let mut fixture = app();
    fixture.app.status.as_mut().unwrap().workspaces[0].name = Some("rt".to_owned());
    select(&mut fixture.app, 201);
    let target = shell::prepare(&mut fixture.app, 201).unwrap();
    let output = std::process::Command::new("ssh")
        .args(["-G", "-F"])
        .arg(home.path().join("config"))
        .args(["-o", "CanonicalizeHostname=no", &target])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let effective = String::from_utf8(output.stdout).unwrap();
    for expected in [
        "hostname 192.0.2.114",
        "user developer",
        "identitiesonly yes",
        "identityfile /test/rt-security-key",
    ] {
        assert!(effective.lines().any(|line| line == expected), "{effective}");
    }
}

#[test]
fn a_missing_or_invalid_alias_never_becomes_an_ip_login() {
    for name in [
        None,
        Some(""),
        Some("-oProxyCommand=anything"),
        Some("user@192.168.100.114"),
        Some("192.168.100.114"),
    ] {
        let mut fixture = app();
        fixture.app.status.as_mut().unwrap().workspaces[0].name = name.map(str::to_owned);
        select(&mut fixture.app, 201);
        assert_eq!(shell::prepare(&mut fixture.app, 201), None);
        assert_eq!(fixture.app.details_target(), None);
    }
}

#[test]
fn the_log_says_how_ssh_ended() {
    assert_eq!(shell::outcome("vm204", Some(0)), "ssh vm204: ended");
    assert_eq!(shell::outcome("vm204", Some(255)), "ssh vm204: could not connect (255)");
    assert_eq!(shell::outcome("vm204", Some(130)), "ssh vm204: ended with 130");
    assert_eq!(shell::outcome("vm204", None), "ssh vm204: could not run ssh");
}

#[test]
fn ssh_uses_only_public_key_authentication_with_the_alias() {
    let command = shell::command("rt");
    assert_eq!(command.get_program(), "ssh");
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        ["-o", "PreferredAuthentications=publickey", "rt"]
    );
}
