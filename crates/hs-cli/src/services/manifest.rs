//! Service manifest schema 1 (`.agents/skills/homestack-service/manifest-schema.md`):
//! one `.homestack/<file>.yaml` document describes one service. The parser
//! is strict: unknown fields, wrong types and unsafe paths are refused, and
//! nothing in a manifest is ever executed or interpolated.

use yaml_rust2::{Yaml, YamlLoader};

/// The largest manifest file read.
pub const MAX_BYTES: u64 = 64 * 1024;
/// The only persistent mount schema 1 allows.
pub const MOUNT: &str = "/home/app";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub image_repository: String,
    pub ports: Vec<Port>,
    pub healthcheck: Option<Healthcheck>,
    pub directories: Vec<String>,
    pub mappings: Vec<Mapping>,
    pub multi_node: bool,
    pub state_model: StateModel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Port {
    pub name: String,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Healthcheck {
    Http {
        port: String,
        path: String,
        expected_status: u16,
    },
    /// Only that the port accepts connections, not application health.
    Tcp { port: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapping {
    pub source: String,
    pub destination: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateModel {
    Local,
    Stateless,
    External,
}

impl StateModel {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Stateless => "stateless",
            Self::External => "external",
        }
    }
}

type Fields<'a> = &'a yaml_rust2::yaml::Hash;

/// The mapping at `path`, its keys limited to `allowed`.
fn mapping<'a>(value: &'a Yaml, path: &str, allowed: &[&str]) -> Result<Fields<'a>, String> {
    let Yaml::Hash(fields) = value else {
        return Err(format!("{path}: expected a mapping"));
    };
    for key in fields.keys() {
        match key.as_str() {
            Some(name) if allowed.contains(&name) => {}
            Some(name) => return Err(format!("{}: unknown field", join(path, name))),
            None => return Err(format!("{path}: field names must be strings")),
        }
    }
    Ok(fields)
}

fn join(path: &str, name: &str) -> String {
    if path.is_empty() {
        name.to_owned()
    } else {
        format!("{path}.{name}")
    }
}

fn field<'a>(fields: Fields<'a>, name: &str) -> Option<&'a Yaml> {
    fields.get(&Yaml::String(name.to_owned()))
}

fn required<'a>(fields: Fields<'a>, path: &str, name: &str) -> Result<&'a Yaml, String> {
    field(fields, name).ok_or_else(|| format!("{}: required", join(path, name)))
}

fn string(value: &Yaml, path: &str) -> Result<String, String> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("{path}: expected a string"))
}

fn integer(value: &Yaml, path: &str, range: std::ops::RangeInclusive<i64>) -> Result<i64, String> {
    let Yaml::Integer(number) = value else {
        return Err(format!("{path}: expected an integer"));
    };
    if !range.contains(number) {
        return Err(format!("{path}: must be {}–{}", range.start(), range.end()));
    }
    Ok(*number)
}

fn list<'a>(value: &'a Yaml, path: &str) -> Result<&'a [Yaml], String> {
    value
        .as_vec()
        .map(Vec::as_slice)
        .ok_or_else(|| format!("{path}: expected a list"))
}

/// `^[a-z0-9][a-z0-9-]{0,62}$`
fn valid_id(text: &str) -> bool {
    (1..=63).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !text.starts_with('-')
}

/// `^[a-z][a-z0-9-]{0,31}$`
fn valid_port_name(text: &str) -> bool {
    (1..=32).contains(&text.len())
        && text.starts_with(|c: char| c.is_ascii_lowercase())
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// An OCI repository without tag or digest, lowercase: an optional
/// registry host (with a port) and path components of `[a-z0-9._-]`.
fn valid_repository(text: &str) -> bool {
    let mut parts = text.split('/');
    let Some(first) = parts.next() else {
        return false;
    };
    let rest: Vec<&str> = parts.collect();
    let component = |part: &str| {
        !part.is_empty()
            && part.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
            && part
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
    };
    let host = match first.split_once(':') {
        Some((host, port)) => component(host) && !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()),
        None => component(first),
    };
    !rest.is_empty() && host && rest.iter().all(|part| component(part))
}

/// A relative path inside the mount: no leading `/`, no empty, `.` or
/// `..` components, no control characters or backslashes.
fn valid_relative(text: &str) -> bool {
    !text.is_empty()
        && !text.starts_with('/')
        && !text.chars().any(|c| c.is_control() || c == '\\')
        && text
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn valid_absolute(text: &str) -> bool {
    text.strip_prefix('/').is_some_and(valid_relative)
}

fn within(path: &str, directory: &str) -> bool {
    path == directory || path.strip_prefix(directory).is_some_and(|rest| rest.starts_with('/'))
}

/// Parse and validate one manifest file's text.
pub fn parse(text: &str) -> Result<Manifest, String> {
    if text.len() as u64 > MAX_BYTES {
        return Err(format!("larger than {MAX_BYTES} bytes"));
    }
    let documents = YamlLoader::load_from_str(text).map_err(|error| format!("invalid YAML: {error}"))?;
    let [document] = documents.as_slice() else {
        return Err(format!("expected exactly one YAML document, found {}", documents.len()));
    };
    let top = mapping(
        document,
        "",
        &["schema", "id", "name", "image", "runtime", "persistence", "deployment"],
    )?;
    integer(required(top, "", "schema")?, "schema", 1..=1)?;
    let id = string(required(top, "", "id")?, "id")?;
    if !valid_id(&id) {
        return Err("id: must match ^[a-z0-9][a-z0-9-]{0,62}$".to_owned());
    }
    let name = string(required(top, "", "name")?, "name")?;
    if !(1..=64).contains(&name.chars().count()) {
        return Err("name: must be 1–64 characters".to_owned());
    }
    let image = mapping(required(top, "", "image")?, "image", &["repository"])?;
    let image_repository = string(required(image, "image", "repository")?, "image.repository")?;
    if !valid_repository(&image_repository) {
        return Err("image.repository: expected a lowercase OCI repository without tag or digest".to_owned());
    }
    let (ports, healthcheck) = match field(top, "runtime") {
        Some(runtime) => runtime_of(runtime)?,
        None => (Vec::new(), None),
    };
    let (directories, mappings) = persistence_of(required(top, "", "persistence")?)?;
    let deployment = mapping(
        required(top, "", "deployment")?,
        "deployment",
        &["multi_node", "state_model"],
    )?;
    let multi_node = match field(deployment, "multi_node") {
        None => false,
        Some(Yaml::Boolean(value)) => *value,
        Some(_) => return Err("deployment.multi_node: expected a boolean".to_owned()),
    };
    let state_model = match string(
        required(deployment, "deployment", "state_model")?,
        "deployment.state_model",
    )?
    .as_str()
    {
        "local" => StateModel::Local,
        "stateless" => StateModel::Stateless,
        "external" => StateModel::External,
        _ => return Err("deployment.state_model: expected local, stateless or external".to_owned()),
    };
    if multi_node && state_model == StateModel::Local {
        return Err("deployment.multi_node: true requires state_model stateless or external".to_owned());
    }
    Ok(Manifest {
        id,
        name,
        image_repository,
        ports,
        healthcheck,
        directories,
        mappings,
        multi_node,
        state_model,
    })
}

fn runtime_of(value: &Yaml) -> Result<(Vec<Port>, Option<Healthcheck>), String> {
    let runtime = mapping(value, "runtime", &["ports", "healthcheck"])?;
    let mut ports: Vec<Port> = Vec::new();
    if let Some(entries) = field(runtime, "ports") {
        for (index, entry) in list(entries, "runtime.ports")?.iter().enumerate() {
            let path = format!("runtime.ports[{index}]");
            let fields = mapping(entry, &path, &["name", "port", "protocol"])?;
            let name = string(required(fields, &path, "name")?, &join(&path, "name"))?;
            if !valid_port_name(&name) {
                return Err(format!("{path}.name: must match ^[a-z][a-z0-9-]{{0,31}}$"));
            }
            let port = u16::try_from(integer(
                required(fields, &path, "port")?,
                &join(&path, "port"),
                1..=65535,
            )?)
            .map_err(|_| format!("{path}.port: must be 1–65535"))?;
            if string(required(fields, &path, "protocol")?, &join(&path, "protocol"))? != "tcp" {
                return Err(format!("{path}.protocol: must be tcp"));
            }
            if ports.iter().any(|known| known.name == name) {
                return Err(format!("{path}.name: duplicate port name {name}"));
            }
            if ports.iter().any(|known| known.port == port) {
                return Err(format!("{path}.port: duplicate port {port}"));
            }
            ports.push(Port { name, port });
        }
    }
    let healthcheck = match field(runtime, "healthcheck") {
        None => None,
        Some(value) => {
            let path = "runtime.healthcheck";
            let fields = mapping(value, path, &["type", "port", "path", "expected_status"])?;
            let port = string(required(fields, path, "port")?, "runtime.healthcheck.port")?;
            if !ports.iter().any(|known| known.name == port) {
                return Err(format!("{path}.port: {port} is not a name in runtime.ports"));
            }
            match string(required(fields, path, "type")?, "runtime.healthcheck.type")?.as_str() {
                "http" => {
                    let probe = string(required(fields, path, "path")?, "runtime.healthcheck.path")?;
                    if !probe.starts_with('/') || probe.chars().any(char::is_control) {
                        return Err(format!("{path}.path: expected an absolute URL path starting with /"));
                    }
                    let status = integer(
                        required(fields, path, "expected_status")?,
                        "runtime.healthcheck.expected_status",
                        100..=599,
                    )?;
                    Some(Healthcheck::Http {
                        port,
                        path: probe,
                        expected_status: u16::try_from(status).unwrap_or_default(),
                    })
                }
                "tcp" => {
                    for name in ["path", "expected_status"] {
                        if field(fields, name).is_some() {
                            return Err(format!("{path}.{name}: only for type http"));
                        }
                    }
                    Some(Healthcheck::Tcp { port })
                }
                _ => return Err(format!("{path}.type: expected http or tcp")),
            }
        }
    };
    Ok((ports, healthcheck))
}

fn persistence_of(value: &Yaml) -> Result<(Vec<String>, Vec<Mapping>), String> {
    let path = "persistence";
    let fields = mapping(value, path, &["mount", "directories", "mappings"])?;
    if string(required(fields, path, "mount")?, "persistence.mount")? != MOUNT {
        return Err(format!("persistence.mount: must be exactly {MOUNT}"));
    }
    let mut directories: Vec<String> = Vec::new();
    if let Some(entries) = field(fields, "directories") {
        for (index, entry) in list(entries, "persistence.directories")?.iter().enumerate() {
            let directory = string(entry, &format!("persistence.directories[{index}]"))?;
            if !valid_relative(&directory) {
                return Err(format!(
                    "persistence.directories[{index}]: expected a relative path inside the mount without .."
                ));
            }
            if directories.contains(&directory) {
                return Err(format!("persistence.directories[{index}]: duplicate {directory}"));
            }
            directories.push(directory);
        }
    }
    let mut mappings = Vec::new();
    if let Some(entries) = field(fields, "mappings") {
        for (index, entry) in list(entries, "persistence.mappings")?.iter().enumerate() {
            let at = format!("persistence.mappings[{index}]");
            let pair = mapping(entry, &at, &["source", "destination"])?;
            let source = string(required(pair, &at, "source")?, &join(&at, "source"))?;
            if !valid_absolute(&source) || within(&source, MOUNT) {
                return Err(format!("{at}.source: expected an absolute path outside {MOUNT}"));
            }
            let destination = string(required(pair, &at, "destination")?, &join(&at, "destination"))?;
            if !valid_relative(&destination) || !directories.iter().any(|directory| within(&destination, directory)) {
                return Err(format!(
                    "{at}.destination: expected one of persistence.directories or a path below one"
                ));
            }
            mappings.push(Mapping { source, destination });
        }
    }
    Ok((directories, mappings))
}
