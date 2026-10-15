//! The plan of a setup and the desktop files it copies (Python
//! `test_setup.py::PlanTests`, `AdditionalSafetyTests`,
//! `test_setup_files.py`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod setup_fixture;

use hs_cli::setup::catalog::{Catalog, Github, select_entries};
use hs_cli::setup::definitions::{Application, Entry, Params, config_entries, parse_setup};
use hs_cli::setup::engine::backup_paths_for_entry;
use hs_cli::setup::files::file_plan_item;
use hs_cli::setup::plan::build_plan;
use hs_cli::setup::text::home_path;
use serde_json::{Value, json};
use setup_fixture::{entry, settings, target};

fn file(id: &str, group: &str, path: &str) -> Entry {
    Entry {
        id: id.into(),
        group: group.into(),
        handler: "file".into(),
        label: id.into(),
        description: "A file".into(),
        params: Params::File {
            path: path.into(),
            application: None,
        },
        depends_on: Vec::new(),
    }
}

fn no_github() -> (impl Fn() -> Result<Value, hs_cli::AppError>, impl Fn() -> Catalog) {
    (
        || -> Result<Value, hs_cli::AppError> { panic!("unexpected GitHub") },
        || -> Catalog { panic!("unexpected GitHub") },
    )
}

fn ids(entries: &[Entry]) -> Vec<String> {
    entries.iter().map(|e| e.id.clone()).collect()
}

#[test]
fn repository_clone_never_snapshots_checkout_contents() {
    let home = tempfile::tempdir().unwrap();
    let cfg = settings(home.path());
    let repository = Entry {
        id: "owner/project".into(),
        group: "repo".into(),
        handler: "repository".into(),
        label: "Project".into(),
        description: "Repository".into(),
        params: Params::Repository {
            repository: "owner/project".into(),
        },
        depends_on: Vec::new(),
    };
    let checkout = "/home/user/src/project";
    let missing = json!({"repository": {"checkout_state": "missing", "checkout": checkout},
                         "actions": ["generate-key", "clone", "verify-access"]});
    assert!(backup_paths_for_entry(&cfg, &repository, &missing).is_empty());
    let ready = json!({"repository": {"checkout_state": "ready", "checkout": checkout},
                       "actions": ["set-origin", "verify-access"]});
    assert_eq!(
        backup_paths_for_entry(&cfg, &repository, &ready),
        ["src/project/.git/config"]
    );
}

#[test]
fn independent_apps_repositories_and_explicit_dependencies() {
    let home = tempfile::tempdir().unwrap();
    let cfg = settings(home.path());
    let codex = entry("codex");
    let plan = build_plan(&cfg, &target(), std::slice::from_ref(&codex), None, false, true).unwrap();
    let mut expected = vec![codex.clone()];
    expected.extend(config_entries(&codex));
    assert_eq!(plan.entries, expected);
    let (identity, catalog) = no_github();
    let github = Github {
        identity: &identity,
        catalog: &catalog,
    };
    let (selected, _) = select_entries(&cfg, &["repo=owner/project".to_owned()], None, &github).unwrap();
    assert_eq!(
        build_plan(&cfg, &target(), &selected, None, false, true)
            .unwrap()
            .entries,
        selected
    );
    let mut custom = codex;
    custom.depends_on = vec!["bash".into()];
    let error = build_plan(&cfg, &target(), std::slice::from_ref(&custom), None, false, true).unwrap_err();
    assert!(error.0.contains("explicit selection"), "{}", error.0);
    let plan = build_plan(&cfg, &target(), &[custom.clone(), entry("bash")], None, false, true).unwrap();
    let mut expected = vec!["bash".to_owned(), "codex".to_owned()];
    expected.extend(config_entries(&custom).into_iter().map(|e| e.id));
    assert_eq!(ids(&plan.entries), expected);
}

#[test]
fn selected_overlap_missing_source_and_source_symlink() {
    let home = tempfile::tempdir().unwrap();
    let cfg = settings(home.path());
    std::fs::write(home.path().join(".bashrc"), "custom").unwrap();
    let dotfile = file("dotfile", "files", "~/.bashrc");
    let error = build_plan(&cfg, &target(), &[dotfile, entry("bash")], None, false, true).unwrap_err();
    assert!(error.0.contains("Overlapping"), "{}", error.0);
    std::os::unix::fs::symlink(home.path(), home.path().join("link")).unwrap();
    let linked = file("dotfile", "files", "~/link/.bashrc");
    let error = build_plan(&cfg, &target(), &[linked], None, false, true).unwrap_err();
    assert!(error.0.contains("symlink"), "{}", error.0);
}

#[test]
fn repo_destination_collision() {
    let home = tempfile::tempdir().unwrap();
    let cfg = settings(home.path());
    let (identity, catalog) = no_github();
    let github = Github {
        identity: &identity,
        catalog: &catalog,
    };
    let (selected, _) =
        select_entries(&cfg, &["repo=owner/project,another/project".to_owned()], None, &github).unwrap();
    let error = build_plan(&cfg, &target(), &selected, None, false, true).unwrap_err();
    assert!(error.0.contains("Overlapping"), "{}", error.0);
}

#[test]
fn unattended_interactive_rejected_even_yes_no_implicit_actions() {
    let home = tempfile::tempdir().unwrap();
    let cfg = settings(home.path());
    let mut custom = entry("codex");
    custom.params = Params::Application(Box::new(Application::new("read -r answer")));
    let error = build_plan(&cfg, &target(), std::slice::from_ref(&custom), None, true, true).unwrap_err();
    assert!(error.0.contains("interactive terminal"), "{}", error.0);
    let mut safe = Application::new("read -r answer");
    safe.non_interactive = "printf ready".into();
    custom.params = Params::Application(Box::new(safe));
    build_plan(&cfg, &target(), &[custom], None, true, true).unwrap();
    assert!(build_plan(&cfg, &target(), &[], None, false, true).is_err());
}

#[test]
fn credentials_are_planned_after_everything_else() {
    let home = tempfile::tempdir().unwrap();
    let cfg = settings(home.path());
    std::fs::write(home.path().join("fixture"), "Synthetic fixture").unwrap();
    std::fs::write(home.path().join("secret"), "Synthetic credential").unwrap();
    let mut application = entry("codex");
    let mut printf = Application::new("printf done");
    printf.interaction = "non-interactive".into();
    application.params = Params::Application(Box::new(printf));
    let backup = Entry {
        id: "bk".into(),
        group: "backup".into(),
        handler: "backup".into(),
        label: "BK".into(),
        description: "Backup tool".into(),
        params: Params::Backup,
        depends_on: Vec::new(),
    };
    let plan = build_plan(
        &cfg,
        &target(),
        &[
            file("secret", "cred", "~/secret"),
            file("fixture", "files", "~/fixture"),
            application,
            backup,
            entry("bash"),
        ],
        None,
        false,
        true,
    )
    .unwrap();
    assert_eq!(ids(&plan.entries), ["bash", "fixture", "codex", "bk", "secret"]);
}

#[test]
fn home_path_spec_uses_trailing_slash_for_directory() {
    assert_eq!(
        home_path("~/.config/example/settings.json").unwrap(),
        (".config/example/settings.json".into(), false)
    );
    assert_eq!(
        home_path("~/.config/example-tool/").unwrap(),
        (".config/example-tool".into(), true)
    );
    for invalid in ["/tmp/outside", "~/", "~/.config/../parent", "~/.config//item"] {
        assert!(home_path(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn file_plan_item_checks_file_and_directory_type() {
    let home = tempfile::tempdir().unwrap();
    let cfg = settings(home.path());
    std::fs::create_dir_all(home.path().join(".config/example")).unwrap();
    std::fs::write(home.path().join(".config/example/settings.json"), "example").unwrap();
    std::fs::create_dir_all(home.path().join(".config/example-tool")).unwrap();
    let file_item = file_plan_item(&cfg, "~/.config/example/settings.json").unwrap();
    let dir_item = file_plan_item(&cfg, "~/.config/example-tool/").unwrap();
    let mismatch = file_plan_item(&cfg, "~/.config/example-tool").unwrap();
    let missing = file_plan_item(&cfg, "~/.missing/item").unwrap();
    assert_eq!((file_item.status.as_str(), file_item.kind.as_str()), ("ready", "file"));
    assert_eq!(file_item.destination, "/home/user/.config/example/settings.json");
    assert_eq!(
        (dir_item.status.as_str(), dir_item.kind.as_str()),
        ("ready", "directory")
    );
    assert_eq!(dir_item.destination, "/home/user/.config/example-tool/");
    assert_eq!(mismatch.status, "type mismatch");
    assert_eq!(missing.status, "missing");
}

#[test]
fn missing_selected_source_blocks_setup_before_ssh() {
    let home = tempfile::tempdir().unwrap();
    let mut cfg = settings(home.path());
    cfg.setup = parse_setup(
        &"[[items]]\nid = \"missing-file\"\ngroup = \"files\"\nhandler = \"file\"\nlabel = \"Missing file\"\ndescription = \"Missing file\"\npath = \"~/missing-fixture\"\n"
            .parse()
            .unwrap(),
    )
    .unwrap();
    let selected: Vec<Entry> = cfg
        .setup
        .items
        .iter()
        .filter(|e| e.id == "missing-file")
        .cloned()
        .collect();
    let error = build_plan(&cfg, &target(), &selected, None, false, true).unwrap_err();
    assert!(error.0.contains("missing"), "{}", error.0);
}
