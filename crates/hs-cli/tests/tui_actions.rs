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
    select(&mut fixture.app, 201);
    let buf = render(&fixture.app, 120, 36);
    for label in ["refresh", "resize", "addkey", "migrate", "destroy"] {
        let (x, y) = position(&buf, label);
        assert_eq!(buf[(x + 1, y)].fg, palette.foreground, "{label}");
    }
    // 201 runs: setup too (step 5); its key is the e.
    let (x, y) = position(&buf, "setup");
    assert_eq!(buf[(x, y)].fg, palette.foreground);
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
    // Enter applies, in destroy too (owner, 2026-10-15).
    assert_eq!(
        press(&mut fixture.app, KeyCode::Enter),
        Some(Request::Decide(Decision::Apply(None)))
    );
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
    let busy = text.lines().find(|line| line.contains("│ 203 ")).unwrap_or_default();
    assert!(
        busy.contains("◐") && !busy.contains("refresh"),
        "a busy dot, the word is in the box: {busy}"
    );
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
    let busy = text.lines().find(|line| line.contains("│ 203 ")).unwrap_or_default();
    assert!(busy.contains("◐"), "the hidden action still shows on its row: {busy}");
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
    // Without the pools in the status, the server's; its first free VMID and that VMID's address are
    // only shown.
    assert!(text.contains("Pool       ‹ homestack ›"), "{text}");
    assert!(text.contains("VMID       101010"), "{text}");
    assert!(text.contains("IP         10.0.101.10"), "{text}");
    assert!(!text.contains("[ 101010"), "{text}");
    for label in ["Name", "Node", "Storage", "Home size"] {
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
    fixture.app.home_default = Some(63 * 1024);
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
    // Home size: one G above the server's default.
    press(&mut fixture.app, KeyCode::Right);
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
    // From the 64G home: three times 10 GiB and twice 1 GiB.
    for code in [
        KeyCode::PageUp,
        KeyCode::PageUp,
        KeyCode::PageUp,
        KeyCode::Right,
        KeyCode::Right,
    ] {
        press(&mut fixture.app, code);
    }
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
    // From the 64G home: three times 10 GiB and twice 1 GiB.
    for code in [
        KeyCode::PageUp,
        KeyCode::PageUp,
        KeyCode::PageUp,
        KeyCode::Right,
        KeyCode::Right,
    ] {
        press(&mut fixture.app, code);
    }
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
    press(&mut fixture.app, KeyCode::Right);
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
    assert!(row(&buf, 23).contains("┘6/13└") || text.contains("6/13"), "{text}");
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

#[test]
fn a_click_focuses_a_field_and_a_click_on_its_arrow_changes_the_choice() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let mut fixture = app();
    press(&mut fixture.app, KeyCode::Char('n'));
    let buf = render(&fixture.app, 120, 36);
    let (x, y) = position(&buf, "Node");
    let click = |column, row| MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };
    fixture.app.mouse(click(x, y));
    let buf = render(&fixture.app, 120, 36);
    let text = screen(&buf);
    assert!(
        text.contains("Where the VM is made"),
        "the Node field has the focus: {text}"
    );
    let line = row(&buf, y);
    let arrow = u16::try_from(line.chars().position(|c| c == '›').unwrap()).unwrap();
    fixture.app.mouse(click(arrow, y));
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("‹ pve1 ›"), "the next node: {text}");
}

#[test]
fn resize_starts_at_the_disk_s_size_only_grows_and_needs_a_change() {
    let mut fixture = app();
    select(&mut fixture.app, 201);
    fixture.app.set_details(201, &tui_fixture::details());
    press(&mut fixture.app, KeyCode::Char('z'));
    press(&mut fixture.app, KeyCode::Down);
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("‹ 64G ›"), "the home's size now: {text}");
    // Unchanged: refused before the server is asked.
    assert_eq!(press(&mut fixture.app, KeyCode::Enter), None);
    assert!(screen(&render(&fixture.app, 120, 36)).contains("Choose a size above the 64G"));
    // Never below the size now.
    press(&mut fixture.app, KeyCode::Left);
    assert!(screen(&render(&fixture.app, 120, 36)).contains("‹ 64G ›"));
    press(&mut fixture.app, KeyCode::Right);
    press(&mut fixture.app, KeyCode::PageUp);
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("‹ 75G ›") && text.contains("+11G"), "{text}");
    let start = started(press(&mut fixture.app, KeyCode::Enter));
    assert_eq!(start.parameters, json!({"vmid": 201, "home_size": "75G"}));
}

#[test]
fn a_plan_is_not_a_change_so_its_row_shows_no_busy_dot() {
    let mut fixture = app();
    select(&mut fixture.app, 203);
    started(press(&mut fixture.app, KeyCode::Char('d')));
    fixture
        .app
        .progress(plan(&["DESTROY PLAN"], "Permanently destroy this workspace?"));
    let text = screen(&render(&fixture.app, 120, 36));
    let row = text.lines().find(|line| line.contains("│ 203 ")).unwrap_or_default();
    assert!(!row.contains('◐'), "only a plan: {row}");
}

#[test]
fn the_focused_button_of_a_plan_is_an_accent_bar() {
    let mut fixture = app();
    select(&mut fixture.app, 203);
    started(press(&mut fixture.app, KeyCode::Char('r')));
    fixture
        .app
        .progress(plan(&["REFRESH PLAN"], "Refresh this workspace root?"));
    let palette = Palette::named("HomeStack", false).unwrap();
    let buf = render(&fixture.app, 120, 36);
    let (x, y) = position(&buf, "apply");
    assert_eq!(buf[(x + 1, y)].bg, palette.focus, "apply has the focus");
    let (x, y) = position(&buf, "cancel esc");
    assert_ne!(buf[(x + 1, y)].bg, palette.focus);
    press(&mut fixture.app, KeyCode::Right);
    let buf = render(&fixture.app, 120, 36);
    let (x, y) = position(&buf, "cancel esc");
    assert_eq!(buf[(x + 1, y)].bg, palette.focus, "cancel has it now");
}

/// The app with the pools of an install, each by its comment.
fn app_with_pools() -> tui_fixture::Fixture {
    let mut fixture = app();
    let mut status = tui_fixture::status();
    status["pools"] = json!([
        {"id": "homestack", "comment": "Workspaces"},
        {"id": "homestack-gold", "comment": "Gold"},
        {"id": "homestack-test", "comment": "Test Workspaces"},
    ]);
    fixture.app.set_status(&status);
    fixture.app.pool = Some("homestack".to_owned());
    fixture
}

#[test]
fn new_takes_the_pool_of_the_group_shown_by_its_name() {
    // All, and a group no workspace goes to: the workspaces.
    for group in [None, Some("homestack-gold")] {
        let mut fixture = app_with_pools();
        fixture.app.group = group.map(str::to_owned);
        press(&mut fixture.app, KeyCode::Char('n'));
        let text = screen(&render(&fixture.app, 120, 36));
        assert!(text.contains("Pool       ‹ workspaces ›"), "{group:?}: {text}");
        assert!(text.contains("VMID       101010"), "{group:?}: {text}");
    }

    // Test workspaces: their VMIDs and addresses, and the pool is sent.
    let mut fixture = app_with_pools();
    fixture.app.group = Some("homestack-test".to_owned());
    press(&mut fixture.app, KeyCode::Char('n'));
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("Pool       ‹ test workspaces ›"), "{text}");
    assert!(text.contains("VMID       101200"), "{text}");
    assert!(text.contains("IP         10.0.101.200"), "{text}");
    press(&mut fixture.app, KeyCode::Tab);
    type_text(&mut fixture.app, "lab-new");
    let start = started(press(&mut fixture.app, KeyCode::Enter));
    assert_eq!(
        start.parameters,
        json!({"vmid": 101200, "name": "lab-new", "pool": "homestack-test"})
    );
}

#[test]
fn new_pool_choice_changes_the_vmid_and_address() {
    let mut fixture = app_with_pools();
    press(&mut fixture.app, KeyCode::Char('n'));
    press(&mut fixture.app, KeyCode::Right);
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("Pool       ‹ test workspaces ›"), "{text}");
    assert!(text.contains("VMID       101200"), "{text}");
    press(&mut fixture.app, KeyCode::Right);
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("Pool       ‹ workspaces ›"), "{text}");
    assert!(text.contains("IP         10.0.101.10"), "{text}");
}

/// The new form with the focus on Home size (the fifth field).
fn home_size_form(default_gib: Option<u64>) -> tui_fixture::Fixture {
    let mut fixture = app();
    fixture.app.home_default = default_gib.map(|gib| gib * 1024);
    press(&mut fixture.app, KeyCode::Char('n'));
    press(&mut fixture.app, KeyCode::Tab);
    type_text(&mut fixture.app, "lab-new");
    for _ in 0..3 {
        press(&mut fixture.app, KeyCode::Tab);
    }
    fixture
}

#[test]
fn home_size_starts_at_the_default_and_is_sent_only_when_changed() {
    let mut fixture = home_size_form(Some(64));
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("Home size  64G  -64G+  -0M+"), "{text}");
    let start = started(press(&mut fixture.app, KeyCode::Enter));
    assert_eq!(start.parameters, json!({"vmid": 101010, "name": "lab-new"}));

    let mut fixture = home_size_form(None);
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("Home size  default  -0G+  -0M+"), "{text}");
    // Nothing below nothing.
    press(&mut fixture.app, KeyCode::Left);
    assert!(screen(&render(&fixture.app, 120, 36)).contains("default  -0G+"));
}

#[test]
fn home_size_steps_its_g_and_its_m_part() {
    let mut fixture = home_size_form(Some(64));
    // Space: the M part; three steps of 100M.
    type_text(&mut fixture.app, " +++");
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("Home size  64.3G  -64G+  -300M+"), "{text}");
    let start = started(press(&mut fixture.app, KeyCode::Enter));
    assert_eq!(start.parameters["home_size"], json!("65836M"));

    // The M part carries into the G part, both ways.
    let mut fixture = home_size_form(Some(1));
    press(&mut fixture.app, KeyCode::Char(' '));
    for _ in 0..10 {
        press(&mut fixture.app, KeyCode::Right);
    }
    assert!(screen(&render(&fixture.app, 120, 36)).contains("2G  -2G+  -0M+"));
    press(&mut fixture.app, KeyCode::Left);
    assert!(screen(&render(&fixture.app, 120, 36)).contains("1.9G  -1G+  -900M+"));
    // Space again: the G part; down to 0G, the 900M stay.
    type_text(&mut fixture.app, " --");
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("0.9G  -0G+  -900M+"), "{text}");
    let start = started(press(&mut fixture.app, KeyCode::Enter));
    assert_eq!(start.parameters["home_size"], json!("900M"));
}

#[test]
fn home_size_never_outgrows_the_storage_and_its_colour_follows_its_share() {
    // pve1's local-zfs, Gold's node's first storage, has 1720G free.
    let palette = Palette::named("HomeStack", false).unwrap();
    let colour = |default_gib| {
        let fixture = home_size_form(Some(default_gib));
        let buf = render(&fixture.app, 120, 36);
        let (x, y) = position(&buf, &format!("Home size  {default_gib}G"));
        buf[(x + 11, y)].fg
    };
    assert_eq!(colour(172), palette.foreground, "10 % is fine");
    assert_eq!(colour(173), palette.warning, "above 10 %");
    assert_eq!(colour(861), palette.error, "above 50 %");

    let mut fixture = home_size_form(Some(1720));
    press(&mut fixture.app, KeyCode::Right);
    type_text(&mut fixture.app, " +");
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("Home size  1720G  -1720G+  -0M+"), "{text}");
}

#[test]
fn a_click_chooses_a_part_of_home_size_and_the_wheel_steps_it() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let mut fixture = home_size_form(Some(2));
    let mouse = |kind, column, row| MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };
    let click = MouseEventKind::Down(MouseButton::Left);
    // A click on 0M chooses the M part: the wheel up adds 100M.
    let buf = render(&fixture.app, 120, 36);
    let (label, _) = position(&buf, "Home size");
    let (x, y) = position(&buf, "0M+");
    fixture.app.mouse(mouse(click, x, y));
    fixture.app.mouse(mouse(MouseEventKind::ScrollUp, label, y));
    let buf = render(&fixture.app, 120, 36);
    assert!(screen(&buf).contains("2.1G  -2G+  -100M+"), "{}", screen(&buf));
    // The + of the G part.
    let (x, y) = position(&buf, "-2G+");
    fixture.app.mouse(mouse(click, x + 3, y));
    let buf = render(&fixture.app, 120, 36);
    assert!(screen(&buf).contains("3.1G  -3G+  -100M+"), "{}", screen(&buf));
    // The wheel down over the row: the chosen part, now G.
    fixture.app.mouse(mouse(MouseEventKind::ScrollDown, label, y));
    assert!(screen(&render(&fixture.app, 120, 36)).contains("2.1G  -2G+  -100M+"));
}

#[test]
fn tab_goes_through_g_and_m_and_up_down_switch_them() {
    let mut fixture = home_size_form(Some(2));
    let chosen = |fixture: &tui_fixture::Fixture| {
        let buf = render(&fixture.app, 120, 36);
        let (x, y) = position(&buf, "Home size");
        // The reversed cells: the chosen part; G's number after its -.
        let line: Vec<char> = row(&buf, y).chars().collect();
        let after = (usize::from(x)..line.len())
            .find(|at| line[*at..].starts_with(&[' ', ' ', '-']))
            .unwrap();
        let start = u16::try_from(after + 3).unwrap();
        let g = buf[(start, y)].modifier.contains(ratatui::style::Modifier::REVERSED);
        let focused = buf[(x, y)].modifier.contains(ratatui::style::Modifier::BOLD);
        (focused, if g { "G" } else { "M" })
    };
    assert_eq!(chosen(&fixture), (true, "G"));
    press(&mut fixture.app, KeyCode::Tab);
    assert_eq!(chosen(&fixture), (true, "M"));
    press(&mut fixture.app, KeyCode::Right);
    assert!(screen(&render(&fixture.app, 120, 36)).contains("2.1G  -2G+  -100M+"));
    // ↑ ↓ switch round and stay in the field.
    press(&mut fixture.app, KeyCode::Down);
    assert_eq!(chosen(&fixture), (true, "G"));
    press(&mut fixture.app, KeyCode::Down);
    assert_eq!(chosen(&fixture), (true, "M"));
    press(&mut fixture.app, KeyCode::Up);
    assert_eq!(chosen(&fixture), (true, "G"));
    // BackTab from G leaves for Storage; back in, it lands on M.
    press(&mut fixture.app, KeyCode::BackTab);
    assert!(!chosen(&fixture).0);
    press(&mut fixture.app, KeyCode::Tab);
    assert_eq!(chosen(&fixture), (true, "G"));
    press(&mut fixture.app, KeyCode::Tab);
    press(&mut fixture.app, KeyCode::Tab);
    assert!(!chosen(&fixture).0, "Tab from M goes on to Pool");
    press(&mut fixture.app, KeyCode::BackTab);
    assert_eq!(chosen(&fixture), (true, "M"));
}
