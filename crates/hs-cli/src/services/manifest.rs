//! Service manifest schema 1 (`.agents/skills/homestack-service/manifest-schema.md`):
//! one `.homestack/<file>.yaml` document describes one service. A file that
//! is not one YAML mapping is refused. Otherwise every missing or unknown
//! top-level section, unknown field and invalid value is collected as an
//! [`Issue`] at its path: the file is still shown, but it yields no
//! deployable [`Manifest`].
//! Nothing in a manifest is ever executed or interpolated.

use yaml_rust2::{Yaml, YamlLoader};

/// The largest manifest file read.
pub const MAX_BYTES: u64 = 64 * 1024;
/// The only persistent mount schema 1 allows.
pub const MOUNT: &str = "/home/app";
/// The top-level sections, in the order they are shown.
pub const SECTIONS: [&str; 8] = [
    "schema",
    "id",
    "name",
    "image",
    "runtime",
    "persistence",
    "deployment",
    "exposure",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub image_repository: String,
    pub resources: Resources,
    pub ports: Vec<Port>,
    pub healthcheck: Option<Healthcheck>,
    /// `None`: an empty `persistence`, no home volume.
    pub persistence: Option<Persistence>,
    pub deployment: Deployment,
    /// `None`: an empty `exposure`, not published through Pangolin.
    pub pangolin: Option<Pangolin>,
}

/// Default sizes of one instance, in MiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resources {
    pub cores: u32,
    pub memory_mib: u32,
    pub swap_mib: u32,
    pub rootfs_mib: u32,
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
        heartbeat_seconds: u32,
    },
    /// Only that the port accepts connections, not application health.
    Tcp { port: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Persistence {
    pub size_mib: u32,
    pub directories: Vec<String>,
    pub mappings: Vec<Mapping>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapping {
    pub source: String,
    pub destination: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deployment {
    /// The instances' host name: alone for one instance, `<hostname>-<n>`
    /// for several; the manifest `id` when not declared.
    pub hostname: String,
    pub instances: u32,
    pub placement: Placement,
    pub state_model: StateModel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// One instance per node.
    Spread,
    /// All instances on one node.
    Single,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pangolin {
    pub port: String,
    pub name: String,
    pub subdomain: String,
}

/// A problem at `path` (field names and list indexes from the top).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub path: Vec<String>,
    pub message: String,
}

/// A file whose top level is valid: the document as read, every problem
/// inside its sections, and the manifest when there are none.
#[derive(Debug, Clone)]
pub struct Checked {
    /// `None` when missing or invalid.
    pub id: Option<String>,
    pub name: Option<String>,
    pub document: Yaml,
    pub issues: Vec<Issue>,
    pub manifest: Option<Manifest>,
}

type Fields<'a> = &'a yaml_rust2::yaml::Hash;

fn field<'a>(fields: Fields<'a>, name: &str) -> Option<&'a Yaml> {
    fields.get(&Yaml::String(name.to_owned()))
}

fn at(path: &[String], name: &str) -> Vec<String> {
    let mut path = path.to_vec();
    path.push(name.to_owned());
    path
}

/// Collects the issues found inside the sections.
#[derive(Default)]
struct Check {
    issues: Vec<Issue>,
}

impl Check {
    fn fail(&mut self, path: &[String], message: impl Into<String>) {
        self.issues.push(Issue {
            path: path.to_vec(),
            message: message.into(),
        });
    }

    /// The mapping at `path`; every key outside `allowed` is an issue.
    fn mapping<'a>(&mut self, value: &'a Yaml, path: &[String], allowed: &[&str]) -> Option<Fields<'a>> {
        let Yaml::Hash(fields) = value else {
            self.fail(path, "expected a mapping");
            return None;
        };
        for key in fields.keys() {
            match key.as_str() {
                Some(name) if allowed.contains(&name) => {}
                Some(name) => self.fail(&at(path, name), "unknown field"),
                None => self.fail(path, "field names must be strings"),
            }
        }
        Some(fields)
    }

    /// Field `name`; a missing one is an issue of its mapping.
    fn required<'a>(&mut self, fields: Fields<'a>, path: &[String], name: &str) -> Option<&'a Yaml> {
        let value = field(fields, name);
        if value.is_none() {
            self.fail(path, format!("{name}: required"));
        }
        value
    }

    fn string(&mut self, value: &Yaml, path: &[String]) -> Option<String> {
        let text = value.as_str().map(str::to_owned);
        if text.is_none() {
            self.fail(path, "expected a string");
        }
        text
    }

    fn integer(&mut self, value: &Yaml, path: &[String], range: std::ops::RangeInclusive<i64>) -> Option<i64> {
        let Yaml::Integer(number) = value else {
            self.fail(path, "expected an integer");
            return None;
        };
        if !range.contains(number) {
            self.fail(path, format!("must be {}–{}", range.start(), range.end()));
            return None;
        }
        Some(*number)
    }

    /// A size `<n>M` or `<n>G` in MiB within `range`.
    fn size(&mut self, value: &Yaml, path: &[String], range: std::ops::RangeInclusive<u32>) -> Option<u32> {
        let parsed = value.as_str().and_then(|text| {
            let (digits, unit) = text.split_at(text.len().checked_sub(1)?);
            let factor: u64 = match unit {
                "M" => 1,
                "G" => 1024,
                _ => return None,
            };
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            digits.parse::<u64>().ok()?.checked_mul(factor)
        });
        let Some(mib) = parsed else {
            self.fail(path, "expected a size such as 512M or 2G");
            return None;
        };
        match u32::try_from(mib).ok().filter(|mib| range.contains(mib)) {
            Some(mib) => Some(mib),
            None => {
                self.fail(
                    path,
                    format!("must be {} to {}", show_mib(*range.start()), show_mib(*range.end())),
                );
                None
            }
        }
    }

    fn list<'a>(&mut self, value: &'a Yaml, path: &[String]) -> Option<&'a [Yaml]> {
        let items = value.as_vec().map(Vec::as_slice);
        if items.is_none() {
            self.fail(path, "expected a list");
        }
        items
    }

    /// One of `choices` (YAML text) mapped to its value.
    fn choice<T: Copy>(&mut self, value: &Yaml, path: &[String], choices: &[(&str, T)]) -> Option<T> {
        let text = self.string(value, path)?;
        let found = choices.iter().find(|(name, _)| *name == text).map(|(_, value)| *value);
        if found.is_none() {
            let names: Vec<&str> = choices.iter().map(|(name, _)| *name).collect();
            self.fail(path, format!("expected {}", names.join(", ")));
        }
        found
    }
}

/// A size in MiB as the manifest writes it.
#[must_use]
pub fn show_mib(mib: u32) -> String {
    if mib >= 1024 && mib.is_multiple_of(1024) {
        format!("{}G", mib / 1024)
    } else {
        format!("{mib}M")
    }
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

/// A DNS label: `^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$`
fn valid_subdomain(text: &str) -> bool {
    (1..=63).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !text.starts_with('-')
        && !text.ends_with('-')
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

/// Read one manifest file's text: `Err` when it is not one YAML mapping.
pub fn parse(text: &str) -> Result<Checked, String> {
    if text.len() as u64 > MAX_BYTES {
        return Err(format!("larger than {MAX_BYTES} bytes"));
    }
    let mut documents = YamlLoader::load_from_str(text).map_err(|error| format!("invalid YAML: {error}"))?;
    if documents.len() != 1 {
        return Err(format!("expected exactly one YAML document, found {}", documents.len()));
    }
    let document = documents.remove(0);
    let Yaml::Hash(top) = &document else {
        return Err("expected a mapping of the top-level sections".to_owned());
    };
    let mut check = Check::default();
    for key in top.keys() {
        match key.as_str() {
            Some(name) if SECTIONS.contains(&name) => {}
            Some(name) => check.fail(&[name.to_owned()], "unknown section"),
            None => return Err("top-level field names must be strings".to_owned()),
        }
    }
    for name in SECTIONS {
        if field(top, name).is_none() {
            check.fail(&[name.to_owned()], "missing section");
        }
    }
    if field(top, "schema").is_some_and(|schema| *schema != Yaml::Integer(1)) {
        check.fail(&["schema".to_owned()], "must be 1");
    }
    let id = field(top, "id").and_then(|value| {
        let id = value.as_str().filter(|id| valid_id(id)).map(str::to_owned);
        if id.is_none() {
            check.fail(&["id".to_owned()], "must match ^[a-z0-9][a-z0-9-]{0,62}$");
        }
        id
    });
    let name = field(top, "name").and_then(|value| {
        let name = value
            .as_str()
            .filter(|name| (1..=64).contains(&name.chars().count()))
            .map(str::to_owned);
        if name.is_none() {
            check.fail(&["name".to_owned()], "must be 1–64 characters");
        }
        name
    });
    let image = field(top, "image").and_then(|value| image_of(&mut check, value));
    let runtime = field(top, "runtime").and_then(|value| runtime_of(&mut check, value));
    let persistence = field(top, "persistence").and_then(|value| persistence_of(&mut check, value));
    let deployment = field(top, "deployment").and_then(|value| deployment_of(&mut check, value, id.as_deref()));
    let pangolin = field(top, "exposure").and_then(|value| exposure_of(&mut check, value, runtime.as_ref()));
    if let (Some(deployment), Some(persistence)) = (&deployment, &persistence) {
        let path = vec!["deployment".to_owned(), "state_model".to_owned()];
        match (deployment.state_model, persistence) {
            (StateModel::Local, None) => check.fail(&path, "local requires a non-empty persistence"),
            (StateModel::Stateless, Some(_)) => check.fail(&path, "stateless requires an empty persistence"),
            _ => {}
        }
    }
    let manifest = match (
        id.clone(),
        name.clone(),
        image,
        runtime,
        persistence,
        deployment,
        pangolin,
    ) {
        (
            Some(id),
            Some(name),
            Some(image_repository),
            Some((resources, ports, healthcheck)),
            Some(persistence),
            Some(deployment),
            Some(pangolin),
        ) if check.issues.is_empty() => Some(Manifest {
            id,
            name,
            image_repository,
            resources,
            ports,
            healthcheck,
            persistence,
            deployment,
            pangolin,
        }),
        _ => None,
    };
    Ok(Checked {
        id,
        name,
        document,
        issues: check.issues,
        manifest,
    })
}

fn image_of(check: &mut Check, value: &Yaml) -> Option<String> {
    let path = vec!["image".to_owned()];
    let fields = check.mapping(value, &path, &["repository"])?;
    let at = at(&path, "repository");
    let value = check.required(fields, &path, "repository")?;
    let repository = check.string(value, &at)?;
    if !valid_repository(&repository) {
        check.fail(&at, "expected a lowercase OCI repository without tag or digest");
        return None;
    }
    Some(repository)
}

type Runtime = (Resources, Vec<Port>, Option<Healthcheck>);

fn runtime_of(check: &mut Check, value: &Yaml) -> Option<Runtime> {
    let path = vec!["runtime".to_owned()];
    let runtime = check.mapping(value, &path, &["resources", "ports", "healthcheck"])?;
    let resources = check
        .required(runtime, &path, "resources")
        .and_then(|value| resources_of(check, value, &at(&path, "resources")));
    let ports = match field(runtime, "ports") {
        None => Some(Vec::new()),
        Some(value) => ports_of(check, value, &at(&path, "ports")),
    };
    let healthcheck = match (field(runtime, "healthcheck"), &ports) {
        (None, _) => Some(None),
        (Some(value), Some(ports)) => healthcheck_of(check, value, &at(&path, "healthcheck"), ports).map(Some),
        (Some(_), None) => None,
    };
    Some((resources?, ports?, healthcheck?))
}

fn resources_of(check: &mut Check, value: &Yaml, path: &[String]) -> Option<Resources> {
    let fields = check.mapping(value, path, &["cores", "memory", "swap", "rootfs"])?;
    let cores = check
        .required(fields, path, "cores")
        .and_then(|value| check.integer(value, &at(path, "cores"), 1..=64));
    let mut size = |name: &str, range| {
        check
            .required(fields, path, name)
            .and_then(|value| check.size(value, &at(path, name), range))
    };
    let memory_mib = size("memory", 64..=1024 * 1024);
    let swap_mib = size("swap", 0..=1024 * 1024);
    let rootfs_mib = size("rootfs", 64..=1024 * 1024);
    Some(Resources {
        cores: u32::try_from(cores?).ok()?,
        memory_mib: memory_mib?,
        swap_mib: swap_mib?,
        rootfs_mib: rootfs_mib?,
    })
}

fn ports_of(check: &mut Check, value: &Yaml, path: &[String]) -> Option<Vec<Port>> {
    let mut ports: Vec<Port> = Vec::new();
    let mut valid = true;
    for (index, entry) in check.list(value, path)?.iter().enumerate() {
        let path = at(path, &index.to_string());
        match port_of(check, entry, &path) {
            Some(port) if ports.iter().any(|known| known.name == port.name) => {
                check.fail(&at(&path, "name"), format!("duplicate port name {}", port.name));
                valid = false;
            }
            Some(port) if ports.iter().any(|known| known.port == port.port) => {
                check.fail(&at(&path, "port"), format!("duplicate port {}", port.port));
                valid = false;
            }
            Some(port) => ports.push(port),
            None => valid = false,
        }
    }
    valid.then_some(ports)
}

fn port_of(check: &mut Check, entry: &Yaml, path: &[String]) -> Option<Port> {
    let fields = check.mapping(entry, path, &["name", "port", "protocol"])?;
    let name = check
        .required(fields, path, "name")
        .and_then(|value| check.string(value, &at(path, "name")));
    let name = match name {
        Some(name) if !valid_port_name(&name) => {
            check.fail(&at(path, "name"), "must match ^[a-z][a-z0-9-]{0,31}$");
            None
        }
        name => name,
    };
    let port = check
        .required(fields, path, "port")
        .and_then(|value| check.integer(value, &at(path, "port"), 1..=65535));
    let protocol = check
        .required(fields, path, "protocol")
        .and_then(|value| check.choice(value, &at(path, "protocol"), &[("tcp", ())]));
    protocol?;
    Some(Port {
        name: name?,
        port: u16::try_from(port?).ok()?,
    })
}

fn healthcheck_of(check: &mut Check, value: &Yaml, path: &[String], ports: &[Port]) -> Option<Healthcheck> {
    let fields = check.mapping(
        value,
        path,
        &["type", "port", "path", "expected_status", "heartbeat_seconds"],
    )?;
    let port = check
        .required(fields, path, "port")
        .and_then(|value| check.string(value, &at(path, "port")));
    let port = match port {
        Some(port) if !ports.iter().any(|known| known.name == port) => {
            check.fail(&at(path, "port"), format!("{port} is not a name in runtime.ports"));
            None
        }
        port => port,
    };
    let kind = check
        .required(fields, path, "type")
        .and_then(|value| check.choice(value, &at(path, "type"), &[("http", true), ("tcp", false)]))?;
    if !kind {
        for name in ["path", "expected_status", "heartbeat_seconds"] {
            if field(fields, name).is_some() {
                check.fail(&at(path, name), "only for type http");
            }
        }
        return Some(Healthcheck::Tcp { port: port? });
    }
    let probe = check
        .required(fields, path, "path")
        .and_then(|value| check.string(value, &at(path, "path")));
    let probe = match probe {
        Some(probe) if !probe.starts_with('/') || probe.chars().any(char::is_control) => {
            check.fail(&at(path, "path"), "expected an absolute URL path starting with /");
            None
        }
        probe => probe,
    };
    let status = check
        .required(fields, path, "expected_status")
        .and_then(|value| check.integer(value, &at(path, "expected_status"), 100..=599));
    let heartbeat_seconds = match field(fields, "heartbeat_seconds") {
        Some(value) => check.integer(value, &at(path, "heartbeat_seconds"), 10..=3600)?,
        None => 60,
    };
    Some(Healthcheck::Http {
        port: port?,
        path: probe?,
        expected_status: u16::try_from(status?).ok()?,
        heartbeat_seconds: u32::try_from(heartbeat_seconds).ok()?,
    })
}

/// `Some(None)` for an empty section.
fn persistence_of(check: &mut Check, value: &Yaml) -> Option<Option<Persistence>> {
    if value.is_null() {
        return Some(None);
    }
    let path = vec!["persistence".to_owned()];
    let fields = check.mapping(value, &path, &["mount", "size", "directories", "mappings"])?;
    let mount = check
        .required(fields, &path, "mount")
        .and_then(|value| check.string(value, &at(&path, "mount")));
    if mount.as_deref().is_some_and(|mount| mount != MOUNT) {
        check.fail(&at(&path, "mount"), format!("must be exactly {MOUNT}"));
    }
    let size_mib = check
        .required(fields, &path, "size")
        .and_then(|value| check.size(value, &at(&path, "size"), 1..=16_384 * 1024));
    let mut directories: Vec<String> = Vec::new();
    if let Some(entries) = field(fields, "directories").and_then(|value| check.list(value, &at(&path, "directories"))) {
        for (index, entry) in entries.iter().enumerate() {
            let at = at(&at(&path, "directories"), &index.to_string());
            let Some(directory) = check.string(entry, &at) else {
                continue;
            };
            if !valid_relative(&directory) {
                check.fail(&at, "expected a relative path inside the mount without ..");
            } else if directories.contains(&directory) {
                check.fail(&at, format!("duplicate {directory}"));
            } else {
                directories.push(directory);
            }
        }
    }
    let mut mappings = Vec::new();
    if let Some(entries) = field(fields, "mappings").and_then(|value| check.list(value, &at(&path, "mappings"))) {
        for (index, entry) in entries.iter().enumerate() {
            let at_entry = at(&at(&path, "mappings"), &index.to_string());
            let Some(pair) = check.mapping(entry, &at_entry, &["source", "destination"]) else {
                continue;
            };
            let source = check
                .required(pair, &at_entry, "source")
                .and_then(|value| check.string(value, &at(&at_entry, "source")));
            if let Some(source) = &source
                && (!valid_absolute(source) || within(source, MOUNT))
            {
                check.fail(
                    &at(&at_entry, "source"),
                    format!("expected an absolute path outside {MOUNT}"),
                );
            }
            let destination = check
                .required(pair, &at_entry, "destination")
                .and_then(|value| check.string(value, &at(&at_entry, "destination")));
            if let Some(destination) = &destination
                && (!valid_relative(destination) || !directories.iter().any(|directory| within(destination, directory)))
            {
                check.fail(
                    &at(&at_entry, "destination"),
                    "expected one of persistence.directories or a path below one",
                );
            }
            if let (Some(source), Some(destination)) = (source, destination) {
                mappings.push(Mapping { source, destination });
            }
        }
    }
    mount?;
    Some(Some(Persistence {
        size_mib: size_mib?,
        directories,
        mappings,
    }))
}

fn deployment_of(check: &mut Check, value: &Yaml, id: Option<&str>) -> Option<Deployment> {
    let path = vec!["deployment".to_owned()];
    let fields = check.mapping(value, &path, &["hostname", "instances", "placement", "state_model"])?;
    let hostname = match field(fields, "hostname") {
        None => id.map(str::to_owned),
        Some(value) => match check.string(value, &at(&path, "hostname")) {
            Some(name) if valid_subdomain(&name) => Some(name),
            Some(_) => {
                check.fail(&at(&path, "hostname"), "expected a DNS label: a-z, 0-9 and inner -");
                None
            }
            None => None,
        },
    };
    let instances = check
        .required(fields, &path, "instances")
        .and_then(|value| check.integer(value, &at(&path, "instances"), 1..=16));
    let placement = match field(fields, "placement") {
        None => Some(Placement::Spread),
        Some(value) => check.choice(
            value,
            &at(&path, "placement"),
            &[("spread", Placement::Spread), ("single", Placement::Single)],
        ),
    };
    let state_model = check.required(fields, &path, "state_model").and_then(|value| {
        check.choice(
            value,
            &at(&path, "state_model"),
            &[
                ("stateless", StateModel::Stateless),
                ("local", StateModel::Local),
                ("external", StateModel::External),
            ],
        )
    });
    Some(Deployment {
        hostname: hostname?,
        instances: u32::try_from(instances?).ok()?,
        placement: placement?,
        state_model: state_model?,
    })
}

/// `Some(None)` for an empty section.
fn exposure_of(check: &mut Check, value: &Yaml, runtime: Option<&Runtime>) -> Option<Option<Pangolin>> {
    if value.is_null() {
        return Some(None);
    }
    let path = vec!["exposure".to_owned()];
    let fields = check.mapping(value, &path, &["pangolin"])?;
    let path = at(&path, "pangolin");
    let pangolin = check.required(fields, &["exposure".to_owned()], "pangolin")?;
    let fields = check.mapping(pangolin, &path, &["port", "name", "subdomain"])?;
    let port = check
        .required(fields, &path, "port")
        .and_then(|value| check.string(value, &at(&path, "port")));
    if let (Some(port), Some((_, ports, healthcheck))) = (&port, runtime) {
        if !ports.iter().any(|known| known.name == *port) {
            check.fail(&at(&path, "port"), format!("{port} is not a name in runtime.ports"));
        } else if !matches!(healthcheck, Some(Healthcheck::Http { port: probe, .. }) if probe == port) {
            check.fail(&at(&path, "port"), "requires an http healthcheck on this port");
        }
    }
    let name = check
        .required(fields, &path, "name")
        .and_then(|value| check.string(value, &at(&path, "name")));
    if name
        .as_ref()
        .is_some_and(|name| !(1..=64).contains(&name.chars().count()))
    {
        check.fail(&at(&path, "name"), "must be 1–64 characters");
    }
    let subdomain = check
        .required(fields, &path, "subdomain")
        .and_then(|value| check.string(value, &at(&path, "subdomain")));
    if subdomain.as_ref().is_some_and(|subdomain| !valid_subdomain(subdomain)) {
        check.fail(&at(&path, "subdomain"), "expected a DNS label: a-z, 0-9 and inner -");
    }
    Some(Some(Pangolin {
        port: port?,
        name: name?,
        subdomain: subdomain?,
    }))
}
