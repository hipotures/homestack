//! The keys of the action box and how it follows the work (phase 13b,
//! step 2): which form each action has, what it sends, and how the box
//! moves through plan, run and result.

use crossterm::event::KeyCode;
use serde_json::{Value, json};

use super::action::{Action, Decision, Field, Form, Input, Kind, PlanFocus, Progress, Stage, Start};
use super::state::{App, Request};
use crate::app::{is_vmid, valid_name};

const DEFAULT: &str = "default";
const EXISTING_KEYS: &str = "the keys already in sshkeys";

impl App {
    /// The desktop's public keys, offered by add key.
    pub fn set_keys(&mut self, keys: Vec<String>) {
        self.keys = keys;
    }

    /// The storages of `node` the status lists, in order, once each.
    fn storages_on(&self, node: &str) -> Vec<String> {
        let mut storages: Vec<String> = Vec::new();
        if let Some(status) = &self.status {
            for layout in &status.summary.storage_layouts {
                if layout.node == node && !storages.contains(&layout.storage) {
                    storages.push(layout.storage.clone());
                }
            }
        }
        storages
    }

    /// The first VMID of the workspaces' range that no workspace and not
    /// Gold has (owner's decision of 2026-10-14: 101010–101199).
    fn free_vmid(&self) -> String {
        let used: Vec<u32> = self
            .status
            .iter()
            .flat_map(|status| {
                status
                    .workspaces
                    .iter()
                    .map(|entry| entry.vmid)
                    .chain(status.gold.as_ref().map(|gold| gold.vmid))
            })
            .collect();
        (101_010..=101_199)
            .find(|vmid| !used.contains(vmid))
            .map(|vmid| vmid.to_string())
            .unwrap_or_default()
    }

    fn gold_node(&self) -> Option<String> {
        self.status.as_ref()?.gold.as_ref().map(|gold| gold.node.clone())
    }

    /// Open the box of the action of key `c`, or start it when it has no
    /// form.
    pub(super) fn open_action(&mut self, c: char) -> Option<Request> {
        let kind = match c {
            'n' => Kind::New,
            'r' => Kind::Refresh,
            'z' => Kind::Resize,
            'k' => Kind::Addkey,
            'g' => Kind::Migrate,
            _ => Kind::Destroy,
        };
        if let Some(action) = &self.action {
            let line = format!("one action at a time: {} is still open", action.title());
            self.log("", &line);
            return None;
        }
        if self.status.is_none() || !self.unreachable.is_empty() || self.notice.is_some() {
            return None;
        }
        let entry = self.selected_entry().cloned();
        if kind != Kind::New && entry.is_none() {
            return None;
        }
        let vmid = entry.as_ref().map(|entry| entry.vmid);
        let name = entry.as_ref().and_then(|entry| entry.name.clone()).unwrap_or_default();
        let fields = match kind {
            Kind::New => {
                let gold = self.gold_node().unwrap_or_default();
                let mut nodes = vec![format!("{DEFAULT}: {gold}, the node of Gold")];
                nodes.extend(self.nodes());
                let storage_node = self.gold_node().unwrap_or_default();
                let mut storages = vec![format!("{DEFAULT}: {}", self.storages_on(&storage_node).join(", "))];
                storages.extend(self.storages_on(&storage_node));
                vec![
                    Field::text(
                        "VMID",
                        &self.free_vmid(),
                        "The first free VMID of the workspaces (101010-101199); it is also the address: 101010 is .101.10.",
                    ),
                    Field::text(
                        "Name",
                        "",
                        "A letter first, then letters, digits, '.' or '-'; it is also the SSH alias.",
                    ),
                    Field::choice("Node", nodes, "Where the VM is made; ← → to choose."),
                    Field::choice("Storage", storages, "Where its disks are made; ← → to choose."),
                    Field::text(
                        "Home size",
                        "",
                        "Persistent home in G or T, e.g. 64G or 1T; empty: the server's default.",
                    ),
                ]
            }
            Kind::Resize => vec![
                Field::choice("Disk", vec!["home".to_owned(), "root".to_owned()], "Disks only grow."),
                Field::text("New size", "", "The total size, e.g. 96G; the filesystem grows online."),
            ],
            Kind::Addkey => {
                let mut keys = vec![EXISTING_KEYS.to_owned()];
                keys.extend(self.keys.iter().cloned());
                vec![Field::choice(
                    "Key",
                    keys,
                    "One public key file with a comment, or the keys already in sshkeys.",
                )]
            }
            Kind::Migrate => {
                let here = entry.as_ref().map(|entry| entry.node.clone()).unwrap_or_default();
                let nodes: Vec<String> = self.nodes().into_iter().filter(|node| *node != here).collect();
                let storages = nodes.first().map(|node| self.storages_on(node)).unwrap_or_default();
                vec![
                    Field::choice("Node", nodes, "Offline: root, home and the cloud-init disk move."),
                    Field::choice("Storage", storages, "The storage on the target node."),
                ]
            }
            Kind::Refresh | Kind::Destroy => Vec::new(),
        };
        let mut action = Action {
            kind,
            vmid,
            name,
            stage: Stage::Asking,
            hidden: false,
        };
        if fields.is_empty() {
            let start = Start {
                kind,
                vmid,
                parameters: json!({"vmid": vmid}),
                key_file: None,
            };
            self.action = Some(action);
            return Some(Request::Act(start));
        }
        action.stage = Stage::Form(Form {
            fields,
            focus: 0,
            error: None,
        });
        self.action = Some(action);
        None
    }

    /// What the form sends, or what is wrong with it.
    fn start_of(&self, action: &Action, form: &Form) -> Result<Start, String> {
        let value = |label: &str| {
            form.fields
                .iter()
                .find(|field| field.label == label)
                .map(super::action::Field::value)
                .unwrap_or_default()
        };
        let vmid = action.vmid;
        let mut key_file = None;
        let parameters: Value = match action.kind {
            Kind::New => {
                let id = value("VMID");
                if !is_vmid(&id) {
                    return Err("The VMID is a number, e.g. 213.".to_owned());
                }
                let name = value("Name");
                if !valid_name(&name) {
                    return Err(format!(
                        "Invalid name {name:?}: the first character is a letter, then letters, digits, '.' or '-'."
                    ));
                }
                let mut parameters = json!({"vmid": id.parse::<u64>().unwrap_or(0), "name": name});
                for (label, key) in [("Node", "node"), ("Storage", "storage")] {
                    let chosen = value(label);
                    if !chosen.starts_with(DEFAULT) && !chosen.is_empty() {
                        parameters[key] = json!(chosen);
                    }
                }
                let home = value("Home size");
                if !home.is_empty() {
                    parameters["home_size"] = json!(home);
                }
                parameters
            }
            Kind::Resize => {
                let size = value("New size");
                if size.is_empty() {
                    return Err("Write the new total size, e.g. 96G.".to_owned());
                }
                let key = if value("Disk") == "root" {
                    "root_size"
                } else {
                    "home_size"
                };
                json!({"vmid": vmid, key: size})
            }
            Kind::Addkey => {
                let chosen = value("Key");
                if chosen != EXISTING_KEYS {
                    key_file = Some(chosen);
                }
                json!({"vmid": vmid, "restart": false})
            }
            Kind::Migrate => {
                let node = value("Node");
                let storage = value("Storage");
                if node.is_empty() || storage.is_empty() {
                    return Err("There is no other node with a storage for workspace disks.".to_owned());
                }
                json!({"vmid": vmid, "target_node": node, "target_storage": storage})
            }
            Kind::Refresh | Kind::Destroy => json!({"vmid": vmid}),
        };
        Ok(Start {
            kind: action.kind,
            vmid,
            parameters,
            key_file,
        })
    }

    /// A choice moved: the storages follow the node.
    fn follow_choice(&self, kind: Kind, form: &mut Form) {
        let node = form
            .fields
            .iter()
            .find(|field| field.label == "Node")
            .map(super::action::Field::value)
            .unwrap_or_default();
        let node = if node.starts_with(DEFAULT) {
            self.gold_node().unwrap_or_default()
        } else {
            node
        };
        let mut storages = Vec::new();
        if kind == Kind::New {
            storages.push(format!("{DEFAULT}: {}", self.storages_on(&node).join(", ")));
        }
        storages.extend(self.storages_on(&node));
        if let Some(field) = form.fields.iter_mut().find(|field| field.label == "Storage")
            && let Input::Choice { options, index } = &mut field.input
            && *options != storages
        {
            *options = storages;
            *index = 0;
        }
    }

    /// A key while the box is open.
    pub(super) fn action_key(&mut self, code: KeyCode) -> Option<Request> {
        let mut action = self.action.take()?;
        let request = match &mut action.stage {
            Stage::Form(form) => {
                let mut form = form.clone();
                let request = self.form_key(&mut action, &mut form, code);
                if matches!(action.stage, Stage::Form(_)) {
                    action.stage = Stage::Form(form);
                }
                if code == KeyCode::Esc {
                    return None;
                }
                request
            }
            Stage::Asking => {
                if code == KeyCode::Esc {
                    return Some(Request::Decide(Decision::Cancel));
                }
                None
            }
            Stage::Plan {
                lines,
                blocked,
                secret,
                focus,
                scroll,
                ..
            } => {
                let can_apply = blocked.is_none();
                match (code, *focus) {
                    (KeyCode::Esc, _) => return Some(Request::Decide(Decision::Cancel)),
                    (KeyCode::Char(c), PlanFocus::Secret) => {
                        if let Some((_, typed)) = secret.as_mut() {
                            typed.push(c);
                        }
                        None
                    }
                    (KeyCode::Backspace, PlanFocus::Secret) => {
                        if let Some((_, typed)) = secret.as_mut() {
                            typed.pop();
                        }
                        None
                    }
                    (KeyCode::Tab | KeyCode::Left | KeyCode::Right | KeyCode::BackTab, _) => {
                        let order: Vec<PlanFocus> = [
                            (secret.is_some(), PlanFocus::Secret),
                            (can_apply, PlanFocus::Apply),
                            (true, PlanFocus::Cancel),
                        ]
                        .into_iter()
                        .filter(|(on, _)| *on)
                        .map(|(_, focus)| focus)
                        .collect();
                        let at = order.iter().position(|item| item == focus).unwrap_or(0);
                        let back = matches!(code, KeyCode::Left | KeyCode::BackTab);
                        let next = if back { at + order.len() - 1 } else { at + 1 } % order.len();
                        *focus = order[next];
                        None
                    }
                    (KeyCode::Up, _) => {
                        *scroll = scroll.saturating_sub(1);
                        None
                    }
                    (KeyCode::Down, _) => {
                        *scroll = (*scroll + 1).min(lines.len().saturating_sub(1));
                        None
                    }
                    (KeyCode::Char('a'), _) | (KeyCode::Enter, PlanFocus::Apply | PlanFocus::Secret) if can_apply => {
                        match secret {
                            // An empty passphrase is never sent.
                            Some((_, typed)) if typed.is_empty() => None,
                            Some((_, typed)) => {
                                let secrets = json!({"luks_passphrase": typed.clone()});
                                action.stage = Stage::Running { steps: Vec::new() };
                                Some(Request::Decide(Decision::Apply(Some(secrets))))
                            }
                            None => {
                                action.stage = Stage::Running { steps: Vec::new() };
                                Some(Request::Decide(Decision::Apply(None)))
                            }
                        }
                    }
                    (KeyCode::Enter, _) => return Some(Request::Decide(Decision::Cancel)),
                    _ => None,
                }
            }
            Stage::Running { .. } => {
                if code == KeyCode::Esc {
                    action.hidden = true;
                }
                None
            }
            Stage::Question { yes, .. } => match code {
                KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                    *yes = !*yes;
                    None
                }
                KeyCode::Enter => {
                    let answer = *yes;
                    action.stage = Stage::Running { steps: Vec::new() };
                    Some(Request::Decide(Decision::Restart(answer)))
                }
                KeyCode::Esc => {
                    action.stage = Stage::Running { steps: Vec::new() };
                    Some(Request::Decide(Decision::Restart(false)))
                }
                _ => None,
            },
            Stage::Done { .. } => match code {
                KeyCode::Enter | KeyCode::Esc => return Some(Request::Reload),
                _ => None,
            },
        };
        self.action = Some(action);
        request
    }

    fn form_key(&self, action: &mut Action, form: &mut Form, code: KeyCode) -> Option<Request> {
        let count = form.fields.len();
        match code {
            KeyCode::Tab | KeyCode::Down => form.focus = (form.focus + 1) % count,
            KeyCode::BackTab | KeyCode::Up => form.focus = (form.focus + count - 1) % count,
            KeyCode::Left | KeyCode::Right => {
                if let Some(Field {
                    input: Input::Choice { options, index },
                    ..
                }) = form.fields.get_mut(form.focus)
                    && !options.is_empty()
                {
                    let size = options.len();
                    *index = if code == KeyCode::Right {
                        (*index + 1) % size
                    } else {
                        (*index + size - 1) % size
                    };
                }
                self.follow_choice(action.kind, form);
            }
            KeyCode::Char(c) => {
                if let Some(Field {
                    input: Input::Text(text),
                    ..
                }) = form.fields.get_mut(form.focus)
                {
                    text.push(c);
                }
            }
            KeyCode::Backspace => {
                if let Some(Field {
                    input: Input::Text(text),
                    ..
                }) = form.fields.get_mut(form.focus)
                {
                    text.pop();
                }
            }
            KeyCode::Enter => match self.start_of(action, form) {
                Ok(start) => {
                    action.stage = Stage::Asking;
                    return Some(Request::Act(start));
                }
                Err(message) => form.error = Some(message),
            },
            _ => {}
        }
        None
    }

    /// What the work said.
    pub fn progress(&mut self, progress: Progress) {
        let Some(action) = self.action.as_mut() else {
            return;
        };
        action.stage = match progress {
            Progress::Plan {
                lines,
                question,
                blocked,
                secret,
            } => {
                let focus = if secret.is_some() {
                    PlanFocus::Secret
                } else if action.kind == Kind::Destroy || blocked.is_some() {
                    PlanFocus::Cancel
                } else {
                    PlanFocus::Apply
                };
                Stage::Plan {
                    lines,
                    question,
                    blocked,
                    secret: secret.map(|prompt| (prompt, String::new())),
                    focus,
                    scroll: 0,
                }
            }
            Progress::Steps(steps) => Stage::Running { steps },
            Progress::Question { lines } => {
                action.hidden = false;
                Stage::Question { lines, yes: false }
            }
            Progress::Done { ok, lines } => {
                action.hidden = false;
                Stage::Done { ok, lines }
            }
        };
    }
}
