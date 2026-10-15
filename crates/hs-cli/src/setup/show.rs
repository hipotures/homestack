//! What `homestack setup` prints (Python `setup_cli.show_*`): Rich's
//! default table, with a heavy head and cells wrapped to the terminal.

use serde_json::Value;

use super::Settings;
use super::catalog::Catalog;
use super::plan::Plan;
use crate::view::{Color, Style, visible_width};

/// Rich's `Table()` with `box.HEAVY_HEAD`.
pub struct BoxTable {
    pub title: Option<String>,
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

impl BoxTable {
    #[must_use]
    pub fn new(headers: &[&str]) -> Self {
        Self {
            title: None,
            headers: headers.iter().map(|h| (*h).to_owned()).collect(),
            rows: Vec::new(),
        }
    }

    pub fn row(&mut self, cells: Vec<String>) {
        self.rows.push(cells);
    }

    /// The table's lines, wrapped to `width` columns.
    #[must_use]
    pub fn render(&self, style: &Style, width: usize) -> String {
        let columns = self.headers.len();
        let longest_word = |text: &str| text.split_whitespace().map(visible_width).max().unwrap_or(0);
        let mut widths: Vec<usize> = self.headers.iter().map(|h| visible_width(h)).collect();
        let mut minimum: Vec<usize> = self.headers.iter().map(|h| longest_word(h).max(1)).collect();
        for row in &self.rows {
            for (index, cell) in row.iter().enumerate().take(columns) {
                widths[index] = widths[index].max(cell.lines().map(visible_width).max().unwrap_or(0));
                minimum[index] = minimum[index].max(longest_word(cell).min(10));
            }
        }
        // Borders and one space of padding on each side of every cell.
        let frame = columns * 3 + 1;
        let available = width.saturating_sub(frame).max(columns);
        while widths.iter().sum::<usize>() > available {
            let Some((index, _)) = widths
                .iter()
                .enumerate()
                .filter(|(i, w)| **w > minimum[*i].max(1))
                .max_by_key(|(_, w)| **w)
            else {
                break;
            };
            widths[index] -= 1;
        }
        let rule = |left: &str, line: &str, middle: &str, right: &str| {
            let parts: Vec<String> = widths.iter().map(|w| line.repeat(w + 2)).collect();
            format!("{left}{}{right}", parts.join(middle))
        };
        let mut lines = Vec::new();
        let total = widths.iter().sum::<usize>() + frame;
        if let Some(title) = &self.title {
            let pad = total.saturating_sub(visible_width(title)) / 2;
            lines.push(format!("{}{}", " ".repeat(pad), style.paint(title, Color::Italic)));
        }
        lines.push(rule("┏", "━", "┳", "┓"));
        let header: Vec<Vec<String>> = self
            .headers
            .iter()
            .enumerate()
            .map(|(i, h)| wrap(h, widths[i]))
            .collect();
        push_row(&mut lines, &header, &widths, "┃", style, true);
        lines.push(rule("┡", "━", "╇", "┩"));
        for row in &self.rows {
            let cells: Vec<Vec<String>> = (0..columns)
                .map(|i| wrap(row.get(i).map_or("", String::as_str), widths[i]))
                .collect();
            push_row(&mut lines, &cells, &widths, "│", style, false);
        }
        lines.push(rule("└", "─", "┴", "┘"));
        lines.join("\n")
    }
}

fn push_row(lines: &mut Vec<String>, cells: &[Vec<String>], widths: &[usize], bar: &str, style: &Style, bold: bool) {
    let height = cells.iter().map(Vec::len).max().unwrap_or(1).max(1);
    for line in 0..height {
        let mut out = String::from(bar);
        for (index, cell) in cells.iter().enumerate() {
            let text = cell.get(line).map_or("", String::as_str);
            let painted = if bold {
                style.paint(text, Color::Bold)
            } else {
                text.to_owned()
            };
            let pad = widths[index].saturating_sub(visible_width(text));
            out.push_str(&format!(" {painted}{} {bar}", " ".repeat(pad)));
        }
        lines.push(out);
    }
}

/// `text` wrapped at spaces into lines of at most `width` columns; a word
/// longer than that is cut.
#[must_use]
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut current = String::new();
        for word in paragraph.split(' ') {
            let mut word = word.to_owned();
            loop {
                let needed = visible_width(&current) + usize::from(!current.is_empty()) + visible_width(&word);
                if needed <= width {
                    if !current.is_empty() {
                        current.push(' ');
                    }
                    current.push_str(&word);
                    break;
                }
                if !current.is_empty() {
                    lines.push(std::mem::take(&mut current));
                    continue;
                }
                let head: String = word.chars().take(width).collect();
                word = word.chars().skip(width).collect();
                lines.push(head);
                if word.is_empty() {
                    break;
                }
            }
        }
        lines.push(current);
    }
    lines
}

fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        other => other.to_string(),
    }
}

/// `setup list` (Python `show_catalog`).
#[must_use]
pub fn catalog(settings: &Settings, catalog: &Catalog, style: &Style) -> String {
    let mut table = BoxTable::new(&[
        "Group",
        "Index",
        "ID",
        "Label",
        "Path / repository",
        "Availability / timestamps",
    ]);
    table.title = Some(format!(
        "Setup catalog {}",
        catalog.snapshot_id.as_deref().unwrap_or("None")
    ));
    let rows = catalog.rows(settings);
    for row in &rows {
        let mut availability = text(&row["availability"]);
        if row.get("created_at").is_some() {
            availability.push_str(&format!(
                "; created {}; pushed {}",
                text(&row["created_at"]),
                text(&row["pushed_at"])
            ));
        }
        table.row(vec![
            text(&row["group"]),
            text(&row["index"]),
            text(&row["id"]),
            text(&row["label"]),
            text(&row["path_or_repository"]),
            availability,
        ]);
    }
    for group in &settings.setup.groups {
        if !rows.iter().any(|row| row["group"] == Value::String(group.id.clone())) {
            table.row(vec![
                group.label.clone(),
                "—".to_owned(),
                "—".to_owned(),
                "No entries".to_owned(),
                String::new(),
                "Not configured or discovery unavailable".to_owned(),
            ]);
        }
    }
    let mut out = vec![table.render(style, style.width)];
    if !catalog.repository_error.is_empty() {
        out.push(catalog.repository_error.clone());
    }
    for row in &rows {
        for config in row["config_files"].as_array().into_iter().flatten() {
            out.push(format!(
                "{}: {} ({})",
                text(&row["id"]),
                text(&config["path"]),
                text(&config["format"])
            ));
            let selectors: Vec<String> = config["selectors"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|selector| format!("{}={}", text(&row["group"]), text(selector)))
                .collect();
            out.push(format!("  Selectors: {}", selectors.join(", ")));
            let values = settings
                .setup
                .items
                .iter()
                .chain(&catalog.entries)
                .find(|e| Value::String(e.id.clone()) == row["id"])
                .and_then(|e| e.application())
                .and_then(|a| {
                    a.config_files
                        .iter()
                        .find(|c| Value::String(c.id.clone()) == config["id"])
                })
                .map_or_else(
                    || python_json(&config["values"]),
                    |c| python_json_toml(&toml::Value::Table(c.values.clone())),
                );
            out.push(format!("  Desired values: {values}"));
        }
        for file in row["files"].as_array().into_iter().flatten() {
            out.push(format!(
                "{}: {} (desktop copy, {})  Selector: {}={}",
                text(&row["id"]),
                text(&file["path"]),
                text(&file["availability"]),
                text(&row["group"]),
                text(&file["selector"])
            ));
        }
    }
    out.push(
        "Guest installation state is unknown. Numeric selectors reuse this snapshot; use stable IDs in long-lived scripts."
            .to_owned(),
    );
    out.join("\n") + "\n"
}

/// `json.dumps(value, ensure_ascii=False)`: `", "` and `": "` separators.
#[must_use]
pub fn python_json(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let items: Vec<String> = map
                .iter()
                .map(|(key, value)| format!("{}: {}", Value::String(key.clone()), python_json(value)))
                .collect();
            format!("{{{}}}", items.join(", "))
        }
        Value::Array(items) => format!("[{}]", items.iter().map(python_json).collect::<Vec<_>>().join(", ")),
        other => other.to_string(),
    }
}

/// [`python_json`] of a TOML value, in the file's key order.
#[must_use]
pub fn python_json_toml(value: &toml::Value) -> String {
    match value {
        toml::Value::Table(table) => {
            let items: Vec<String> = table
                .iter()
                .map(|(key, value)| format!("{}: {}", Value::String(key.clone()), python_json_toml(value)))
                .collect();
            format!("{{{}}}", items.join(", "))
        }
        toml::Value::Array(items) => {
            format!(
                "[{}]",
                items.iter().map(python_json_toml).collect::<Vec<_>>().join(", ")
            )
        }
        other => python_json(&super::json_of(other)),
    }
}

/// The plan before it is applied (Python `show_plan`).
#[must_use]
pub fn plan(plan: &Plan, style: &Style) -> String {
    let label = if plan.target.local {
        "local".to_owned()
    } else {
        format!("VM {}", plan.target.vmid)
    };
    let mut out = vec![format!(
        "Setup: {} ({label}) — installation state unknown",
        plan.target.name
    )];
    let mut table = BoxTable::new(&[
        "Group",
        "ID",
        "Action",
        "Paths / prerequisites",
        "Desired",
        "Dependencies",
    ]);
    for action in plan.public()["actions"].as_array().into_iter().flatten() {
        let joined = |key: &str| {
            action[key]
                .as_array()
                .map(|items| items.iter().map(text).collect::<Vec<_>>().join(", "))
                .unwrap_or_default()
        };
        let location = [
            action.get("destination").map(text).unwrap_or_default(),
            joined("destinations"),
            action.get("repository").map(text).unwrap_or_default(),
            action.get("profile").map(text).unwrap_or_default(),
            joined("prerequisites"),
        ]
        .into_iter()
        .find(|text| !text.is_empty())
        .unwrap_or_default();
        let desired = if action.get("key").is_some() {
            format!("{} = {}", python_json(&action["key"]), python_json(&action["desired"]))
        } else {
            String::new()
        };
        let dependencies = joined("depends_on");
        table.row(vec![
            text(&action["group"]),
            text(&action["id"]),
            text(&action["label"]),
            location,
            desired,
            if dependencies.is_empty() {
                "none".to_owned()
            } else {
                dependencies
            },
        ]);
    }
    out.push(table.render(style, style.width));
    out.join("\n") + "\n"
}

fn timestamp_text(item: &Value, key: &str) -> String {
    let latest = item["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|file| file[key].as_i64())
        .max();
    let Some(nanos) = latest else {
        return "—".to_owned();
    };
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(nanos)).map_or_else(
        |_| "—".to_owned(),
        |when| {
            format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
                when.year(),
                u8::from(when.month()),
                when.day(),
                when.hour(),
                when.minute(),
                when.second()
            )
        },
    )
}

/// `setup status` (Python `show_workspace_setup_status`); `order` is the
/// order of the items' IDs, as Python's dict keeps it.
#[must_use]
pub fn status(target: &Value, state: &Value, order: &[String], style: &Style) -> String {
    let label = if target["local"] == Value::Bool(true) {
        "local".to_owned()
    } else {
        format!("VM {}", text(&target["vmid"]))
    };
    let ip = target["ip"].as_str().unwrap_or_default();
    let mut out = vec![
        format!("Setup status: {} ({label}) {ip}", text(&target["name"])),
        format!(
            "Checked: {}    Registry: {} ({})",
            text(&state["checked_at"]),
            text(&state["state_path"]),
            if state["registry_present"] == Value::Bool(true) {
                "present"
            } else {
                "not created"
            }
        ),
    ];
    let mut table = BoxTable::new(&[
        "Group",
        "Item",
        "State",
        "Installed",
        "First managed",
        "Last applied",
        "Created",
        "Modified",
    ]);
    let mut visible = 0;
    let items = state["items"].as_object().cloned().unwrap_or_default();
    let mut ordered: Vec<(&String, &Value)> = order.iter().filter_map(|id| items.get_key_value(id)).collect();
    ordered.extend(items.iter().filter(|(id, _)| !order.contains(id)));
    for (identity, item) in ordered {
        let mut label = item["label"]
            .as_str()
            .filter(|l| !l.is_empty())
            .unwrap_or(identity)
            .to_owned();
        let structured = item["handler"] == Value::String("structured".into())
            || (super::engine::truthy(&item["application"])
                && super::engine::truthy(&item["config_file"])
                && super::engine::truthy(&item["key"]));
        if structured {
            let key = match &item["key"] {
                Value::Array(parts) => parts.iter().map(text).collect::<Vec<_>>().join("."),
                other => text(other),
            };
            let application = item["application"].as_str().unwrap_or("application");
            label = format!("{application} {} {key}", text(&item["config_file"]));
        }
        let empty = [
            "absent",
            "not installed",
            "not configured",
            "shell missing",
            "unknown; no installation check",
        ]
        .contains(&item["state"].as_str().unwrap_or_default());
        if empty && !super::engine::truthy(&item["managed"]) && !super::engine::truthy(&item["exists"]) {
            continue;
        }
        visible += 1;
        let or_dash = |value: &Value| match value {
            Value::String(text) if !text.is_empty() => text.clone(),
            _ => "—".to_owned(),
        };
        table.row(vec![
            item["group"]
                .as_str()
                .or_else(|| item["handler"].as_str())
                .unwrap_or_default()
                .to_owned(),
            label,
            item["state"].as_str().unwrap_or("unknown").to_owned(),
            or_dash(&item["installed_at"]),
            or_dash(&item["first_managed_at"]),
            or_dash(&item["last_applied_at"]),
            timestamp_text(item, "birthtime_ns"),
            timestamp_text(item, "mtime_ns"),
        ]);
    }
    if visible > 0 {
        out.push(table.render(style, style.width));
    } else {
        out.push("No installed, configured, modified, or HomeStack-managed setup items were detected.".to_owned());
    }
    out.join("\n") + "\n"
}

/// A repository's state (Python `ui.show_repository_status`).
#[must_use]
pub fn repository(result: &Value, style: &Style) -> String {
    let field = |key: &str| crate::view::text(&result[key]);
    let status = result["status"].as_str().filter(|s| !s.is_empty()).unwrap_or("unknown");
    let status_text = match status {
        "ready" => style.paint("✓ ready", Color::Green),
        "not configured" => style.paint("not configured", Color::Dim),
        other => style.paint(other, Color::Yellow),
    };
    let mut deploy_text = result["deploy_key_state"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("missing")
        .to_owned();
    if let Some(id) = result["deploy_key_id"].as_i64() {
        deploy_text.push(' ');
        deploy_text.push_str(&style.paint(&format!("(ID {id})"), Color::Dim));
    }
    let tools = ["git", "ssh", "ssh-keygen"]
        .iter()
        .map(|name| {
            let present = result["tools"].get(*name).is_some_and(super::engine::truthy);
            format!("{name}: {}", if present { "yes" } else { "missing" })
        })
        .collect::<Vec<_>>()
        .join(", ");
    let row = |label: &str, value: String| (label.to_owned(), value);
    let mut sections = vec![
        vec![
            row("Workspace", field("workspace")),
            row("Repository", field("repository")),
            row("Status", status_text),
        ],
        vec![
            row("Checkout", field("checkout")),
            row("Checkout state", field("checkout_state")),
            row("Working tree", field("working_tree")),
            row("Origin", field("origin")),
        ],
        vec![
            row("Key", field("key_path")),
            row("Key state", field("key_state")),
            row("Fingerprint", field("key_fingerprint")),
            row("GitHub deploy key", deploy_text),
            row("Git access", field("access")),
            row("Repo SSH config", field("ssh_config_state")),
            row("Workspace tools", tools),
        ],
    ];
    if let Some(message) = result["message"].as_str().filter(|m| !m.is_empty()) {
        sections.push(vec![row("Result", message.to_owned())]);
    }
    let workspace = result["workspace"]
        .as_str()
        .filter(|w| !w.is_empty())
        .unwrap_or("workspace");
    crate::view::kv_panel(&format!("REPOSITORY — {workspace}"), &sections, style)
}
