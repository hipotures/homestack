//! The first look at the access: is it root, is it Proxmox VE, and is the
//! node alone or in a cluster. Nothing is read from the cluster beyond that
//! and nothing is changed.

use serde_json::Value;

use crate::InstallError;
use crate::root::Root;

/// One node of the cluster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub name: String,
    pub ip: String,
    pub online: bool,
    pub local: bool,
}

/// A single node or a cluster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Topology {
    pub cluster: bool,
    /// The cluster's quorum; `None` for a single node.
    pub quorate: Option<bool>,
    pub nodes: Vec<Node>,
}

impl Topology {
    /// The node the access is on.
    #[must_use]
    pub fn local_node(&self) -> Option<&Node> {
        self.nodes.iter().find(|node| node.local)
    }

    /// "single node" or "cluster of N nodes (a b c)".
    #[must_use]
    pub fn describe(&self) -> String {
        if self.cluster {
            let names: Vec<&str> = self.nodes.iter().map(|node| node.name.as_str()).collect();
            format!("cluster of {} nodes ({})", names.len(), names.join(" "))
        } else {
            "single node".to_owned()
        }
    }
}

/// What the access turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Access {
    /// The node's own name.
    pub host: String,
    /// Proxmox VE's version of the node.
    pub version: String,
    pub topology: Topology,
}

/// A JSON flag of Proxmox: `1`, `0`, `true` or `false`.
pub(crate) fn flag(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_i64().is_some_and(|n| n != 0),
        Some(Value::String(text)) => text == "1" || text == "true",
        _ => false,
    }
}

pub(crate) fn text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => number.to_string(),
        _ => String::new(),
    }
}

/// `pvesh get PATH` as JSON.
pub(crate) fn get(root: &mut dyn Root, path: &str, extra: &str) -> Result<Value, InstallError> {
    let command = format!("pvesh get {path}{extra} --output-format json");
    let output = root.run(&command)?;
    if output.code != 0 {
        return Err(InstallError::failed(format!("`{command}` failed: {}", output.detail())));
    }
    serde_json::from_str(output.stdout.trim()).map_err(|error| {
        InstallError::failed(format!(
            "`{command}` did not answer JSON ({error}); the answer started with {:?}",
            { output.stdout.chars().take(80).collect::<String>() }
        ))
    })
}

/// The nodes of `GET /cluster/status`.
pub(crate) fn topology(status: &Value) -> Topology {
    let entries = status.as_array().map_or(&[][..], Vec::as_slice);
    let cluster = entries.iter().find(|entry| entry["type"] == "cluster");
    let nodes = entries
        .iter()
        .filter(|entry| entry["type"] == "node")
        .map(|entry| Node {
            name: text(entry.get("name")),
            ip: text(entry.get("ip")),
            online: flag(entry.get("online")),
            local: flag(entry.get("local")),
        })
        .collect();
    Topology {
        cluster: cluster.is_some(),
        quorate: cluster.map(|entry| flag(entry.get("quorate"))),
        nodes,
    }
}

/// Check the access in this order: root, Proxmox VE, cluster or not.
pub fn probe(root: &mut dyn Root) -> Result<Access, InstallError> {
    let where_ = root.describe();
    let output = root.run(
        "printf 'uid=%s\\nhost=%s\\n' \"$(id -u)\" \"$(hostname -s)\"; \
         for t in pvesh pveum pvesm; do command -v \"$t\" > /dev/null 2>&1 && echo \"has=$t\"; done; \
         echo \"os=$(. /etc/os-release 2> /dev/null && echo \"$PRETTY_NAME\")\"",
    )?;
    if output.code != 0 {
        return Err(InstallError::failed(format!(
            "the access to {where_} does not run commands: {}",
            output.detail()
        )));
    }
    let field = |key: &str| {
        output
            .stdout
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{key}=")))
            .map(|value| value.trim().to_owned())
    };
    let uid = field("uid").unwrap_or_default();
    let host = field("host").unwrap_or_default();
    if uid != "0" {
        return Err(InstallError::failed(format!(
            "the access to {where_} is not root (uid {uid}) on {host}: install and uninstall need root once"
        )));
    }
    let has: Vec<String> = output
        .stdout
        .lines()
        .filter_map(|line| line.strip_prefix("has="))
        .map(str::to_owned)
        .collect();
    let missing: Vec<&str> = ["pvesh", "pveum", "pvesm"]
        .into_iter()
        .filter(|tool| !has.iter().any(|found| found == tool))
        .collect();
    if !missing.is_empty() {
        let os = field("os")
            .filter(|os| !os.is_empty())
            .map(|os| format!(" ({os})"))
            .unwrap_or_default();
        return Err(InstallError::failed(format!(
            "{host}{os} is not a Proxmox VE node: {} not found",
            missing.join(", ")
        )));
    }
    let version = get(root, "/version", "").map_err(|error| {
        InstallError::failed(format!(
            "{host} has the Proxmox tools but pvesh does not answer: {error}"
        ))
    })?;
    let version = text(version.get("version"));
    let status = get(root, "/cluster/status", "")?;
    let topology = topology(&status);
    if topology.local_node().is_none() {
        return Err(InstallError::failed(
            "cannot tell which node this is (pvesh get /cluster/status)",
        ));
    }
    Ok(Access {
        host,
        version,
        topology,
    })
}
