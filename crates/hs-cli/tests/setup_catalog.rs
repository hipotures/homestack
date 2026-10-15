//! The catalog and its selectors (Python `test_setup.py::CatalogTests`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
mod setup_fixture;

use std::sync::{Arc, Mutex};

use common::FakeSystem;
use hs_cli::AppError;
use hs_cli::setup::catalog::{
    Catalog, Github, discover_repositories, load_catalog, parse_assignments, save_snapshot, select_entries,
    snapshot_directory,
};
use hs_cli::setup::definitions::{CODEX_RECIPE, Entry, Params, defaults, parse_setup};
use hs_cli::system::Output;
use serde_json::{Value, json};
use setup_fixture::settings;

fn identity() -> Value {
    json!({"host": "github.com", "account": "owner", "account_id": 42, "owner": "owner"})
}

fn owner_settings(home: &std::path::Path) -> hs_cli::setup::Settings {
    let mut settings = settings(home);
    settings.repo.owner = Some("owner".into());
    settings
}

fn no_github() -> (impl Fn() -> Result<Value, AppError>, impl Fn() -> Catalog) {
    (
        || -> Result<Value, AppError> { panic!("unexpected GitHub") },
        || -> Catalog { panic!("unexpected GitHub") },
    )
}

fn tokens(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| (*word).to_owned()).collect()
}

fn repository(name: &str) -> Entry {
    Entry {
        id: name.into(),
        group: "repo".into(),
        handler: "repository".into(),
        label: name.into(),
        description: "Repository".into(),
        params: Params::Repository {
            repository: name.into(),
        },
        depends_on: Vec::new(),
    }
}

#[test]
fn alias_sets_conflicts_all_and_invalid_inputs() {
    let home = tempfile::tempdir().unwrap();
    let cfg = owner_settings(home.path());
    assert_eq!(
        parse_assignments(&tokens(&["f=0", "e=bash,bash", "env=bash"]), &cfg).unwrap(),
        vec![
            ("files".to_owned(), vec!["all".to_owned()]),
            ("env".to_owned(), vec!["bash".to_owned()])
        ]
    );
    assert_eq!(
        parse_assignments(&tokens(&["c=0"]), &cfg).unwrap(),
        vec![("cred".to_owned(), vec!["all".to_owned()])]
    );
    let (identity, catalog) = no_github();
    let github = Github {
        identity: &identity,
        catalog: &catalog,
    };
    for words in [
        &["x=0"][..],
        &["env=0,bash"],
        &["env=all,1"],
        &["env="],
        &["env=,bash"],
        &["env=bash", "e=zsh"],
        &["env"],
        &["app=x=y"],
    ] {
        assert!(
            select_entries(&cfg, &tokens(words), None, &github).is_err(),
            "{words:?}"
        );
    }
    for value in ["-1", "1.2", "missing"] {
        assert!(select_entries(&cfg, &tokens(&[&format!("env={value}")]), None, &github).is_err());
    }
}

#[test]
fn numeric_requires_snapshot_and_no_github_for_nonrepo() {
    let home = tempfile::tempdir().unwrap();
    let cfg = owner_settings(home.path());
    let (identity, catalog) = no_github();
    let github = Github {
        identity: &identity,
        catalog: &catalog,
    };
    let error = select_entries(&cfg, &tokens(&["env=1"]), None, &github).unwrap_err();
    assert!(error.0.contains("setup list"), "{}", error.0);
    let mut loaded = load_catalog(&cfg, None);
    let identifier = save_snapshot(&cfg, &mut loaded).unwrap();
    let (selected, used) = select_entries(&cfg, &tokens(&["e=1,2"]), Some(&identifier), &github).unwrap();
    assert_eq!(
        selected.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
        ["bash", "zsh"]
    );
    assert_eq!(used.as_deref(), Some(identifier.as_str()));
    let error = select_entries(&cfg, &tokens(&["env=99"]), None, &github).unwrap_err();
    assert!(error.0.contains("out of range"), "{}", error.0);
}

#[test]
fn paginated_private_repos_sorts_null_dates_and_ties() {
    let home = tempfile::tempdir().unwrap();
    let r = |name: &str, created: &str, pushed: Option<&str>| json!({"full_name": name, "created_at": created, "pushed_at": pushed, "private": true});
    let rows = json!([
        [
            r("owner/z", "2025-01-01T00:00:00Z", None),
            r("other/x", "2026-01-01T00:00:00Z", None)
        ],
        [
            r("owner/b", "2024-01-01T00:00:00Z", Some("2025-01-01T00:00:00Z")),
            r("owner/a", "2025-01-01T00:00:00Z", None)
        ],
    ]);
    for (sort, expected) in [
        ("recent", ["owner/a", "owner/b", "owner/z"]),
        ("created", ["owner/a", "owner/z", "owner/b"]),
        ("name", ["owner/a", "owner/b", "owner/z"]),
    ] {
        let calls: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
        let seen = Arc::clone(&calls);
        let answer = rows.clone();
        let system = FakeSystem::new().run(move |argv| {
            seen.lock().unwrap().push(argv.to_vec());
            let user = argv.last().is_some_and(|last| last == "user");
            Ok(Output::stdout(
                &if user {
                    json!({"login": "owner", "id": 42})
                } else {
                    answer.clone()
                }
                .to_string(),
            ))
        });
        let mut cfg = owner_settings(home.path());
        cfg.repo.sort = sort.into();
        let (_, repos) = discover_repositories(&system, &cfg).unwrap();
        let names: Vec<&str> = repos.iter().map(|r| r["full_name"].as_str().unwrap()).collect();
        assert_eq!(names, expected, "{sort}");
        assert!(repos.iter().all(|r| r["private"] == json!(true)));
        let last = calls.lock().unwrap().last().cloned().unwrap();
        assert!(last.contains(&"--paginate".to_owned()) && last.contains(&"--slurp".to_owned()));
    }
}

#[test]
fn immutable_snapshot_no_numeric_drift_and_bindings() {
    let home = tempfile::tempdir().unwrap();
    let cfg = owner_settings(home.path());
    let repositories = [repository("owner/first"), repository("owner/second")];
    let mut catalog = Catalog {
        entries: defaults().into_iter().chain(repositories.iter().cloned()).collect(),
        github: Some(identity()),
        ..Catalog::default()
    };
    let first = save_snapshot(&cfg, &mut catalog).unwrap();
    catalog.entries = defaults()
        .into_iter()
        .chain(repositories.iter().rev().cloned())
        .collect();
    let second = save_snapshot(&cfg, &mut catalog).unwrap();
    let identity_ok = || Ok(identity());
    let renumbered = || -> Catalog { panic!("renumbered") };
    let github = Github {
        identity: &identity_ok,
        catalog: &renumbered,
    };
    assert_eq!(
        select_entries(&cfg, &tokens(&["repo=1"]), Some(&first), &github)
            .unwrap()
            .0[0]
            .id,
        "owner/first"
    );
    assert_eq!(
        select_entries(&cfg, &tokens(&["repo=1"]), None, &github).unwrap().0[0].id,
        "owner/second"
    );
    assert_ne!(first, second);
    let altered = parse_setup(
        &"[[items]]\nid = \"catalog-file\"\ngroup = \"files\"\nhandler = \"file\"\nlabel = \"Catalog file\"\ndescription = \"Catalog file\"\npath = \"~/catalog-file\"\n"
            .parse()
            .unwrap(),
    )
    .unwrap();
    let mut changed_setup = cfg.clone();
    changed_setup.setup = altered;
    let mut changed_path = cfg.clone();
    changed_path.config_path = home.path().join("different.toml");
    for other in [&changed_setup, &changed_path] {
        assert!(select_entries(other, &tokens(&["env=1"]), Some(&first), &github).is_err());
    }
    let other_account = || Ok(json!({"host": "github.com", "account": "owner", "account_id": 9, "owner": "owner"}));
    let github = Github {
        identity: &other_account,
        catalog: &renumbered,
    };
    let error = select_entries(&cfg, &tokens(&["repo=1"]), Some(&first), &github).unwrap_err();
    assert!(error.0.contains("identity"), "{}", error.0);
    let text = std::fs::read_to_string(snapshot_directory(&cfg).join(format!("{first}.json"))).unwrap();
    assert!(!text.contains(CODEX_RECIPE));
    assert!(!text.contains("command"));
}

#[test]
fn discovery_failure_does_not_hide_other_groups() {
    let home = tempfile::tempdir().unwrap();
    let cfg = owner_settings(home.path());
    let fail =
        |_: &hs_cli::setup::Settings| -> Result<(Value, Vec<Value>), AppError> { Err(AppError::new("Unavailable")) };
    let catalog = load_catalog(&cfg, Some(&fail));
    assert_eq!(catalog.repository_error, "Unavailable");
    assert!(catalog.entries.iter().any(|e| e.id == "bash"));
    assert!(
        catalog
            .rows(&cfg)
            .iter()
            .all(|row| row["guest_state"] == json!("unknown"))
    );
}
