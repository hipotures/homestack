//! The program run on the workspace (Python `setup_guest.py`, sent
//! unchanged) and the clean environment the setup's commands run in
//! (Python `setup.guest`, `command_environment`, `require_tool`).

use serde_json::{Map, Value};

use super::Settings;
use super::workspace::Workspace;
use crate::AppError;
use hs_install::shell;

/// The guest program, byte for byte Python's `setup_guest.py`.
pub const PROGRAM: &str = include_str!("../../guest/setup_guest.py");

/// `command` in `env -i` with the workspace user's home, a fixed `PATH`
/// (the `bins` first) and `interpreter` without its profile files.
#[must_use]
pub fn command_environment(
    settings: &Settings,
    command: &str,
    interpreter: &str,
    bins: &[String],
    pipefail: bool,
) -> String {
    let home = settings.guest_home();
    let mut directories: Vec<String> = vec![format!("{home}/.local/bin")];
    for bin in bins {
        let path = format!("{home}/{}", bin.get(2..).unwrap_or_default());
        if !directories.contains(&path) {
            directories.push(path);
        }
    }
    directories.extend(["/usr/local/bin", "/usr/bin", "/bin"].map(str::to_owned));
    let mut args: Vec<String> = vec![
        "env".into(),
        "-i".into(),
        format!("HOME={home}"),
        format!("USER={}", settings.user),
        format!("LOGNAME={}", settings.user),
        format!("PATH={}", directories.join(":")),
        "TERM=xterm-256color".into(),
        "LANG=C.UTF-8".into(),
        format!("SHELL=/bin/{interpreter}"),
        interpreter.into(),
    ];
    match interpreter {
        "bash" => args.extend(["--noprofile".into(), "--norc".into()]),
        "zsh" => args.push("-f".into()),
        _ => {}
    }
    if pipefail {
        args.extend(["-e".into(), "-o".into(), "pipefail".into()]);
    }
    args.push("-c".into());
    args.push(format!("cd {}\numask 077\n{command}", shell::quote(&home)));
    shell::join(&args)
}

/// The program and its payload as streamed to `python3 -`: the payload is
/// a string literal, never an argument (no `ARG_MAX`, nothing in `ps`).
#[must_use]
pub fn script(payload: &Value) -> String {
    // A JSON string is a valid Python string literal.
    let literal = serde_json::to_string(&payload.to_string()).unwrap_or_default();
    format!("{PROGRAM}\nmain({literal})\n")
}

/// Run guest `operation` with `values`; its answer, or its error.
pub fn guest(
    workspace: &mut dyn Workspace,
    settings: &Settings,
    operation: &str,
    values: Map<String, Value>,
) -> Result<Value, AppError> {
    let mut payload = Map::new();
    payload.insert("home".into(), Value::String(settings.guest_home()));
    payload.insert("operation".into(), Value::String(operation.to_owned()));
    payload.extend(values);
    let payload = Value::Object(payload);
    let output = if workspace.local() {
        workspace.local_guest(&payload)?
    } else {
        let command = command_environment(settings, "python3 -", "sh", &[], false);
        let output = workspace.run(&command, Some(&script(&payload)), false)?;
        if output.code != 0 {
            return Err(AppError::new(format!(
                "Workspace command failed (exit {}); output withheld",
                output.code
            )));
        }
        output.stdout
    };
    let data: Value = serde_json::from_str(&output)
        .map_err(|_| AppError::new("Guest operation returned invalid data; output withheld"))?;
    if !data.is_object() {
        return Err(AppError::new("Guest operation returned invalid data; output withheld"));
    }
    if !super::engine::truthy(&data["ok"]) {
        return Err(AppError::new(
            data["error"].as_str().unwrap_or("Guest operation failed").to_owned(),
        ));
    }
    Ok(data)
}

/// `command -v TOOL` on the workspace, or a clear refusal.
pub fn require_tool(
    workspace: &mut dyn Workspace,
    settings: &Settings,
    tool: &str,
    bins: &[String],
) -> Result<(), AppError> {
    let test = format!("command -v {} >/dev/null", shell::quote(tool));
    if workspace.local() {
        if workspace.local_command(&test)?.code != 0 {
            return Err(AppError::new(format!(
                "Missing local executable {tool}; install system prerequisites separately"
            )));
        }
        return Ok(());
    }
    let output = workspace.run(&command_environment(settings, &test, "sh", bins, false), None, false)?;
    if output.code == 255 {
        return Err(AppError::new(
            "Workspace SSH transport failed during prerequisite inspection",
        ));
    }
    if output.code != 0 {
        return Err(AppError::new(format!(
            "Missing workspace executable {tool}; install system prerequisites on the target VM separately"
        )));
    }
    Ok(())
}

/// `values` as a JSON object (the keyword arguments of a guest call).
#[macro_export]
macro_rules! guest_args {
    ($($key:literal : $value:expr),* $(,)?) => {{
        let mut map = serde_json::Map::new();
        $(map.insert($key.to_owned(), serde_json::json!($value));)*
        map
    }};
}
