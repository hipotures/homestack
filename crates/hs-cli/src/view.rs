//! The human output: the panels and tables of Python's `ui.py`, as plain
//! text with ANSI colours when the terminal takes them.

use serde_json::Value;

const NARROW_WIDTH: usize = 80;
const DASH: &str = "—";

/// How output is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub color: bool,
    pub width: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    Plain,
    White,
    Green,
    Yellow,
    Red,
    Dim,
    Grey,
    Title,
    Bold,
    Italic,
}

impl Color {
    const fn code(self) -> &'static str {
        match self {
            Self::Plain => "",
            Self::White => "37",
            Self::Green => "32",
            Self::Yellow => "33",
            Self::Red => "31",
            Self::Dim => "2",
            Self::Grey => "90",
            Self::Title => "2;94",
            Self::Bold => "1",
            Self::Italic => "3",
        }
    }
}

/// Text and its colour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Styled {
    pub text: String,
    pub color: Color,
}

impl Styled {
    #[must_use]
    pub fn new(text: impl Into<String>, color: Color) -> Self {
        Self {
            text: text.into(),
            color,
        }
    }

    #[must_use]
    pub fn plain(text: impl Into<String>) -> Self {
        Self::new(text, Color::Plain)
    }
}

impl Style {
    /// `text` in `color` (nothing added without colour).
    #[must_use]
    pub fn paint(&self, text: &str, color: Color) -> String {
        if !self.color || color == Color::Plain || text.is_empty() {
            return text.to_owned();
        }
        format!("\x1b[{}m{text}\x1b[0m", color.code())
    }

    fn styled(&self, value: &Styled) -> String {
        self.paint(&value.text, value.color)
    }
}

/// Visible width: ANSI sequences skipped, one cell per character.
#[must_use]
pub fn visible_width(text: &str) -> usize {
    let mut width = 0;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c == 'm' {
                    break;
                }
            }
        } else {
            width += 1;
        }
    }
    width
}

fn pad(text: &str, width: usize, right: bool) -> String {
    let fill = " ".repeat(width.saturating_sub(visible_width(text)));
    if right {
        format!("{fill}{text}")
    } else {
        format!("{text}{fill}")
    }
}

/// `str(value)` of a JSON value; `—` for null or "".
#[must_use]
pub fn text(value: &Value) -> String {
    match value {
        Value::Null => DASH.to_owned(),
        Value::String(text) if text.is_empty() => DASH.to_owned(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn integer(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64().or_else(|| number.as_f64().map(|f| f.trunc() as i64)),
        Value::String(text) => text.trim().parse().ok(),
        Value::Bool(flag) => Some(i64::from(*flag)),
        _ => None,
    }
}

/// Python's `%g` for the values HomeStack shows (sizes in GiB).
#[must_use]
pub fn compact(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        let text = format!("{value:.6}");
        text.trim_end_matches('0').trim_end_matches('.').to_owned()
    }
}

/// `16 GiB`, `20.5 GiB`, `—`.
#[must_use]
pub fn gib_value(value: &Value) -> String {
    value
        .as_f64()
        .map_or_else(|| DASH.to_owned(), |gib| format!("{} GiB", compact(gib)))
}

/// `1.5 GiB`; bytes below 1 KiB without decimals.
#[must_use]
pub fn human_bytes(value: i64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut size = value as f64;
    let mut unit = units[0];
    for (index, candidate) in units.iter().enumerate() {
        unit = candidate;
        if size < 1024.0 || index == units.len() - 1 {
            break;
        }
        size /= 1024.0;
    }
    if unit == "B" {
        format!("{} {unit}", size as i64)
    } else {
        format!("{size:.1} {unit}")
    }
}

#[must_use]
pub fn byte_value(value: &Value) -> String {
    integer(value).map_or_else(|| DASH.to_owned(), human_bytes)
}

/// Bytes in a fixed unit with `decimals` (`[display]`).
#[must_use]
pub fn byte_value_in_unit(value: &Value, unit: &str, decimals: usize) -> String {
    let Some(bytes) = integer(value) else {
        return DASH.to_owned();
    };
    let power = match unit {
        "KiB" => 1,
        "MiB" => 2,
        "GiB" => 3,
        "TiB" => 4,
        "PiB" => 5,
        _ => return DASH.to_owned(),
    };
    format!("{:.decimals$}", bytes as f64 / 1024_f64.powi(power))
}

fn gib_text(value: &Value, decimals: usize) -> String {
    integer(value).map_or_else(
        || DASH.to_owned(),
        |bytes| format!("{:.decimals$}", bytes as f64 / 1024_f64.powi(3)),
    )
}

/// Free share of `total`, 0–100 (Python `status.free_percent`).
#[must_use]
pub fn free_percent(total: Option<i64>, available: Option<i64>) -> Option<f64> {
    let (total, available) = (total?, available?);
    if total <= 0 {
        return None;
    }
    Some((100.0 * available as f64 / total as f64).clamp(0.0, 100.0))
}

/// Green up to 70 % used, yellow up to 90 %, red beyond.
#[must_use]
pub fn free_percent_color(value: Option<f64>) -> Color {
    let Some(free) = value else {
        return Color::White;
    };
    let used = 100.0 - free;
    if used <= 70.0 {
        Color::Green
    } else if used <= 90.0 {
        Color::Yellow
    } else {
        Color::Red
    }
}

#[must_use]
pub fn free_percent_text(value: Option<f64>, decimals: usize) -> Styled {
    match value {
        None => Styled::new(DASH, Color::White),
        Some(free) => Styled::new(format!("{free:.decimals$}%"), free_percent_color(value)),
    }
}

/// A table without borders, like Rich's `box=None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub headers: Vec<String>,
    pub right: Vec<bool>,
    pub show_header: bool,
    pub rows: Vec<Vec<Styled>>,
}

impl Table {
    #[must_use]
    pub fn new(headers: &[&str]) -> Self {
        Self {
            headers: headers.iter().map(|h| (*h).to_owned()).collect(),
            right: vec![false; headers.len()],
            show_header: true,
            rows: Vec::new(),
        }
    }

    #[must_use]
    pub fn right(mut self, columns: &[usize]) -> Self {
        for column in columns {
            if let Some(flag) = self.right.get_mut(*column) {
                *flag = true;
            }
        }
        self
    }

    pub fn row(&mut self, cells: Vec<Styled>) {
        self.rows.push(cells);
    }

    /// The table's lines joined, without a final newline.
    #[must_use]
    pub fn render(&self, style: &Style) -> String {
        let mut widths: Vec<usize> = self
            .headers
            .iter()
            .map(|h| if self.show_header { visible_width(h) } else { 0 })
            .collect();
        for row in &self.rows {
            for (index, cell) in row.iter().enumerate() {
                if let Some(width) = widths.get_mut(index) {
                    *width = (*width).max(visible_width(&cell.text));
                }
            }
        }
        let mut lines = Vec::new();
        if self.show_header {
            let cells: Vec<String> = self
                .headers
                .iter()
                .enumerate()
                .map(|(i, h)| pad(&style.paint(h, Color::Bold), widths[i], self.right[i]))
                .collect();
            lines.push(cells.join(" ").trim_end().to_owned());
        }
        for row in &self.rows {
            let cells: Vec<String> = row
                .iter()
                .enumerate()
                .map(|(i, cell)| {
                    pad(
                        &style.styled(cell),
                        widths.get(i).copied().unwrap_or(0),
                        self.right.get(i).copied().unwrap_or(false),
                    )
                })
                .collect();
            lines.push(cells.join(" ").trim_end().to_owned());
        }
        lines.join("\n")
    }
}

/// A box around `body` with `title` centred in its top edge.
#[must_use]
pub fn panel(title: &str, body: &[String], style: &Style, padding: (usize, usize)) -> String {
    let (vertical, horizontal) = padding;
    let inner = body
        .iter()
        .map(|line| visible_width(line))
        .max()
        .unwrap_or(0)
        .max(visible_width(title) + 2)
        + 2 * horizontal;
    let border = |text: &str| style.paint(text, Color::Grey);
    let title_text = format!(" {title} ");
    let left = (inner.saturating_sub(visible_width(&title_text))) / 2;
    let right = inner.saturating_sub(visible_width(&title_text) + left);
    let mut lines = vec![format!(
        "{}{}{}",
        border(&format!("╭{}", "─".repeat(left))),
        style.paint(&title_text, Color::Title),
        border(&format!("{}╮", "─".repeat(right)))
    )];
    let blank = format!("{}{}{}", border("│"), " ".repeat(inner), border("│"));
    lines.extend(std::iter::repeat_n(blank.clone(), vertical));
    for line in body {
        let margin = " ".repeat(horizontal);
        lines.push(format!(
            "{}{margin}{}{margin}{}",
            border("│"),
            pad(line, inner - 2 * horizontal, false),
            border("│")
        ));
    }
    lines.extend(std::iter::repeat_n(blank, vertical));
    lines.push(border(&format!("╰{}╯", "─".repeat(inner))));
    lines.join("\n") + "\n"
}

/// Labelled values in sections divided by a rule (Python `show_kv_panel`).
#[must_use]
pub fn kv_panel(title: &str, sections: &[Vec<(String, String)>], style: &Style) -> String {
    let rows: Vec<&(String, String)> = sections.iter().flatten().collect();
    if rows.is_empty() {
        return String::new();
    }
    let key_width = rows.iter().map(|(label, _)| visible_width(label)).max().unwrap_or(0);
    let value_width = rows
        .iter()
        .flat_map(|(_, value)| value.lines().map(visible_width).collect::<Vec<_>>())
        .max()
        .unwrap_or(0);
    let mut body = Vec::new();
    for (index, section) in sections.iter().enumerate() {
        for (label, value) in section {
            let mut lines = value.lines();
            let first = lines.next().unwrap_or_default();
            body.push(format!(
                "{}{}{first}",
                style.paint(&pad(label, key_width, false), Color::Grey),
                style.paint(" : ", Color::Grey)
            ));
            for more in lines {
                body.push(format!("{}{more}", " ".repeat(key_width + 3)));
            }
        }
        if index + 1 != sections.len() && !section.is_empty() {
            body.push(style.paint(&"-".repeat(key_width + 3 + value_width), Color::Grey));
        }
    }
    panel(title, &body, style, (1, 2))
}

fn row(label: &str, value: impl Into<String>) -> (String, String) {
    (label.to_owned(), value.into())
}

/// An error, its text shown as written.
#[must_use]
pub fn error(message: &str, style: &Style) -> String {
    let lines: Vec<String> = message.lines().map(str::to_owned).collect();
    panel(&style.paint("ERROR", Color::Red), &lines, style, (0, 1))
}

fn check(style: &Style, value: &Value) -> String {
    match value {
        Value::Bool(true) => style.paint("✓ yes", Color::Green),
        Value::Bool(false) => style.paint("no", Color::Dim),
        _ => style.paint("unknown", Color::Dim),
    }
}

fn vm_status(style: &Style, value: &Value) -> String {
    match value.as_str().unwrap_or("unknown") {
        "running" => style.paint("● running", Color::Green),
        "stopped" => style.paint("○ stopped", Color::Dim),
        "absent" => style.paint("absent", Color::Dim),
        other => other.to_owned(),
    }
}

fn dim_value(style: &Style, value: &Value) -> String {
    let shown = text(value);
    if shown == DASH {
        style.paint(DASH, Color::Dim)
    } else {
        shown
    }
}

fn root_disk(plan: &Value, style: &Style, suffix: &str) -> String {
    match plan["root_disk_gb"].as_f64() {
        Some(gb) => format!("{}G {}", compact(gb), style.paint(suffix, Color::Dim)),
        None => style.paint(suffix, Color::Dim),
    }
}

fn transfer(plan: &Value) -> &'static str {
    match plan["transfer_method"].as_str() {
        Some("clone-migrate") => "Clone on Gold's node, then migrate offline to the target",
        Some("stream") => "Stream to target; no source disk copy",
        _ => "Local clone",
    }
}

fn key_labels(value: &Value, style: &Style) -> String {
    let lines: Vec<String> = value
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| match item {
            Value::Object(_) => {
                let label = text(&item["label"]);
                if item["verified"] == true {
                    format!("{} {label}", style.paint("✓", Color::Green))
                } else {
                    format!("{} {label}", style.paint("✗", Color::Red))
                }
            }
            other => text(other),
        })
        .collect();
    if lines.is_empty() {
        style.paint("none", Color::Dim)
    } else {
        lines.join("\n")
    }
}

/// Python `show_create_plan`.
#[must_use]
pub fn create_plan(plan: &Value, style: &Style) -> String {
    let sections = vec![
        vec![
            row("VMID", text(&plan["vmid"])),
            row("Name", text(&plan["name"])),
            row("Node", text(&plan["node"])),
            row(
                "Source",
                format!("Gold VM {} on {}", text(&plan["gold_vmid"]), text(&plan["source_node"])),
            ),
            row(
                "Clone",
                format!("FULL {}", style.paint("independent RW root", Color::Dim)),
            ),
            row("Transfer", transfer(plan)),
        ],
        vec![
            row("IP", format!("{}/{}", text(&plan["ip"]), text(&plan["cidr"]))),
            row("Gateway", text(&plan["gateway"])),
        ],
        vec![
            row("Root storage", text(&plan["root_storage"])),
            row("Root disk", root_disk(plan, style, "inherited from Gold")),
            row("Pool", text(&plan["pool"])),
        ],
        vec![
            row(
                "Persistent home",
                format!("{} on {}", text(&plan["home_disk"]), text(&plan["home_storage"])),
            ),
            row("Home size", text(&plan["home_size"])),
            row("Filesystem label", text(&plan["home_label"])),
            row("Home volume", text(&plan["home_volume_name"])),
        ],
        vec![
            row("SSH public keys", key_labels(&plan["ssh_public_keys"], style)),
            row("Key source", style.paint(&text(&plan["ssh_key_source"]), Color::Dim)),
        ],
        vec![row(
            "User",
            format!(
                "{} {}",
                text(&plan["user"]),
                style.paint(&format!("({}:{})", text(&plan["uid"]), text(&plan["gid"])), Color::Dim)
            ),
        )],
    ];
    format!(
        "\n{}\n",
        kv_panel("CREATE PLAN — NO CHANGES MADE YET", &sections, style)
    )
}

fn home_relative(path: &str, home: Option<&str>) -> String {
    match home {
        Some(home) if !home.is_empty() && path.starts_with(home) => format!("~{}", &path[home.len()..]),
        _ => path.to_owned(),
    }
}

/// Python `show_create_result`.
#[must_use]
pub fn create_result(result: &Value, style: &Style, home: Option<&str>) -> String {
    let sections = vec![
        vec![
            row("VMID", text(&result["vmid"])),
            row("Name", text(&result["name"])),
            row("Node", text(&result["node"])),
            row("Status", vm_status(style, &result["status"])),
        ],
        vec![
            row("IP", format!("{}/{}", text(&result["ip"]), text(&result["cidr"]))),
            row("MAC", text(&result["mac"])),
        ],
        vec![
            row(
                "Clone",
                format!(
                    "FULL {}",
                    style.paint(
                        &format!(
                            "from Gold {} on {}",
                            text(&result["gold_vmid"]),
                            text(&result["source_node"])
                        ),
                        Color::Dim
                    )
                ),
            ),
            row(
                "Root",
                format!(
                    "{} on {}",
                    root_disk(result, style, "inherited from Gold"),
                    text(&result["root_storage"])
                ),
            ),
        ],
        vec![
            row(
                "Persistent home",
                format!("{} on {}", text(&result["home_disk"]), text(&result["home_storage"])),
            ),
            row("Home size", text(&result["home_size"])),
            row("Filesystem label", text(&result["home_label"])),
        ],
        vec![
            row("SSH public keys", key_labels(&result["ssh_public_keys"], style)),
            row("Key source", style.paint(&text(&result["ssh_key_source"]), Color::Dim)),
            row("Root login", text(&result["ssh"]["root"])),
            row("User login", text(&result["ssh"]["user"])),
            row("SSH config", home_relative(&text(&result["ssh_config_path"]), home)),
        ],
    ];
    format!("\n{}", kv_panel("WORKSPACE CREATED", &sections, style))
}

/// Python `show_destroy_plan`.
#[must_use]
pub fn destroy_plan(plan: &Value, style: &Style) -> String {
    let mut vm = Table::new(&["VMID", "NAME", "NODE", "STATUS"]);
    vm.row(vec![
        Styled::plain(text(&plan["vmid"])),
        Styled::plain(text(&plan["name"])),
        Styled::new(text(&plan["node"]), Color::Grey),
        Styled::plain(vm_status(&Style { color: false, ..*style }, &plan["status"])),
    ]);
    let mut disks = Table::new(&["RESOURCE", "DISK", "STORAGE", "SIZE", "DATA USED", "ACTION"]).right(&[3, 4]);
    disks.row(vec![
        Styled::new("root", Color::Grey),
        Styled::plain(text(&plan["root_disk"])),
        Styled::plain(text(&plan["root_storage"])),
        Styled::plain(gib_value(&plan["root_size_gib"])),
        Styled::plain(DASH),
        Styled::new("DELETE", Color::Red),
    ]);
    disks.row(vec![
        Styled::new("home", Color::Grey),
        Styled::plain(text(&plan["home_disk"])),
        Styled::plain(text(&plan["home_storage"])),
        Styled::plain(gib_value(&plan["home_size_gib"])),
        Styled::plain(byte_value(&plan["home_used_bytes"])),
        Styled::new("DELETE PERMANENTLY", Color::Red),
    ]);
    let mut body: Vec<String> = vm.render(style).lines().map(str::to_owned).collect();
    body.push(String::new());
    body.extend(disks.render(style).lines().map(str::to_owned));
    if let Some(identity) = plan["destructive_identity"].as_object() {
        body.push(String::new());
        body.push(format!("Authorized pool: {}", text(&identity["pool"])));
        body.push(format!("Role: {}", text(&identity["role"])));
        body.push(format!("PVE digest: {}", text(&identity["config"]["digest"])));
        body.push("Exact VM-managed volumes to delete:".to_owned());
        for disk in identity["disks"].as_array().into_iter().flatten() {
            body.push(format!(
                "  {} ({}): {}",
                text(&disk["slot"]),
                text(&disk["role"]),
                text(&disk["config"])
            ));
        }
    }
    let storages: Vec<&Value> = plan["storage_status"].as_array().into_iter().flatten().collect();
    if !storages.is_empty() {
        let mut table = Table::new(&["STORAGE", "FREE NOW", "TOTAL"]).right(&[1, 2]);
        for item in storages {
            table.row(vec![
                Styled::plain(text(&item["name"])),
                Styled::new(byte_value(&item["available_bytes"]), Color::Green),
                Styled::plain(byte_value(&item["total_bytes"])),
            ]);
        }
        body.push(String::new());
        body.extend(table.render(style).lines().map(str::to_owned));
    }
    let files: Vec<String> = plan["node_files_left"]
        .as_array()
        .into_iter()
        .flatten()
        .map(text)
        .collect();
    if !files.is_empty() {
        body.push(String::new());
        body.push(style.paint("Files of the old Python client left on the nodes:", Color::Yellow));
        body.extend(files.iter().map(|file| format!("  {file}")));
    }
    format!("\n{}\n", panel("DESTROY PLAN", &body, style, (1, 2)))
}

fn removed(style: &Style, value: &Value) -> String {
    match value.as_array().map(Vec::len) {
        Some(count) if count > 0 => style.paint(&format!("✓ {count} removed"), Color::Green),
        _ => style.paint("no stale entry", Color::Dim),
    }
}

/// Python `show_destroy_result`.
#[must_use]
pub fn destroy_result(result: &Value, style: &Style) -> String {
    let mut local = vec![
        row("VM deleted", check(style, &result["vm_deleted"])),
        row("Persistent home deleted", check(style, &result["home_deleted"])),
        row("Local SSH config", removed(style, &result["ssh_config_removed"])),
        row("Local known_hosts", removed(style, &result["ssh_known_hosts_removed"])),
    ];
    if let Some(left) = result["backup_keys_left"].as_array().filter(|left| !left.is_empty()) {
        local.push(row(
            "BK keys left",
            style.paint(&left.iter().map(text).collect::<Vec<_>>().join("\n"), Color::Yellow),
        ));
    }
    let sections = vec![
        vec![
            row("VMID", text(&result["vmid"])),
            row("Name", text(&result["name"])),
            row("Status", style.paint("absent", Color::Dim)),
        ],
        local,
    ];
    format!("\n{}", kv_panel("WORKSPACE DESTROYED", &sections, style))
}

/// Python `format_volume_lines`: a header and one line per volume.
#[must_use]
pub fn format_volume_lines(volumes: &[Value]) -> Vec<String> {
    if volumes.is_empty() {
        return Vec::new();
    }
    let field = |item: &Value, key: &str, default: &str| {
        item[key]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(default)
            .to_owned()
    };
    let sizes: Vec<String> = volumes
        .iter()
        .map(|item| match &item["size"] {
            Value::String(size) if !size.is_empty() => size.clone(),
            Value::Number(size) => size.to_string(),
            _ => item["size_bytes"].as_i64().map_or_else(|| DASH.to_owned(), human_bytes),
        })
        .collect();
    let width = |key: &str, default: &str, least: usize| {
        volumes
            .iter()
            .map(|item| field(item, key, default).chars().count())
            .max()
            .unwrap_or(0)
            .max(least)
    };
    let slot_width = width("slot", DASH, 4);
    let role_width = width("role", "disk", 4);
    let volume_width = width("volume", DASH, 6);
    let size_width = sizes.iter().map(|s| s.chars().count()).max().unwrap_or(0).max(4);
    let mut lines = vec![format!(
        "{:<slot_width$}  {:<role_width$}  {:<volume_width$}  {:>size_width$}",
        "SLOT", "ROLE", "VOLUME", "SIZE"
    )];
    for (item, size) in volumes.iter().zip(&sizes) {
        lines.push(format!(
            "{:<slot_width$}  {:<role_width$}  {:<volume_width$}  {:>size_width$}",
            field(item, "slot", DASH),
            field(item, "role", "disk"),
            field(item, "volume", DASH),
            size
        ));
    }
    lines
}

/// Python `show_migrate_plan`.
#[must_use]
pub fn migrate_plan(plan: &Value, style: &Style) -> String {
    let volumes: Vec<Value> = plan["volumes"].as_array().cloned().unwrap_or_default();
    let lines = format_volume_lines(&volumes);
    let sizes: Vec<i64> = volumes.iter().filter_map(|item| item["size_bytes"].as_i64()).collect();
    let total = if sizes.is_empty() {
        DASH.to_owned()
    } else {
        human_bytes(sizes.iter().sum())
    };
    let status = plan["status"].as_str().unwrap_or("unknown");
    let mut sections = vec![
        vec![
            row("VMID", text(&plan["vmid"])),
            row("Name", text(&plan["name"])),
            row("Status", vm_status(style, &plan["status"])),
        ],
        vec![
            row("Source node", text(&plan["source_node"])),
            row("Target node", text(&plan["target_node"])),
            row("Target storage", text(&plan["target_storage"])),
        ],
    ];
    if !lines.is_empty() {
        sections.push(vec![row("Volumes", lines.join("\n")), row("Data to migrate", total)]);
    }
    sections.push(vec![
        row(
            "Persistent home",
            format!("{} · {}", text(&plan["home_disk"]), text(&plan["home_label"])),
        ),
        row(
            "Migration",
            format!(
                "{} {}",
                style.paint("OFFLINE", Color::Yellow),
                style.paint("— root + home + cloud-init", Color::Dim)
            ),
        ),
        row(
            "Power state",
            format!(
                "{} {}",
                style.paint("PRESERVE", Color::Green),
                style.paint(&format!("— {status} → {status}"), Color::Dim)
            ),
        ),
    ]);
    format!(
        "\n{}\n",
        kv_panel("MIGRATE PLAN — NO CHANGES MADE YET", &sections, style)
    )
}

/// Python `show_refresh_plan`.
#[must_use]
pub fn refresh_plan(plan: &Value, style: &Style) -> String {
    if plan["mode"] == "recover" {
        let mut sections = vec![
            vec![row("VMID", text(&plan["vmid"])), row("Name", text(&plan["name"]))],
            vec![
                row(
                    "Interrupted phase",
                    plan["recovery_phase"].as_str().unwrap_or("unknown"),
                ),
                row("Action", style.paint("recover or finish cleanup", Color::Yellow)),
            ],
            vec![row(
                "Persistent home",
                format!(
                    "{} {}",
                    style.paint("PRESERVE", Color::Green),
                    text(&plan["home_label"])
                ),
            )],
        ];
        let mismatches: Vec<String> = plan["mismatches"].as_array().into_iter().flatten().map(text).collect();
        if !mismatches.is_empty() {
            sections.push(vec![row(
                "Mismatches",
                style.paint(&mismatches.join("\n"), Color::Yellow),
            )]);
        }
        return kv_panel("REFRESH RECOVERY PLAN — NO CHANGES MADE YET", &sections, style);
    }
    let status = plan["status"].as_str().unwrap_or("unknown");
    let sections = vec![
        vec![
            row("VMID", text(&plan["vmid"])),
            row("Name", text(&plan["name"])),
            row("Status", vm_status(style, &plan["status"])),
        ],
        vec![
            row("Source", format!("Gold VM {}", text(&plan["gold_vmid"]))),
            row("Root action", style.paint("STAGE + VERIFY + REPLACE", Color::Yellow)),
            row(
                "Root disk",
                format!(
                    "{} on {}",
                    root_disk(plan, style, "from Gold"),
                    text(&plan["root_storage"])
                ),
            ),
        ],
        vec![
            row("IP", format!("{}/{}", text(&plan["ip"]), text(&plan["cidr"]))),
            row("Gateway", text(&plan["gateway"])),
        ],
        vec![
            row(
                "Persistent home",
                format!("{} on {}", text(&plan["home_disk"]), text(&plan["home_storage"])),
            ),
            row("Filesystem label", text(&plan["home_label"])),
            row(
                "Home policy",
                format!(
                    "{} {}",
                    style.paint("PRESERVE", Color::Green),
                    style.paint("— never format during refresh", Color::Dim)
                ),
            ),
            row(
                "Power state",
                format!(
                    "{} {}",
                    style.paint("PRESERVE", Color::Green),
                    style.paint(&format!("— {status} → {status}"), Color::Dim)
                ),
            ),
        ],
    ];
    format!(
        "\n{}\n",
        kv_panel("REFRESH PLAN — ROOT WILL BE REPLACED", &sections, style)
    )
}

/// Python `show_refresh_result`.
#[must_use]
pub fn refresh_result(result: &Value, style: &Style) -> String {
    let sections = vec![
        vec![
            row("VMID", text(&result["vmid"])),
            row("Name", text(&result["name"])),
            row("Status", vm_status(style, &result["status"])),
        ],
        vec![
            row(
                "Root",
                format!(
                    "{} from Gold {}",
                    style.paint("refreshed", Color::Green),
                    text(&result["gold_vmid"])
                ),
            ),
            row("IP", format!("{}/{}", text(&result["ip"]), text(&result["cidr"]))),
            row("MAC", text(&result["mac"])),
        ],
        vec![
            row(
                "Persistent home",
                format!(
                    "{} {}",
                    style.paint("✓ preserved", Color::Green),
                    text(&result["home_disk"])
                ),
            ),
            row("Filesystem label", text(&result["home_label"])),
            row(
                "Power state",
                format!(
                    "{} {}",
                    style.paint("✓ preserved", Color::Green),
                    result["status"].as_str().unwrap_or("unknown")
                ),
            ),
        ],
        vec![
            row("Root login", text(&result["ssh"]["root"])),
            row("User login", text(&result["ssh"]["user"])),
        ],
    ];
    format!("\n{}", kv_panel("WORKSPACE REFRESHED", &sections, style))
}

/// The Gold VM and the workspaces of the global status.
#[must_use]
pub fn global_status_rows(result: &Value) -> Vec<Value> {
    let mut rows = Vec::new();
    if result["gold"].is_object() {
        rows.push(result["gold"].clone());
    }
    rows.extend(
        result["workspaces"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|w| w.is_object())
            .cloned(),
    );
    rows
}

/// The summary under the global status table, as plain lines.
#[must_use]
pub fn global_summary_lines(result: &Value, narrow: bool) -> Vec<String> {
    let summary = &result["summary"];
    let homes = &summary["homes"];
    let count = integer(&summary["workspace_count"]).unwrap_or(0);
    let running = integer(&summary["running"]).unwrap_or(0);
    let stopped = integer(&summary["stopped"]).unwrap_or(0);
    let mut values: Vec<(String, String)> = vec![if narrow {
        row("WS", format!("{count} ({running} running)"))
    } else {
        row("Workspaces", format!("{count} ({running} running / {stopped} stopped)"))
    }];
    let nodes: Vec<String> = summary["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item.is_object())
        .map(|item| {
            format!(
                "{} {}",
                item["node"].as_str().filter(|s| !s.is_empty()).unwrap_or(DASH),
                item["status"].as_str().filter(|s| !s.is_empty()).unwrap_or("unknown")
            )
        })
        .collect();
    if !nodes.is_empty() {
        values.push(row(if narrow { "Nodes" } else { "Cluster nodes" }, nodes.join(", ")));
    }
    let orphaned = summary["orphaned_volumes"].as_array().map_or(0, Vec::len);
    if orphaned > 0 {
        values.push(row(
            if narrow { "Detached" } else { "Detached HS volumes" },
            orphaned.to_string(),
        ));
    }
    if count > 0 {
        let free = homes["free_percent"]
            .as_f64()
            .map_or_else(|| "unknown".to_owned(), |free| format!("{free:.1}%"));
        let usage = format!(
            "{} used / {} allocated",
            byte_value(&homes["used_bytes"]),
            byte_value(&homes["quota_bytes"])
        );
        if narrow {
            values.push(row("Homes", usage));
            values.push(row("Home free", free));
        } else {
            values.push(row("Persistent homes", usage));
            values.push(row("Home filesystem free", free));
        }
    }
    let width = values.iter().map(|(label, _)| label.chars().count()).max().unwrap_or(0);
    let mut lines: Vec<String> = values
        .iter()
        .map(|(label, value)| format!("{label:<width$} : {value}"))
        .collect();
    let missing = integer(&homes["missing_count"]).unwrap_or(0);
    if missing > 0 {
        lines.push(format!("Missing home disks: {missing}"));
    }
    lines
}

/// The `storage_layouts` rows of the summary.
#[must_use]
pub fn global_storage_rows(result: &Value) -> Vec<Value> {
    result["summary"]["storage_layouts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item.is_object())
        .cloned()
        .collect()
}

/// The storage table of the global status.
#[must_use]
pub fn build_global_storage_table(result: &Value, narrow: bool) -> Option<Table> {
    let rows = global_storage_rows(result);
    if rows.is_empty() {
        return None;
    }
    let summary = &result["summary"];
    let unit = summary["storage_unit"].as_str().unwrap_or("GiB");
    let decimals = summary["storage_decimals"].as_u64().unwrap_or(0) as usize;
    let mut table = if narrow {
        Table::new(&["NODE", "STATE", "STORAGE", "FREE %"]).right(&[3])
    } else {
        let used = format!("USED {unit}");
        let free = format!("FREE {unit}");
        Table::new(&["LAYOUT", "NODE", "STATE", "STORAGE", &used, &free, "FREE %"]).right(&[4, 5, 6])
    };
    for item in rows {
        let node_status = item["node_status"].as_str().unwrap_or("unknown");
        let state = Styled::new(
            node_status,
            if node_status == "online" {
                Color::Green
            } else {
                Color::Yellow
            },
        );
        let free = free_percent_text(
            free_percent(integer(&item["total_bytes"]), integer(&item["available_bytes"])),
            1,
        );
        let node = Styled::new(text(&item["node"]), Color::Grey);
        let storage = Styled::plain(text(&item["storage"]));
        if narrow {
            table.row(vec![node, state, storage, free]);
        } else {
            table.row(vec![
                Styled::new(text(&item["layout"]), Color::Grey),
                node,
                state,
                storage,
                Styled::plain(byte_value_in_unit(&item["used_bytes"], unit, decimals)),
                Styled::plain(byte_value_in_unit(&item["available_bytes"], unit, decimals)),
                free,
            ]);
        }
    }
    Some(table)
}

/// Python `show_global_status`.
#[must_use]
pub fn global_status(result: &Value, style: &Style) -> String {
    let narrow = style.width < NARROW_WIDTH;
    let mut table = if narrow {
        Table::new(&["VMID", "ROLE", "NAME", ""])
    } else {
        Table::new(&["VMID", "ROLE", "NAME", "STATUS", "IP", "HOME GiB", "FREE %"]).right(&[5, 6])
    };
    table.show_header = !narrow;
    for item in global_status_rows(result) {
        let status = item["status"].as_str().unwrap_or("unknown");
        let status_cell = match (status, narrow) {
            ("running", true) => Styled::new("●", Color::Green),
            ("running", false) => Styled::new("● running", Color::Green),
            ("stopped", true) => Styled::new("○", Color::Dim),
            ("stopped", false) => Styled::new("○ stopped", Color::Dim),
            (_, true) => Styled::new("!", Color::Yellow),
            (other, false) => Styled::new(other, Color::Yellow),
        };
        let mut name = text(&item["name"]);
        if !item["warning"].is_null() {
            name.push_str(" !");
        }
        let mut cells = vec![
            Styled::plain(text(&item["vmid"])),
            Styled::new(text(&item["role"]), Color::Grey),
            Styled::plain(name),
            status_cell,
        ];
        if !narrow {
            let home = &item["home"];
            let (size, free) = if home.is_object() && !home["size_bytes"].is_null() {
                (gib_text(&home["size_bytes"], 1), home["free_percent"].as_f64())
            } else {
                (DASH.to_owned(), None)
            };
            cells.extend([
                Styled::plain(text(&item["ip"])),
                Styled::plain(size),
                free_percent_text(free, 1),
            ]);
        }
        table.row(cells);
    }
    let mut body: Vec<String> = table.render(style).lines().map(str::to_owned).collect();
    body.push(style.paint(&"─".repeat(style.width.saturating_sub(8).clamp(20, 72)), Color::Grey));
    body.extend(global_summary_lines(result, narrow));
    if let Some(storage) = build_global_storage_table(result, narrow) {
        body.push(String::new());
        body.extend(storage.render(style).lines().map(str::to_owned));
    }
    let mut out = panel("HOMESTACK", &body, style, (1, 2));
    let warnings: Vec<String> = result["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .map(text)
        // By design (decision 10) and nothing to act on: `--json` keeps it.
        .filter(|warning| !warning.starts_with("HomeStack orphan scan incomplete:"))
        .map(|warning| style.paint(&format!("! {warning}"), Color::Yellow))
        .collect();
    if !warnings.is_empty() {
        out.push_str(&panel(
            &style.paint("WARNINGS", Color::Yellow),
            &warnings,
            style,
            (0, 1),
        ));
    }
    out
}

/// Python `show_status_result`.
#[must_use]
pub fn status_result(result: &Value, style: &Style) -> String {
    if result["ok"] != true {
        return panel(
            "WORKSPACE STATUS",
            &[format!("VM {} does not exist.", text(&result["vmid"]))],
            style,
            (1, 2),
        );
    }
    let mut first = vec![
        row("Name", dim_value(style, &result["name"])),
        row("Role", dim_value(style, &result["role"])),
        row("Role tag", check(style, &result["role_tag_ok"])),
        row("Node", dim_value(style, &result["node"])),
        row("Status", vm_status(style, &result["status"])),
    ];
    let mut extra: Vec<String> = result["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|tag| !["homestack-ws", "homestack-alien", "homestack-gold"].contains(tag))
        .map(str::to_owned)
        .collect();
    extra.sort();
    extra.dedup();
    if !extra.is_empty() {
        first.insert(3, row("Additional tags", extra.join(", ")));
    }
    let mut sections = vec![
        first,
        vec![
            row("Configured IP", dim_value(style, &result["configured_ip"])),
            row("Actual hostname", dim_value(style, &result["actual_hostname"])),
            row("Actual IP", dim_value(style, &result["actual_ip"])),
            row("QEMU Guest Agent", check(style, &result["qga"])),
        ],
        vec![
            row("Expected label", dim_value(style, &result["home_label"])),
            row("Disk identity", check(style, &result["home_identity_ok"])),
            row("Guest home", dim_value(style, &result["home_mount"])),
            row("Mount identity", check(style, &result["home_mount_ok"])),
        ],
        vec![
            row("Root login", dim_value(style, &result["ssh"]["root"])),
            row("User login", dim_value(style, &result["ssh"]["user"])),
        ],
    ];
    if let Some(warning) = result["role_warning"].as_str() {
        sections.insert(1, vec![row("Warning", style.paint(warning, Color::Yellow))]);
    }
    let volumes: Vec<Value> = result["volumes"].as_array().cloned().unwrap_or_default();
    let lines = format_volume_lines(&volumes);
    if !lines.is_empty() {
        let at = sections.len() - 1;
        sections.insert(at, vec![row("Volumes", lines.join("\n"))]);
    }
    kv_panel(
        &format!("WORKSPACE STATUS — VM {}", text(&result["vmid"])),
        &sections,
        style,
    )
}

/// The server instances and the node (`transport`).
#[must_use]
pub fn transport_result(result: &Value, style: &Style) -> String {
    let mut sections = Vec::new();
    for instance in result["transport"]["instances"].as_array().into_iter().flatten() {
        let state = if instance["ok"] == true {
            format!(
                "{} {}",
                style.paint("✓", Color::Green),
                style.paint(&format!("server {}", text(&instance["version"])), Color::Dim)
            )
        } else {
            style.paint(&format!("✗ {}", text(&instance["error"])), Color::Red)
        };
        sections.push(vec![
            row("Instance", text(&instance["url"])),
            row("State", state),
            row("Config version", dim_value(style, &instance["config_version"])),
        ]);
    }
    let pve = &result["pve"];
    sections.push(vec![
        row("Node", dim_value(style, &pve["node"])),
        row("PVE version", dim_value(style, &pve["version"])),
        row("Kernel", style.paint(&text(&pve["kernel"]), Color::Dim)),
        row("Uptime", dim_value(style, &pve["uptime"])),
    ]);
    kv_panel("HOMESTACK TRANSPORT", &sections, style)
}

/// The local tools and keys and what the server found (`discover`).
#[must_use]
pub fn discovery_report(report: &Value, style: &Style) -> String {
    let mut local = Table::new(&["Component", "State"]);
    for (name, found) in report["local"]["commands"].as_object().into_iter().flatten() {
        local.row(vec![
            Styled::plain(name.clone()),
            if found == true {
                Styled::new("✓ found", Color::Green)
            } else {
                Styled::new("✗ missing", Color::Red)
            },
        ]);
    }
    let identities = report["local"]["hardware_ssh_identities"]
        .as_array()
        .map_or(0, Vec::len);
    local.row(vec![
        Styled::plain("hardware SSH identities"),
        Styled::plain(identities.to_string()),
    ]);
    let mut body: Vec<String> = local.render(style).lines().map(str::to_owned).collect();
    let discovery = &report["server"]["discovery"];
    if discovery.is_object() {
        body.push(String::new());
        body.push(format!("Server: {}", text(&report["server"]["url"])));
        let mut nodes = Table::new(&["NODE", "STATUS"]);
        for node in discovery["nodes"].as_array().into_iter().flatten() {
            nodes.row(vec![
                Styled::plain(text(&node["node"])),
                Styled::plain(text(&node["status"])),
            ]);
        }
        body.extend(nodes.render(style).lines().map(str::to_owned));
        let gold: Vec<String> = discovery["gold_candidates"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|item| {
                format!(
                    "{} {} on {}",
                    text(&item["vmid"]),
                    text(&item["name"]),
                    text(&item["node"])
                )
            })
            .collect();
        body.push(format!(
            "Gold candidates: {}",
            if gold.is_empty() {
                "none".to_owned()
            } else {
                gold.join(", ")
            }
        ));
    }
    for error in report["errors"].as_array().into_iter().flatten() {
        body.push(style.paint(&format!("! {}", text(error)), Color::Yellow));
    }
    panel("HOMESTACK DISCOVERY", &body, style, (1, 2))
}
