//! The Cloud-Init snippet update (Settings › General): root on one node
//! compares the vendor snippets on every node with this build's and writes
//! only those that differ or are missing. Nothing else in Proxmox changes.

use std::sync::mpsc::Receiver;

use hs_install::{Access, Root, SnippetState, SnippetStatus};

use super::install::{Event, Found, Mark};
use super::installing::{Command, Setup};

struct Session {
    root: Box<dyn Root>,
    access: Access,
    describe: String,
    storage: String,
    local: String,
    snippets: Vec<SnippetStatus>,
}

fn to_write(snippet: &SnippetStatus) -> bool {
    matches!(snippet.state, SnippetState::Differs | SnippetState::Missing)
}

fn check(session: &Session) -> Found {
    let check = session
        .snippets
        .iter()
        .map(|snippet| {
            let (mark, state) = match &snippet.state {
                SnippetState::Same => (Mark::Ok, "matches this build".to_owned()),
                SnippetState::Differs => (Mark::Add, "differs: will be written".to_owned()),
                SnippetState::Missing => (Mark::Add, "missing: will be written".to_owned()),
                SnippetState::Unknown(why) => (Mark::Note, format!("not checked: {why}")),
            };
            (mark, snippet.node.name.clone(), format!("{} {state}", snippet.name))
        })
        .collect();
    Found {
        root: session.describe.clone(),
        check,
        settings: Vec::new(),
        domain: None,
    }
}

fn read(session: &mut Session) -> Result<(), String> {
    let (local, snippets) = hs_install::snippet_status(session.root.as_mut(), &session.access, &session.storage)
        .map_err(|e| e.to_string())?;
    session.local = local;
    session.snippets = snippets;
    Ok(())
}

fn connect(setup: &Setup, method: super::install::Method, target: &str) -> Result<Session, String> {
    let storage = crate::install_flow::configured_snippet_storage(&setup.token_dir)
        .ok_or("The HomeStack configuration names no snippet storage.")?;
    let mut root = setup.open(method, target).map_err(|e| e.0)?;
    let access = hs_install::probe(root.as_mut()).map_err(|e| e.to_string())?;
    let describe = root.describe();
    let mut session = Session {
        root,
        access,
        describe,
        storage,
        local: String::new(),
        snippets: Vec::new(),
    };
    read(&mut session)?;
    Ok(session)
}

fn review(session: &Session) -> Vec<(String, Vec<String>)> {
    let writes: Vec<String> = session
        .snippets
        .iter()
        .filter(|snippet| to_write(snippet))
        .map(|snippet| format!("write {} on {}", snippet.name, snippet.node.name))
        .collect();
    let skipped: Vec<String> = session
        .snippets
        .iter()
        .filter_map(|snippet| match &snippet.state {
            SnippetState::Unknown(why) => Some(format!("{} on {}: {why}", snippet.name, snippet.node.name)),
            _ => None,
        })
        .collect();
    let mut sections = vec![(
        "Cloud-Init snippets".to_owned(),
        if writes.is_empty() {
            vec!["nothing to write: every checked snippet matches this build".to_owned()]
        } else {
            writes
        },
    )];
    if !skipped.is_empty() {
        sections.push(("Not checked (run this again when they answer)".to_owned(), skipped));
    }
    sections
}

fn write(session: &mut Session, tell: &(dyn Fn(Event) + Send + Sync)) -> (bool, Vec<String>) {
    let mut ok = true;
    let mut lines = Vec::new();
    let pending: Vec<SnippetStatus> = session.snippets.iter().filter(|s| to_write(s)).cloned().collect();
    for snippet in &pending {
        let what = format!("{} on {}", snippet.name, snippet.node.name);
        tell(Event::Busy(format!("Writing {what}")));
        let line = match hs_install::write_snippet(
            session.root.as_mut(),
            &session.local,
            &snippet.node,
            snippet.name,
            &snippet.path,
        ) {
            Ok(()) => format!("✓ wrote {what}"),
            Err(error) => {
                ok = false;
                format!("✗ {what}: {error}")
            }
        };
        tell(Event::Line(line.clone()));
        lines.push(line);
    }
    if pending.is_empty() {
        lines.push("Nothing was written: every checked snippet matches this build.".to_owned());
    }
    (ok, lines)
}

pub(super) fn work(setup: &Setup, commands: &Receiver<Command>, tell: &(dyn Fn(Event) + Send + Sync)) {
    let mut session: Option<Session> = None;
    while let Ok(command) = commands.recv() {
        match command {
            Command::Connect { method, target } => {
                tell(Event::Busy(format!(
                    "Connecting to {target} (touch your key or give its PIN if it asks)"
                )));
                match connect(setup, method, &target) {
                    Ok(started) => {
                        tell(Event::Connected(check(&started)));
                        session = Some(started);
                    }
                    Err(error) => tell(Event::Failed(error)),
                }
            }
            Command::Rescan { .. } => {
                let Some(s) = session.as_mut() else { continue };
                tell(Event::Busy("Comparing the snippets again".to_owned()));
                match read(s) {
                    Ok(()) => tell(Event::Connected(check(s))),
                    Err(error) => tell(Event::Failed(error)),
                }
            }
            Command::Review { .. } => {
                let Some(s) = session.as_ref() else { continue };
                tell(Event::Reviewed {
                    sections: review(s),
                    problems: Vec::new(),
                });
            }
            Command::Apply => {
                let Some(s) = session.as_mut() else { continue };
                let (ok, lines) = write(s, tell);
                tell(Event::Done { ok, lines });
            }
            Command::Remove { .. } | Command::PangolinDomains { .. } => {}
            Command::Cancel => return,
        }
    }
}
