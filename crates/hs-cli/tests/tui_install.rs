//! Install and uninstall in the TUI (phase 13b step 3; mockups 02–08, 18):
//! the steps access › check › settings › review › install, ssh's questions
//! in a box, the progress, the end.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod tui_fixture;

use crossterm::event::KeyCode;
use hs_cli::askpass::Kind;
use hs_cli::tui::draw::render;
use hs_cli::tui::install::{AskView, Event, Found, Mark, Method, Mode, Setting, Stage};
use hs_cli::tui::installing::Command;
use hs_cli::tui::state::{App, MENU, Request};
use hs_cli::wizard::Answer;
use tui_fixture::{app, press, screen};

fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        press(app, KeyCode::Char(c));
    }
}

fn stage(app: &App) -> Stage {
    app.install.as_ref().expect("the install screen").stage
}

fn found() -> Found {
    Found {
        root: "ssh root@pve2 · PVE 9.2.21".to_owned(),
        check: vec![
            (
                Mark::Ok,
                "root".to_owned(),
                "ssh root@pve2 answers, Proxmox VE 9.2.21".to_owned(),
            ),
            (Mark::Ok, "nodes".to_owned(), "cluster of 3 nodes".to_owned()),
            (Mark::Add, "Proxmox".to_owned(), "31 changes to make".to_owned()),
            (
                Mark::Note,
                "domain".to_owned(),
                "no certificate yet: requested during the install".to_owned(),
            ),
        ],
        settings: vec![
            Setting {
                title: "Workspace disks".to_owned(),
                note: "Where workspace VM disks are made.".to_owned(),
                multiple: true,
                choices: vec![
                    ("local-lvm".to_owned(), "lvmthin  pve2, pve3".to_owned()),
                    ("local-zfs".to_owned(), "zfspool  pve1".to_owned()),
                ],
                picked: vec![0],
            },
            Setting {
                title: "Gold VM".to_owned(),
                note: "The VM workspaces are cloned from.".to_owned(),
                multiple: false,
                choices: vec![
                    ("112".to_owned(), "hs-gold (pve2)".to_owned()),
                    ("101".to_owned(), "old-gold (pve1)".to_owned()),
                ],
                picked: vec![0],
            },
        ],
        domain: Some("hs.example.net".to_owned()),
    }
}

/// The screen with root reached and the check shown.
fn checked(mode: Mode) -> tui_fixture::Fixture {
    let mut fixture = app();
    fixture.app.open_install(mode, false);
    type_text(&mut fixture.app, "root@pve2");
    assert!(matches!(
        press(&mut fixture.app, KeyCode::Enter),
        Some(Request::Install(Command::Connect { .. }))
    ));
    fixture.app.install_event(Event::Connected(found()));
    fixture
}

#[test]
fn without_client_toml_the_install_screen_opens_at_access() {
    let mut fixture = app();
    fixture.app.open_install(Mode::Install, true);
    let text = screen(&render(&fixture.app, 80, 24));
    for part in [
        "HomeStack install",
        "┌access┐",
        "check",
        "settings",
        "review",
        "Method",
        "‹ SSH ›",
        "Host",
        "step 1 of 5",
    ] {
        assert!(text.contains(part), "{part:?} missing:\n{text}");
    }
    // The first start has nothing to go back to: Esc quits.
    assert_eq!(press(&mut fixture.app, KeyCode::Esc), Some(Request::Quit));
}

#[test]
fn the_menu_opens_install_and_uninstall_and_esc_goes_back() {
    assert!(MENU.contains(&"Install or reconfigure"), "{MENU:?}");
    assert!(MENU.contains(&"Uninstall"), "{MENU:?}");
    let mut fixture = app();
    press(&mut fixture.app, KeyCode::Char('m'));
    let at = MENU.iter().position(|item| *item == "Uninstall").unwrap();
    for _ in 0..at {
        press(&mut fixture.app, KeyCode::Down);
    }
    press(&mut fixture.app, KeyCode::Enter);
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("HomeStack uninstall"), "{text}");
    assert!(
        text.contains("step 1 of 4"),
        "access › check › review › remove:\n{text}"
    );
    assert_eq!(press(&mut fixture.app, KeyCode::Esc), None);
    assert!(fixture.app.install.is_none(), "esc goes back to the main screen");
}

#[test]
fn connect_sends_the_method_and_the_host_and_herdr_lists_its_tabs() {
    let mut fixture = app();
    fixture.app.open_install(Mode::Install, false);
    type_text(&mut fixture.app, "root@pve2");
    match press(&mut fixture.app, KeyCode::Enter) {
        Some(Request::Install(Command::Connect { method, target, .. })) => {
            assert_eq!((method, target.as_str()), (Method::Ssh, "root@pve2"));
        }
        other => panic!("{other:?}"),
    }
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("Connecting"), "{text}");

    let mut fixture = app();
    fixture.app.open_install(Mode::Install, false);
    // Tab to the method, then → for Herdr: its tabs are asked for.
    press(&mut fixture.app, KeyCode::Tab);
    assert_eq!(press(&mut fixture.app, KeyCode::Right), Some(Request::HerdrTabs));
    fixture
        .app
        .set_herdr_tabs(vec![("w3:p2".to_owned(), "root@pve2".to_owned())]);
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("‹ Herdr pane ›") && text.contains("w3:p2"), "{text}");
    match press(&mut fixture.app, KeyCode::Enter) {
        Some(Request::Install(Command::Connect { method, target, .. })) => {
            assert_eq!((method, target.as_str()), (Method::Herdr, "w3:p2"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_connection_that_fails_says_why_and_stays_at_access() {
    let mut fixture = app();
    fixture.app.open_install(Mode::Install, false);
    type_text(&mut fixture.app, "root@pve2");
    press(&mut fixture.app, KeyCode::Enter);
    fixture
        .app
        .install_event(Event::Failed("The server rejected the key.".to_owned()));
    assert_eq!(stage(&fixture.app), Stage::Access);
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("The server rejected the key."), "{text}");
}

#[test]
fn ssh_asks_for_a_touch_a_pin_and_a_host_key_in_a_box() {
    let mut fixture = app();
    fixture.app.open_install(Mode::Install, false);
    fixture.app.ask(AskView::new(
        1,
        Kind::Touch,
        "Confirm user presence for key ECDSA-SK SHA256:abc",
    ));
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("Touch your security key now."), "{text}");
    assert!(text.contains("SHA256:abc"), "{text}");
    assert!(text.contains("cancel Esc"), "{text}");
    fixture.app.ask_ended(1);
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(!text.contains("Touch your security key"), "{text}");

    fixture
        .app
        .ask(AskView::new(2, Kind::Secret, "Enter PIN for ECDSA-SK key /k: "));
    type_text(&mut fixture.app, "1234");
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("••••") && !text.contains("1234"), "{text}");
    assert_eq!(
        press(&mut fixture.app, KeyCode::Enter),
        Some(Request::Answer(Some("1234".to_owned())))
    );
    assert!(fixture.app.asking.is_none());

    fixture.app.ask(AskView::new(
        3,
        Kind::HostKey,
        "The authenticity of host 'pve2' can't be established.\nAre you sure you want to continue connecting (yes/no/[fingerprint])? ",
    ));
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("Trust"), "{text}");
    assert_eq!(
        press(&mut fixture.app, KeyCode::Esc),
        Some(Request::Answer(Some("no".to_owned())))
    );
    fixture
        .app
        .ask(AskView::new(4, Kind::HostKey, "continue connecting (yes/no)? "));
    assert_eq!(
        press(&mut fixture.app, KeyCode::Enter),
        Some(Request::Answer(Some("yes".to_owned())))
    );
}

#[test]
fn the_check_shows_what_is_there_and_what_the_install_adds() {
    let mut fixture = checked(Mode::Install);
    assert_eq!(stage(&fixture.app), Stage::Settings, "connecting opens the settings");
    // One Esc back shows the check.
    press(&mut fixture.app, KeyCode::Esc);
    assert_eq!(stage(&fixture.app), Stage::Check);
    let text = screen(&render(&fixture.app, 120, 36));
    for part in [
        "✓access",
        "┌check┐",
        "ssh root@pve2 · PVE 9.2.21",
        "✓ root",
        "· Proxmox",
        "! domain",
        "Nothing was changed.",
        "step 2 of 5",
    ] {
        assert!(text.contains(part), "{part:?} missing:\n{text}");
    }
}

#[test]
fn the_settings_are_chosen_in_a_box_and_go_to_the_review() {
    let mut fixture = checked(Mode::Install);
    assert_eq!(stage(&fixture.app), Stage::Settings);
    let text = screen(&render(&fixture.app, 120, 36));
    for part in [
        "Workspace disks",
        "1 chosen",
        "Gold VM",
        "112",
        "Server domain",
        "hs.example.net",
        "[x] local-lvm",
        "[ ] local-zfs",
        "step 3 of 5",
    ] {
        assert!(text.contains(part), "{part:?} missing:\n{text}");
    }
    // Into the chooser, down to local-zfs, tick it.
    press(&mut fixture.app, KeyCode::Tab);
    press(&mut fixture.app, KeyCode::Down);
    press(&mut fixture.app, KeyCode::Char(' '));
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("[x] local-zfs") && text.contains("2 chosen"), "{text}");
    // Back to the list, down to Gold, choose 101.
    press(&mut fixture.app, KeyCode::Tab);
    press(&mut fixture.app, KeyCode::Down);
    press(&mut fixture.app, KeyCode::Tab);
    press(&mut fixture.app, KeyCode::Down);
    press(&mut fixture.app, KeyCode::Char(' '));
    match press(&mut fixture.app, KeyCode::Enter) {
        Some(Request::Install(Command::Review { answers, domain })) => {
            assert_eq!(answers, [Answer::Picked(vec![0, 1]), Answer::Picked(vec![1])]);
            assert_eq!(domain.as_deref(), Some("hs.example.net"));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(stage(&fixture.app), Stage::Review);
}

#[test]
fn the_review_lists_every_change_and_problems_keep_install_grey() {
    let mut fixture = checked(Mode::Install);
    press(&mut fixture.app, KeyCode::Enter);
    fixture.app.install_event(Event::Reviewed {
        sections: vec![
            (
                "Your choices".to_owned(),
                vec!["Workspace disks:     local-lvm".to_owned()],
            ),
            (
                "Servers".to_owned(),
                vec!["api1.hs.example.net  192.0.2.97  pve1  CT 2097  local-lvm".to_owned()],
            ),
        ],
        problems: vec!["api2.hs.example.net does not resolve on this desktop".to_owned()],
    });
    let text = screen(&render(&fixture.app, 120, 36));
    for part in [
        "┌review┐",
        "Problems",
        "api2.hs.example.net does not resolve",
        "Your choices",
        "CT 2097",
        "step 4 of 5",
    ] {
        assert!(text.contains(part), "{part:?} missing:\n{text}");
    }
    assert_eq!(
        press(&mut fixture.app, KeyCode::Enter),
        None,
        "Install is grey while there are problems"
    );

    fixture.app.install_event(Event::Reviewed {
        sections: vec![(
            "Your choices".to_owned(),
            vec!["Workspace disks:     local-lvm".to_owned()],
        )],
        problems: Vec::new(),
    });
    assert_eq!(
        press(&mut fixture.app, KeyCode::Enter),
        Some(Request::Install(Command::Apply))
    );
    assert_eq!(stage(&fixture.app), Stage::Run);
}

#[test]
fn the_run_shows_each_step_and_the_end_opens_the_main_screen() {
    let mut fixture = checked(Mode::Install);
    press(&mut fixture.app, KeyCode::Enter);
    fixture.app.install_event(Event::Reviewed {
        sections: Vec::new(),
        problems: Vec::new(),
    });
    press(&mut fixture.app, KeyCode::Enter);
    fixture
        .app
        .install_event(Event::Line("- create user homestack@pve".to_owned()));
    fixture
        .app
        .install_event(Event::Line("- create token homestack@pve!server".to_owned()));
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("✓ create user homestack@pve"), "{text}");
    assert!(text.contains("◐ create token homestack@pve!server"), "{text}");
    assert!(text.contains("step 5 of 5"), "{text}");
    // It cannot be stopped midway.
    assert_eq!(press(&mut fixture.app, KeyCode::Esc), None);
    assert_eq!(stage(&fixture.app), Stage::Run);

    fixture.app.install_event(Event::Done {
        ok: true,
        lines: vec!["~/.config/homestack/client.toml written".to_owned()],
    });
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("HomeStack is installed."), "{text}");
    assert!(text.contains("✓install"), "{text}");
    assert_eq!(press(&mut fixture.app, KeyCode::Enter), Some(Request::Installed));
    assert!(fixture.app.install.is_none());
}

#[test]
fn a_failed_run_says_so_and_goes_back_to_the_review() {
    let mut fixture = checked(Mode::Install);
    press(&mut fixture.app, KeyCode::Enter);
    fixture.app.install_event(Event::Reviewed {
        sections: Vec::new(),
        problems: Vec::new(),
    });
    press(&mut fixture.app, KeyCode::Enter);
    fixture.app.install_event(Event::Done {
        ok: false,
        lines: vec!["failed: create user homestack@pve: exit 2; run this again to finish".to_owned()],
    });
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("The install did not finish."), "{text}");
    assert!(text.contains("run this again to finish"), "{text}");
}

#[test]
fn the_uninstall_goes_from_check_straight_to_its_review_with_cancel_focused() {
    let mut fixture = checked(Mode::Uninstall);
    assert_eq!(
        press(&mut fixture.app, KeyCode::Enter),
        Some(Request::Install(Command::Review {
            answers: Vec::new(),
            domain: None
        }))
    );
    fixture.app.install_event(Event::Reviewed {
        sections: vec![("Removed from Proxmox".to_owned(), vec!["user homestack@pve".to_owned()])],
        problems: Vec::new(),
    });
    let text = screen(&render(&fixture.app, 120, 36));
    for part in [
        "HomeStack uninstall",
        "┌review┐",
        "Removed from Proxmox",
        "▶cancel",
        "step 3 of 4",
    ] {
        assert!(text.contains(part), "{part:?} missing:\n{text}");
    }
    // Enter on the focused cancel removes nothing.
    assert_eq!(
        press(&mut fixture.app, KeyCode::Enter),
        Some(Request::Install(Command::Cancel))
    );
    assert!(fixture.app.install.is_none());
}

#[test]
fn the_log_panel_shows_under_the_install_at_120_by_36_only() {
    let mut fixture = app();
    fixture.app.open_install(Mode::Install, false);
    fixture.app.log("10:41:02", "install screen opened");
    assert!(screen(&render(&fixture.app, 120, 36)).contains("⁴log"));
    assert!(!screen(&render(&fixture.app, 80, 24)).contains("⁴log"));
}

#[test]
fn the_access_that_connected_is_kept_for_the_next_install_screen() {
    let mut fixture = app();
    fixture.app.open_install(Mode::Install, false);
    type_text(&mut fixture.app, "root@pve1.example.net");
    press(&mut fixture.app, KeyCode::Enter);
    fixture.app.install_event(Event::Connected(found()));
    fixture.app.install = None;

    fixture.app.open_install(Mode::Install, false);
    let install = fixture.app.install.as_ref().unwrap();
    assert_eq!(install.host, "root@pve1.example.net");
    let saved = std::fs::read_to_string(fixture.app.preferences.path.clone()).unwrap();
    assert!(saved.contains("install_host = \"root@pve1.example.net\""), "{saved}");
}

#[test]
fn the_host_is_edited_where_the_cursor_is() {
    let mut fixture = app();
    fixture.app.open_install(Mode::Install, false);
    type_text(&mut fixture.app, "pve1.example.net");
    press(&mut fixture.app, KeyCode::Home);
    type_text(&mut fixture.app, "root@");
    press(&mut fixture.app, KeyCode::End);
    press(&mut fixture.app, KeyCode::Left);
    press(&mut fixture.app, KeyCode::Left);
    press(&mut fixture.app, KeyCode::Left);
    press(&mut fixture.app, KeyCode::Backspace);
    type_text(&mut fixture.app, ".");
    press(&mut fixture.app, KeyCode::Delete);
    type_text(&mut fixture.app, "o");
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("[ root@pve1.example.o▏et"), "{text}");
    match press(&mut fixture.app, KeyCode::Enter) {
        Some(Request::Install(Command::Connect { target, .. })) => assert_eq!(target, "root@pve1.example.oet"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn esc_goes_back_one_step_and_leaves_only_from_access() {
    let mut fixture = checked(Mode::Install);
    press(&mut fixture.app, KeyCode::Enter);
    fixture.app.install_event(Event::Reviewed {
        sections: Vec::new(),
        problems: Vec::new(),
    });
    assert_eq!(press(&mut fixture.app, KeyCode::Esc), None);
    assert_eq!(stage(&fixture.app), Stage::Settings);
    assert_eq!(press(&mut fixture.app, KeyCode::Esc), None);
    assert_eq!(stage(&fixture.app), Stage::Check);
    assert_eq!(
        press(&mut fixture.app, KeyCode::Esc),
        Some(Request::Install(Command::Cancel))
    );
    assert_eq!(stage(&fixture.app), Stage::Access);
    assert_eq!(press(&mut fixture.app, KeyCode::Esc), None);
    assert!(fixture.app.install.is_none(), "esc at access leaves the screen");
}

#[test]
fn up_goes_to_the_method_and_left_or_right_switches_it_to_herdr() {
    let mut fixture = app();
    fixture.app.open_install(Mode::Install, false);
    press(&mut fixture.app, KeyCode::Up);
    assert_eq!(press(&mut fixture.app, KeyCode::Right), Some(Request::HerdrTabs));
    assert_eq!(fixture.app.install.as_ref().unwrap().method, Method::Herdr);
    assert_eq!(press(&mut fixture.app, KeyCode::Left), None);
    assert_eq!(fixture.app.install.as_ref().unwrap().method, Method::Ssh);
    // Down is the host again: ← → move the cursor there.
    press(&mut fixture.app, KeyCode::Down);
    type_text(&mut fixture.app, "ab");
    press(&mut fixture.app, KeyCode::Left);
    type_text(&mut fixture.app, "x");
    assert_eq!(fixture.app.install.as_ref().unwrap().host, "axb");
}

#[test]
fn the_red_letters_do_what_their_buttons_do() {
    let mut fixture = checked(Mode::Install);
    assert_eq!(stage(&fixture.app), Stage::Settings, "connecting opens the settings");
    assert!(matches!(
        press(&mut fixture.app, KeyCode::Char('r')),
        Some(Request::Install(Command::Review { .. }))
    ));
    fixture.app.install_event(Event::Reviewed {
        sections: Vec::new(),
        problems: Vec::new(),
    });
    assert_eq!(
        press(&mut fixture.app, KeyCode::Char('i')),
        Some(Request::Install(Command::Apply))
    );
    fixture.app.install_event(Event::Done {
        ok: true,
        lines: Vec::new(),
    });
    assert_eq!(press(&mut fixture.app, KeyCode::Char('o')), Some(Request::Installed));
}

#[test]
fn esc_on_the_main_screen_quits_and_m_opens_the_menu() {
    let mut fixture = app();
    assert_eq!(press(&mut fixture.app, KeyCode::Esc), Some(Request::Quit));
    assert_eq!(press(&mut fixture.app, KeyCode::Char('m')), None);
    assert!(fixture.app.overlay.is_some(), "m opens the menu");
    assert_eq!(press(&mut fixture.app, KeyCode::Esc), None, "esc closes the menu first");
    assert!(fixture.app.overlay.is_none());
}

#[test]
fn what_a_setting_is_for_is_in_the_help_not_in_its_box() {
    let mut fixture = checked(Mode::Install);
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(!text.contains("Where workspace VM disks are made."), "{text}");
    press(&mut fixture.app, KeyCode::Char('?'));
    let text = screen(&render(&fixture.app, 120, 36));
    for part in [
        "help: the settings",
        "Where workspace VM disks are made.",
        "The VM workspaces are cloned from.",
        "apiN.<domain>",
    ] {
        assert!(text.contains(part), "{part:?} missing:\n{text}");
    }
    press(&mut fixture.app, KeyCode::Esc);
    assert!(!screen(&render(&fixture.app, 120, 36)).contains("help: the settings"));
    assert_eq!(stage(&fixture.app), Stage::Settings, "esc closed only the help");
}

#[test]
fn esc_after_a_failed_run_goes_back_to_the_review() {
    let mut fixture = checked(Mode::Install);
    press(&mut fixture.app, KeyCode::Enter);
    fixture.app.install_event(Event::Reviewed {
        sections: Vec::new(),
        problems: Vec::new(),
    });
    press(&mut fixture.app, KeyCode::Enter);
    fixture.app.install_event(Event::Done {
        ok: false,
        lines: vec!["Deploying the servers failed".to_owned()],
    });
    assert_eq!(press(&mut fixture.app, KeyCode::Esc), None);
    assert_eq!(stage(&fixture.app), Stage::Review);
    assert_eq!(
        press(&mut fixture.app, KeyCode::Enter),
        Some(Request::Install(Command::Apply))
    );
}

#[test]
fn a_click_chooses_a_setting_and_ticks_a_choice() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let click = |column, row| MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };
    let mut fixture = checked(Mode::Install);
    let buf = render(&fixture.app, 120, 36);
    let (x, y) = tui_fixture::position(&buf, "Gold VM");
    fixture.app.mouse(click(x, y));
    assert_eq!(fixture.app.install.as_ref().unwrap().row, 1);
    let buf = render(&fixture.app, 120, 36);
    let (x, y) = tui_fixture::position(&buf, "old-gold");
    fixture.app.mouse(click(x, y));
    assert_eq!(fixture.app.install.as_ref().unwrap().settings[1].picked, [1]);
}

#[test]
fn esc_cancels_the_touch_of_the_security_key() {
    let mut fixture = app();
    fixture.app.open_install(Mode::Uninstall, false);
    type_text(&mut fixture.app, "root@pve2");
    press(&mut fixture.app, KeyCode::Enter);
    fixture.app.ask(AskView::new(
        1,
        Kind::Touch,
        "Confirm user presence for key ECDSA-SK SHA256:abc",
    ));
    assert_eq!(press(&mut fixture.app, KeyCode::Esc), Some(Request::Answer(None)));
    assert!(fixture.app.asking.is_none());
    // The install screen stays; the failure brings it back to the access.
    assert!(fixture.app.install.is_some());
}
