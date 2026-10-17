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
    let home = tempfile::tempdir().unwrap();
    assert_eq!(
        shell::prepare(&mut fixture.app, home.path(), 112).as_deref(),
        Some("root@gold-e2e")
    );
    assert!(fixture.app.overlay.is_none());
    position(&render(&fixture.app, 120, 36), "ssh root");
}

#[test]
fn running_gold_uses_root_with_its_local_keys_instead_of_the_workspace_account() {
    let mut fixture = app();
    fixture.app.status.as_mut().unwrap().gold.as_mut().unwrap().status = "running".to_owned();
    select(&mut fixture.app, 112);
    let home = tempfile::tempdir().unwrap();
    let directory = home.path().join(".ssh/config.d/homestack");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("vm112-gold.conf"), "Host gold vm112\n").unwrap();
    assert_eq!(
        shell::prepare(&mut fixture.app, home.path(), 112).as_deref(),
        Some("root@vm112")
    );
}

#[test]
fn the_desktops_entry_is_used_when_it_exists() {
    let home = tempfile::tempdir().unwrap();
    let directory = home.path().join(".ssh/config.d/homestack");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("vm101010-dev.conf"), "Host dev vm101010\n").unwrap();
    assert_eq!(
        shell::target(home.path(), 101_010, Some("ssh user@192.168.101.10")).as_deref(),
        Some("vm101010")
    );
    // Another workspace's entry does not count.
    assert_eq!(
        shell::target(home.path(), 10_101, Some("ssh user@192.168.100.101")).as_deref(),
        Some("user@192.168.100.101")
    );
}

#[test]
fn without_an_entry_or_an_address_there_is_nothing_to_run() {
    let home = tempfile::tempdir().unwrap();
    assert_eq!(shell::target(home.path(), 204, None), None);
    assert_eq!(shell::target(home.path(), 204, Some("ssh ")), None);
}

#[test]
fn the_log_says_how_ssh_ended() {
    assert_eq!(shell::outcome("vm204", Some(0)), "ssh vm204: ended");
    assert_eq!(shell::outcome("vm204", Some(255)), "ssh vm204: could not connect (255)");
    assert_eq!(shell::outcome("vm204", Some(130)), "ssh vm204: ended with 130");
    assert_eq!(shell::outcome("vm204", None), "ssh vm204: could not run ssh");
}
