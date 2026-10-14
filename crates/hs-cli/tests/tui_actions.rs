//! Actions in the workspaces panel (phase 13b, step 2; mockups 12–14,
//! 19–21): the keys, the form, the plan with its question, the run with
//! its steps, the result; as a box inside the panel.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod tui_fixture;

use crossterm::event::KeyCode;
use hs_cli::tui::action::{Decision, Kind, Progress, Start, Step, StepState};
use hs_cli::tui::draw::render;
use hs_cli::tui::state::{App, Request};
use hs_cli::tui::theme::Palette;
use serde_json::json;
use tui_fixture::{app, position, press, row, screen};

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

fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        press(app, KeyCode::Char(c));
    }
}

fn plan(lines: &[&str], question: &str) -> Progress {
    Progress::Plan {
        lines: lines.iter().map(|line| (*line).to_owned()).collect(),
        question: question.to_owned(),
        blocked: None,
        secret: None,
    }
}

fn started(request: Option<Request>) -> Start {
    match request {
        Some(Request::Act(start)) => start,
        other => panic!("no action started: {other:?}"),
    }
}

#[test]
fn the_actions_light_up_when_a_workspace_is_selected() {
    let mut fixture = app();
    let palette = Palette::named("HomeStack", false).unwrap();
    assert_eq!(press(&mut fixture.app, KeyCode::Char('d')), None, "nothing selected");
    select(&mut fixture.app, 201);
    let buf = render(&fixture.app, 120, 36);
    for label in ["refresh", "resize", "addkey", "migrate", "destroy"] {
        let (x, y) = position(&buf, label);
        assert_eq!(buf[(x + 1, y)].fg, palette.foreground, "{label}");
    }
    // Not yet: ssh, start/stop and setup come in steps 4 and 5.
    let (x, y) = position(&buf, "setup");
    assert_eq!(buf[(x, y)].fg, palette.muted);
}

#[test]
fn destroy_asks_the_server_for_the_plan_and_shows_it_in_a_box() {
    let mut fixture = app();
    select(&mut fixture.app, 205);
    let start = started(press(&mut fixture.app, KeyCode::Char('d')));
    assert_eq!(start.kind, Kind::Destroy);
    assert_eq!(start.parameters, json!({"vmid": 205}));
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("┐destroy 205 scratch┌"), "{text}");
    assert!(text.contains("asking the server for the plan"), "{text}");

    fixture.app.progress(plan(
        &[
            "DESTROY PLAN — NO CHANGES MADE YET",
            "VMID  NAME     NODE  STATUS",
            "205   scratch  pve1  stopped",
        ],
        "Permanently destroy this workspace INCLUDING its persistent home?",
    ));
    let buf = render(&fixture.app, 120, 36);
    let text = screen(&buf);
    assert!(text.contains("DESTROY PLAN"), "{text}");
    assert!(text.contains("Permanently destroy this workspace"), "{text}");
    assert!(text.contains("┐plan┌"), "the stage is raised: {text}");
    // For destroy the focus starts on cancel: Enter cancels.
    assert_eq!(
        press(&mut fixture.app, KeyCode::Enter),
        Some(Request::Decide(Decision::Cancel))
    );
    assert!(fixture.app.action.is_none());
}

#[test]
fn a_applies_the_plan_and_the_steps_show_while_it_runs() {
    let mut fixture = app();
    select(&mut fixture.app, 203);
    let start = started(press(&mut fixture.app, KeyCode::Char('r')));
    assert_eq!(start.kind, Kind::Refresh);
    fixture.app.progress(plan(
        &["REFRESH PLAN — NO CHANGES MADE YET"],
        "Refresh this workspace root?",
    ));
    assert_eq!(
        press(&mut fixture.app, KeyCode::Char('a')),
        Some(Request::Decide(Decision::Apply(None)))
    );
    fixture.app.progress(Progress::Steps(vec![
        Step {
            name: "Stop the VM".to_owned(),
            state: StepState::Done,
        },
        Step {
            name: "Clone Gold".to_owned(),
            state: StepState::Running,
        },
    ]));
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("✓ Stop the VM"), "{text}");
    assert!(text.contains("◐ Clone Gold"), "{text}");
    assert!(text.contains("┐run┌"), "{text}");
    assert!(text.contains("◐ refresh"), "the row shows the work: {text}");
}

#[test]
fn esc_hides_a_running_action_and_enter_shows_it_again() {
    let mut fixture = app();
    select(&mut fixture.app, 203);
    started(press(&mut fixture.app, KeyCode::Char('r')));
    fixture
        .app
        .progress(plan(&["REFRESH PLAN"], "Refresh this workspace root?"));
    press(&mut fixture.app, KeyCode::Char('a'));
    fixture.app.progress(Progress::Steps(vec![Step {
        name: "Stop the VM".to_owned(),
        state: StepState::Running,
    }]));
    assert_eq!(press(&mut fixture.app, KeyCode::Esc), None);
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(!text.contains("┐refresh 203"), "{text}");
    assert!(text.contains("◐ refresh"), "{text}");
    // A second action waits for the first.
    select(&mut fixture.app, 201);
    assert_eq!(press(&mut fixture.app, KeyCode::Char('d')), None);
    assert!(fixture.app.lines.last().unwrap().text.contains("one action at a time"));
    press(&mut fixture.app, KeyCode::Enter);
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("┐refresh 203 web-lab┌"), "{text}");
}

#[test]
fn the_result_closes_with_enter_and_the_status_is_read_again() {
    let mut fixture = app();
    select(&mut fixture.app, 203);
    started(press(&mut fixture.app, KeyCode::Char('r')));
    fixture
        .app
        .progress(plan(&["REFRESH PLAN"], "Refresh this workspace root?"));
    press(&mut fixture.app, KeyCode::Char('a'));
    fixture.app.progress(Progress::Done {
        ok: true,
        lines: vec!["REFRESH COMPLETE".to_owned(), "ssh developer@10.0.0.23".to_owned()],
    });
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("REFRESH COMPLETE") && text.contains("✓ done"), "{text}");
    assert_eq!(press(&mut fixture.app, KeyCode::Enter), Some(Request::Reload));
    assert!(fixture.app.action.is_none());
}

#[test]
fn a_failed_action_says_why_in_the_error_colour() {
    let mut fixture = app();
    select(&mut fixture.app, 203);
    started(press(&mut fixture.app, KeyCode::Char('r')));
    fixture.app.progress(Progress::Done {
        ok: false,
        lines: vec!["VM 203 is locked (backup)".to_owned()],
    });
    let buf = render(&fixture.app, 120, 36);
    let (x, y) = position(&buf, "VM 203 is locked");
    assert_eq!(buf[(x, y)].fg, Palette::named("HomeStack", false).unwrap().error);
    assert!(screen(&buf).contains("✗ failed"), "{}", screen(&buf));
}

#[test]
fn new_opens_a_form_and_its_values_go_to_the_plan() {
    let mut fixture = app();
    assert_eq!(press(&mut fixture.app, KeyCode::Char('n')), None);
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("┐new workspace┌") && text.contains("┐form┌"), "{text}");
    // The VMID is the first free one of the workspaces' range.
    assert!(text.contains("[ 101010"), "{text}");
    for label in ["VMID", "Name", "Node", "Storage", "Home size"] {
        assert!(text.contains(label), "{label}: {text}");
    }
    press(&mut fixture.app, KeyCode::Tab);
    type_text(&mut fixture.app, "lab-new");
    let start = started(press(&mut fixture.app, KeyCode::Enter));
    assert_eq!(start.kind, Kind::New);
    assert_eq!(start.parameters, json!({"vmid": 101010, "name": "lab-new"}));
}

#[test]
fn the_form_refuses_a_bad_name_before_asking_the_server() {
    let mut fixture = app();
    press(&mut fixture.app, KeyCode::Char('n'));
    press(&mut fixture.app, KeyCode::Tab);
    type_text(&mut fixture.app, "1bad");
    assert_eq!(press(&mut fixture.app, KeyCode::Enter), None);
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("Invalid name \"1bad\""), "{text}");
}

#[test]
fn a_node_and_storage_chosen_in_the_form_are_sent() {
    let mut fixture = app();
    press(&mut fixture.app, KeyCode::Char('n'));
    press(&mut fixture.app, KeyCode::Tab);
    type_text(&mut fixture.app, "lab-new");
    press(&mut fixture.app, KeyCode::Tab);
    // Node: default, pve1, pve2, pve3.
    press(&mut fixture.app, KeyCode::Right);
    press(&mut fixture.app, KeyCode::Right);
    press(&mut fixture.app, KeyCode::Tab);
    // Storage: default, then the storages of pve2.
    press(&mut fixture.app, KeyCode::Right);
    press(&mut fixture.app, KeyCode::Tab);
    type_text(&mut fixture.app, "64G");
    let start = started(press(&mut fixture.app, KeyCode::Enter));
    assert_eq!(
        start.parameters,
        json!({"vmid": 101010, "name": "lab-new", "node": "pve2", "storage": "local-lvm", "home_size": "64G"})
    );
}

#[test]
fn resize_sends_the_chosen_disk_and_size() {
    let mut fixture = app();
    select(&mut fixture.app, 201);
    assert_eq!(press(&mut fixture.app, KeyCode::Char('z')), None);
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(
        text.contains("┐resize 201 dev-main┌") && text.contains("‹ home ›"),
        "{text}"
    );
    press(&mut fixture.app, KeyCode::Tab);
    type_text(&mut fixture.app, "96G");
    let start = started(press(&mut fixture.app, KeyCode::Enter));
    assert_eq!(start.kind, Kind::Resize);
    assert_eq!(start.parameters, json!({"vmid": 201, "home_size": "96G"}));
}

#[test]
fn migrate_offers_the_other_nodes_and_their_storages() {
    let mut fixture = app();
    select(&mut fixture.app, 203);
    press(&mut fixture.app, KeyCode::Char('g'));
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("‹ pve2 ›"), "pve1 is where it is: {text}");
    let start = started(press(&mut fixture.app, KeyCode::Enter));
    assert_eq!(
        start.parameters,
        json!({"vmid": 203, "target_node": "pve2", "target_storage": "local-lvm"})
    );
}

#[test]
fn addkey_offers_the_desktops_public_keys_and_the_existing_ones() {
    let mut fixture = app();
    fixture.app.set_keys(vec!["~/.ssh/id_ed25519.pub".to_owned()]);
    select(&mut fixture.app, 201);
    press(&mut fixture.app, KeyCode::Char('k'));
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("the keys already in sshkeys"), "{text}");
    let start = started(press(&mut fixture.app, KeyCode::Enter));
    assert_eq!(start.parameters, json!({"vmid": 201, "restart": false}));
    assert_eq!(start.key_file, None);
    fixture.app.progress(Progress::Done {
        ok: true,
        lines: vec![],
    });
    press(&mut fixture.app, KeyCode::Enter);

    press(&mut fixture.app, KeyCode::Char('k'));
    press(&mut fixture.app, KeyCode::Right);
    let start = started(press(&mut fixture.app, KeyCode::Enter));
    assert_eq!(start.key_file.as_deref(), Some("~/.ssh/id_ed25519.pub"));
}

#[test]
fn the_restart_question_of_addkey_defaults_to_later() {
    let mut fixture = app();
    select(&mut fixture.app, 201);
    press(&mut fixture.app, KeyCode::Char('k'));
    started(press(&mut fixture.app, KeyCode::Enter));
    fixture.app.progress(Progress::Question {
        lines: vec![
            "The key is prepared. It applies after VM 201 (dev-main) restarts; everything running on it will stop."
                .to_owned(),
            "Restart dev-main now?".to_owned(),
        ],
    });
    let text = screen(&render(&fixture.app, 160, 45));
    assert!(text.contains("Restart dev-main now?"), "{text}");
    assert_eq!(
        press(&mut fixture.app, KeyCode::Enter),
        Some(Request::Decide(Decision::Restart(false)))
    );
}

#[test]
fn a_luks_passphrase_is_typed_hidden_in_the_plan() {
    let mut fixture = app();
    select(&mut fixture.app, 201);
    press(&mut fixture.app, KeyCode::Char('z'));
    press(&mut fixture.app, KeyCode::Tab);
    type_text(&mut fixture.app, "96G");
    started(press(&mut fixture.app, KeyCode::Enter));
    fixture.app.progress(Progress::Plan {
        lines: vec!["RESIZE PLAN — NO CHANGES MADE YET".to_owned()],
        question: "Resize this workspace disk?".to_owned(),
        blocked: None,
        secret: Some("LUKS passphrase for VM 201".to_owned()),
    });
    type_text(&mut fixture.app, "s3cret");
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("••••••") && !text.contains("s3cret"), "{text}");
    press(&mut fixture.app, KeyCode::Tab);
    assert_eq!(
        press(&mut fixture.app, KeyCode::Enter),
        Some(Request::Decide(Decision::Apply(Some(
            json!({"luks_passphrase": "s3cret"})
        ))))
    );
}

#[test]
fn a_plan_that_changes_nothing_has_no_apply() {
    let mut fixture = app();
    select(&mut fixture.app, 201);
    press(&mut fixture.app, KeyCode::Char('z'));
    press(&mut fixture.app, KeyCode::Tab);
    type_text(&mut fixture.app, "64G");
    started(press(&mut fixture.app, KeyCode::Enter));
    fixture.app.progress(Progress::Plan {
        lines: vec!["RESIZE PLAN — NO CHANGES MADE YET".to_owned()],
        question: "Resize this workspace disk?".to_owned(),
        blocked: Some("Disk is already 64G: 64G -> 64G changes nothing.".to_owned()),
        secret: None,
    });
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(
        text.contains("Disk is already 64G") && !text.contains("┘apply└"),
        "{text}"
    );
    assert_eq!(press(&mut fixture.app, KeyCode::Char('a')), None);
    assert_eq!(
        press(&mut fixture.app, KeyCode::Esc),
        Some(Request::Decide(Decision::Cancel))
    );
}

#[test]
fn at_80_columns_the_action_box_splits_the_panel() {
    let mut fixture = app();
    select(&mut fixture.app, 205);
    press(&mut fixture.app, KeyCode::Char('d'));
    fixture
        .app
        .progress(plan(&["DESTROY PLAN — NO CHANGES MADE YET"], "Permanently destroy?"));
    let buf = render(&fixture.app, 80, 24);
    let text = screen(&buf);
    assert!(
        text.contains("┐destroy 205 scratch┌") && text.contains("DESTROY PLAN"),
        "{text}"
    );
    // The list keeps a few rows under the box.
    assert!(text.contains("scratch"), "{text}");
    assert!(row(&buf, 23).contains("┘5/12└") || text.contains("5/12"), "{text}");
}

#[test]
fn a_rule_of_the_plan_is_cut_not_wrapped() {
    let mut fixture = app();
    select(&mut fixture.app, 205);
    press(&mut fixture.app, KeyCode::Char('d'));
    fixture.app.progress(plan(
        &["DESTROY PLAN", &"-".repeat(120), "VMID : 205"],
        "Permanently destroy?",
    ));
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(!text.lines().any(|line| line.contains("│ - ")), "{text}");
}

#[test]
fn a_rule_of_dashes_of_the_cli_is_a_blank_line_in_the_box() {
    let lines = hs_cli::tui::action::unframe(
        "╭── WORKSPACE CREATED ──╮\n│ VMID : 101010 │\n│ ------------- │\n│ IP   : 192.168.101.10/22 │\n╰───────────────╯\n",
    );
    assert_eq!(
        lines,
        ["WORKSPACE CREATED", "VMID : 101010", "", "IP   : 192.168.101.10/22"]
    );
}

#[test]
fn t_stops_a_running_workspace_and_starts_a_stopped_one() {
    let mut fixture = app();
    select(&mut fixture.app, 201);
    let start = started(press(&mut fixture.app, KeyCode::Char('t')));
    assert_eq!((start.kind, start.vmid), (Kind::Stop, Some(201)));
    assert_eq!(start.kind.operation(), "workspace.stop");
    fixture.app.action = None;
    select(&mut fixture.app, 203);
    let start = started(press(&mut fixture.app, KeyCode::Char('t')));
    assert_eq!((start.kind, start.kind.operation()), (Kind::Start, "workspace.start"));
}
