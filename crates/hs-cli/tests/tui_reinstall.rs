//! The servers group: Space marks a server container, Enter opens the
//! reinstall box for it or for every marked one, with the image each was
//! made from and the images published.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod tui_fixture;

use crossterm::event::KeyCode;
use hs_cli::tui::action::{Kind, Progress, Start};
use hs_cli::tui::draw::render;
use hs_cli::tui::reinstall::{Image, Images, Server};
use hs_cli::tui::state::{App, Request};
use serde_json::json;
use tui_fixture::{app, position, press, screen};

const REPOSITORY: &str = "ghcr.io/hipotures/homestack-server";

/// The app with three servers, the containers group shown.
fn servers() -> tui_fixture::Fixture {
    let mut fixture = app();
    let mut status = tui_fixture::status();
    status["pools"] = json!([
        {"id": "homestack", "comment": "Workspaces"},
        {"id": "homestack-service", "comment": "Containers"},
    ]);
    status["others"] = json!(
        (1..=3)
            .map(|n| json!({
                "vmid": 102_000 + n, "role": "CT", "name": format!("api{n}"), "status": "running",
                "node": format!("pve{n}"), "node_status": "online", "pool": "homestack-service",
                "tags": ["homestack-server"], "ip": format!("10.0.102.{n}"), "root_storage": "local-lvm",
                "home": null, "role_tag_ok": true, "warning": null
            }))
            .collect::<Vec<_>>()
    );
    fixture.app.set_status(&status);
    fixture.app.group = Some("homestack-service".to_owned());
    fixture
}

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

fn image(tag: &str, made: &str) -> Image {
    Image {
        image: format!("{REPOSITORY}:{tag}"),
        made: made.to_owned(),
        created: Some(made.replace(' ', "T")),
    }
}

fn images(names: &[(u32, &str)]) -> Images {
    let old = image("0.2.0", "2026-10-16 03:23");
    Images {
        servers: names
            .iter()
            .map(|(vmid, name)| Server {
                vmid: *vmid,
                name: (*name).to_owned(),
                image: Some(old.clone()),
            })
            .collect(),
        published: vec![image("0.2.1", "2026-10-16 03:42"), old],
    }
}

fn started(request: Option<Request>) -> Start {
    match request {
        Some(Request::Act(start)) => start,
        other => panic!("no action started: {other:?}"),
    }
}

#[test]
fn enter_on_a_server_reads_the_images_and_shows_them() {
    let mut fixture = servers();
    select(&mut fixture.app, 102_002);
    assert_eq!(
        press(&mut fixture.app, KeyCode::Enter),
        Some(Request::Images(vec![102_002]))
    );
    let text = screen(&render(&fixture.app, 140, 40));
    assert!(
        text.contains("update API api2") && text.contains("reading the images"),
        "{text}"
    );

    fixture.app.set_images(&[102_002], Ok(images(&[(102_002, "api2")])));
    let text = screen(&render(&fixture.app, 140, 40));
    assert!(text.contains("Image      ‹ 0.2.1  2026-10-16 03:42 ›"), "{text}");
    assert!(text.contains("api2       0.2.0  2026-10-16 03:23"), "{text}");
    assert!(text.contains("› 0.2.1  2026-10-16 03:42  new"), "{text}");
    assert!(text.contains("  0.2.0  2026-10-16 03:23  now on api2"), "{text}");

    let start = started(press(&mut fixture.app, KeyCode::Enter));
    assert_eq!(start.kind, Kind::Reinstall);
    assert_eq!(start.parameters["image"], format!("{REPOSITORY}:0.2.1"));
    assert_eq!(start.parameters["servers"], json!(["api2"]));
    let plan = start.parameters["plan"].to_string();
    assert!(plan.contains("api2  CT 102002  0.2.0 → 0.2.1"), "{plan}");
}

#[test]
fn space_marks_servers_and_enter_on_a_marked_one_takes_them_all() {
    let mut fixture = servers();
    select(&mut fixture.app, 102_001);
    press(&mut fixture.app, KeyCode::Char(' '));
    select(&mut fixture.app, 102_003);
    press(&mut fixture.app, KeyCode::Char(' '));
    // Marked lines are lighter, from the first cell; the others are not.
    select(&mut fixture.app, 102_002);
    let buf = render(&fixture.app, 140, 40);
    let selection = fixture.app.palette.selection;
    for (name, marked) in [("api1", true), ("api2", false), ("api3", true)] {
        let (x, y) = position(&buf, name);
        assert_eq!(buf[(x, y)].bg == selection, marked, "{name}");
        assert_eq!(buf[(x - 9, y)].bg == selection, marked, "{name}: the whole line");
    }
    assert!(!screen(&buf).contains('✓'));
    // The count on the frame, in another group too.
    assert!(screen(&buf).contains("┘2 selected└┘2/3└"), "{}", screen(&buf));
    fixture.app.group = Some("homestack".to_owned());
    assert!(screen(&render(&fixture.app, 140, 40)).contains("┘2 selected└"));
    fixture.app.group = Some("homestack-service".to_owned());

    // Unmarked: only itself.
    select(&mut fixture.app, 102_002);
    assert_eq!(
        press(&mut fixture.app, KeyCode::Enter),
        Some(Request::Images(vec![102_002]))
    );
    press(&mut fixture.app, KeyCode::Esc);
    assert!(fixture.app.action.is_none());

    // Marked: every marked one.
    select(&mut fixture.app, 102_003);
    assert_eq!(
        press(&mut fixture.app, KeyCode::Enter),
        Some(Request::Images(vec![102_001, 102_003]))
    );
    fixture
        .app
        .set_images(&[102_001, 102_003], Ok(images(&[(102_001, "api1"), (102_003, "api3")])));
    let start = started(press(&mut fixture.app, KeyCode::Enter));
    assert_eq!(start.parameters["servers"], json!(["api1", "api3"]));

    // The plan, then the run: both rows show it once the box is hidden.
    fixture.app.progress(Progress::Plan {
        lines: vec!["Reinstall from 0.2.1".to_owned()],
        question: Kind::Reinstall.question(false).to_owned(),
        blocked: None,
        secret: None,
    });
    let text = screen(&render(&fixture.app, 140, 40));
    assert!(text.contains("Update these API containers?"), "{text}");
    press(&mut fixture.app, KeyCode::Enter);
    press(&mut fixture.app, KeyCode::Esc);
    let text = screen(&render(&fixture.app, 140, 40));
    assert_eq!(text.matches('◐').count(), 2, "{text}");

    // Space unmarks.
    select(&mut fixture.app, 102_001);
    fixture.app.action = None;
    press(&mut fixture.app, KeyCode::Char(' '));
    assert_eq!(fixture.app.marked.iter().copied().collect::<Vec<_>>(), [102_003]);
    assert!(screen(&render(&fixture.app, 140, 40)).contains("┘1 selected└"));
    select(&mut fixture.app, 102_003);
    press(&mut fixture.app, KeyCode::Char(' '));
    assert!(!screen(&render(&fixture.app, 140, 40)).contains("selected└"));
}

#[test]
fn space_does_not_mark_a_workspace_and_enter_opens_its_setup() {
    let mut fixture = app();
    select(&mut fixture.app, 201);
    press(&mut fixture.app, KeyCode::Char(' '));
    assert!(fixture.app.marked.is_empty());
    assert_eq!(press(&mut fixture.app, KeyCode::Enter), Some(Request::Setup(Some(201))));
    assert!(fixture.app.action.is_none());
    assert!(fixture.app.setup.is_some());
}

#[test]
fn images_for_another_box_are_dropped_and_a_failure_is_shown() {
    let mut fixture = servers();
    select(&mut fixture.app, 102_001);
    press(&mut fixture.app, KeyCode::Enter);
    fixture.app.set_images(&[102_002], Ok(images(&[(102_002, "api2")])));
    assert!(fixture.app.images.is_none());
    fixture.app.set_images(
        &[102_001],
        Err("CT 102001 is not an instance of deploy.toml".to_owned()),
    );
    let text = screen(&render(&fixture.app, 140, 40));
    assert!(text.contains("CT 102001 is not an instance of deploy.toml"), "{text}");
    press(&mut fixture.app, KeyCode::Enter);
    assert!(
        matches!(
            fixture.app.action.as_ref().map(|action| &action.stage),
            Some(hs_cli::tui::action::Stage::Form(_))
        ),
        "the form stays"
    );
}

#[test]
fn the_run_shows_every_server_and_what_the_one_under_way_does() {
    use hs_cli::tui::action::{Step, StepState};
    let mut fixture = servers();
    select(&mut fixture.app, 102_001);
    press(&mut fixture.app, KeyCode::Enter);
    fixture.app.set_images(&[102_001], Ok(images(&[(102_001, "api1")])));
    press(&mut fixture.app, KeyCode::Enter);
    fixture.app.progress(Progress::Plan {
        lines: Vec::new(),
        question: Kind::Reinstall.question(false).to_owned(),
        blocked: None,
        secret: None,
    });
    press(&mut fixture.app, KeyCode::Enter);
    let step = |name: &str, state| Step {
        name: name.to_owned(),
        state,
    };
    fixture.app.progress(Progress::Steps(vec![
        step("api1: reinstalled from 0.2.161", StepState::Done),
        step("api2: starting", StepState::Running),
        step("api3: waiting", StepState::Waiting),
    ]));
    let text = screen(&render(&fixture.app, 140, 40));
    for line in [
        "✓ api1: reinstalled from 0.2.161",
        "◐ api2: starting",
        "○ api3: waiting",
        "1 of 3 steps done",
    ] {
        assert!(text.contains(line), "{line}: {text}");
    }
}

#[test]
fn update_arrow_touches_the_name_and_stays_separate_from_a_six_digit_vmid() {
    let mut fixture = servers();
    let mut status = serde_json::to_value(fixture.app.status.as_ref().unwrap()).unwrap();
    status["others"][0]["image_update"] = json!({
        "state": "available", "available": format!("{REPOSITORY}:0.2.215"),
        "checked_at": "2026-10-19T10:00:00Z", "error": null
    });
    fixture.app.set_status(&status);
    for width in [80, 120, 160] {
        let buf = render(&fixture.app, width, 36);
        let text = screen(&buf);
        assert!(text.contains("102001 ↑api1"), "{text}");
        assert!(text.contains("102002  api2"), "{text}");
        let (first_x, _) = position(&buf, "api1");
        let (second_x, _) = position(&buf, "api2");
        assert_eq!(first_x, second_x);
    }
    status["others"][0]["image_update"]["available"] = serde_json::Value::Null;
    status["others"][0]["image_update"]["state"] = json!("up_to_date");
    fixture.app.set_status(&status);
    assert!(!screen(&render(&fixture.app, 120, 36)).contains("↑api1"));
}

#[test]
fn container_details_show_installed_and_available_images_and_check_time() {
    let mut fixture = servers();
    let mut status = serde_json::to_value(fixture.app.status.as_ref().unwrap()).unwrap();
    status["others"][0]["image"] = json!(format!("{REPOSITORY}:0.2.214"));
    status["others"][0]["image_update"] = json!({
        "state": "available", "available": format!("{REPOSITORY}:0.2.215"),
        "checked_at": "2026-10-19T10:00:00Z", "error": null
    });
    fixture.app.set_status(&status);
    select(&mut fixture.app, 102_001);
    fixture.app.info = true;
    let text = screen(&render(&fixture.app, 160, 36));
    for expected in ["0.2.214", "0.2.215", "2026-10-19 10:00 UTC"] {
        assert!(text.contains(expected), "{text}");
    }
}

#[test]
fn startup_update_notice_survives_missing_or_invalid_main_status() {
    let mut fixture = app();
    fixture.app.status = None;
    fixture.app.set_api_updates(Ok(images(&[(102001, "api1")])));
    fixture.app.set_notice(Some((
        "Invalid server status".to_owned(),
        vec!["Schema mismatch".to_owned()],
    )));
    assert_eq!(fixture.app.api_update_label().as_deref(), Some("API ↑1"));
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("API ↑1"), "{text}");
    assert!(
        fixture
            .app
            .lines
            .iter()
            .any(|line| line.text.contains("API update available: api1"))
    );
    assert!(fixture.app.action.is_none(), "Checking must never start a deployment");
}

#[test]
fn fresh_startup_check_overrides_stale_server_update_cache() {
    let mut fixture = servers();
    let mut status = serde_json::to_value(fixture.app.status.as_ref().unwrap()).unwrap();
    for entry in status["others"].as_array_mut().unwrap() {
        entry["image_update"] = json!({"state": "up_to_date", "available": null, "checked_at": null, "error": null});
    }
    fixture
        .app
        .set_api_updates(Ok(images(&[(102001, "api1"), (102002, "api2"), (102003, "api3")])));
    fixture.app.set_status(&status);
    assert_eq!(fixture.app.api_update_label().as_deref(), Some("API ↑3"));
    assert!(
        fixture.app.status.as_ref().unwrap().others.iter().all(|entry| entry
            .image_update
            .as_ref()
            .unwrap()
            .available
            .is_some())
    );
    let text = screen(&render(&fixture.app, 140, 40));
    assert!(text.contains("API ↑3"), "{text}");
    assert!(fixture.app.action.is_none());
}

#[test]
fn failed_startup_check_is_reported_without_blocking_workspaces() {
    let mut fixture = app();
    fixture.app.api_updates_checking = true;
    fixture.app.set_api_updates(Err("Registry unavailable".to_owned()));
    assert!(!fixture.app.api_updates_checking);
    assert_eq!(fixture.app.api_update_label().as_deref(), Some("API ?"));
    let text = screen(&render(&fixture.app, 120, 36));
    assert!(text.contains("dev-main") && text.contains("API ?"), "{text}");
    assert!(
        fixture
            .app
            .lines
            .iter()
            .any(|line| line.text.contains("Registry unavailable"))
    );
    assert!(fixture.app.action.is_none());
}

#[test]
fn newer_build_is_detected_when_registry_timestamps_are_missing() {
    let current = Image {
        image: format!("{REPOSITORY}:0.2.100"),
        made: "—".to_owned(),
        created: None,
    };
    let newer = Image {
        image: format!("{REPOSITORY}:0.2.101"),
        made: "—".to_owned(),
        created: None,
    };
    let images = Images {
        servers: vec![Server {
            vmid: 102001,
            name: "api1".to_owned(),
            image: Some(current),
        }],
        published: vec![newer],
    };
    assert!(images.update_for(&images.servers[0]).is_some());
}
