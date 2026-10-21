//! The vendor snippets on the nodes.

use std::collections::BTreeMap;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use sha2::{Digest, Sha256};

use crate::InstallError;
use crate::probe::Node;
use crate::root::{Output, Root};
use crate::shell::quote;
use crate::state::State;

/// The snippets, as the repository has them in `deploy/cloud-init/`.
pub const SNIPPETS: [(&str, &str); 2] = [
    (
        "homestack-vendor-create.yaml",
        include_str!("../../../deploy/cloud-init/homestack-vendor-create.yaml"),
    ),
    (
        "homestack-vendor-keep.yaml",
        include_str!("../../../deploy/cloud-init/homestack-vendor-keep.yaml"),
    ),
];

/// The bytes of a chunk sent in one command: a pane takes a line of at most
/// 4095 characters, and base64 grows the text by a third.
const CHUNK: usize = 1500;

pub fn content(name: &str) -> Option<&'static str> {
    SNIPPETS.iter().find(|(known, _)| *known == name).map(|(_, text)| *text)
}

pub fn sum(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Run `command` as root on `node`: here, or over the node's own root SSH.
pub fn on_node(root: &mut dyn Root, local: &str, node: &Node, command: &str) -> Result<Output, InstallError> {
    root.run(&node_command(local, node, command))
}

/// `command` as the access on `local` runs it on `node`: as it is, or
/// through the node's own root SSH (standard input passes through).
#[must_use]
pub fn node_command(local: &str, node: &Node, command: &str) -> String {
    if node.name == local {
        command.to_owned()
    } else {
        format!(
            "ssh -o BatchMode=yes -o ConnectTimeout=10 -o HostKeyAlias={} root@{} {}",
            quote(&node.name),
            quote(&node.ip),
            quote(command)
        )
    }
}

/// Where the snippet is on the nodes (`pvesm path`, run on the local node).
pub fn path(root: &mut dyn Root, storage: &str, name: &str) -> Result<String, InstallError> {
    let output = root.run(&format!("pvesm path {}", quote(&format!("{storage}:snippets/{name}"))))?;
    let path = output.stdout.trim().to_owned();
    if output.code != 0 || path.is_empty() {
        return Err(InstallError::failed(format!(
            "cannot tell where snippet {name} goes in storage {storage}: {}",
            output.detail()
        )));
    }
    Ok(path)
}

/// The nodes that hold snippets of `storage`.
pub fn nodes_of(state: &State, storage: &str) -> Vec<Node> {
    let Some(found) = state.storages.get(storage) else {
        return Vec::new();
    };
    if found.shared {
        return state.topology.local_node().cloned().into_iter().collect();
    }
    state
        .topology
        .nodes
        .iter()
        .filter(|node| found.nodes.is_empty() || found.nodes.contains(&node.name))
        .cloned()
        .collect()
}

/// The checksums of the snippets that exist on `node`, by file name.
pub fn sums_on(
    root: &mut dyn Root,
    local: &str,
    node: &Node,
    paths: &BTreeMap<&'static str, String>,
) -> Result<BTreeMap<String, String>, InstallError> {
    let files: Vec<String> = paths.values().map(|path| quote(path)).collect();
    let command = format!(
        "for f in {}; do if [ -f \"$f\" ]; then sha256sum \"$f\"; fi; done",
        files.join(" ")
    );
    let output = on_node(root, local, node, &command)?;
    if output.code != 0 {
        return Err(InstallError::failed(output.detail()));
    }
    let mut found = BTreeMap::new();
    for line in output.stdout.lines() {
        if let Some((sum, file)) = line.split_once(char::is_whitespace) {
            found.insert(file.trim().to_owned(), sum.to_owned());
        }
    }
    Ok(found)
}

/// Write the snippet `name` (path `path`) on `node`: in chunks, to a file
/// next to it, then moved into place and checked.
pub fn write(root: &mut dyn Root, local: &str, node: &Node, name: &str, path: &str) -> Result<(), InstallError> {
    let text = content(name).ok_or_else(|| InstallError::failed(format!("no embedded snippet {name}")))?;
    let new = quote(&format!("{path}.homestack-new"));
    let dir = path.rsplit_once('/').map_or(".", |(dir, _)| dir);
    let steps = std::iter::once(format!("(umask 022; mkdir -p {} && : > {new})", quote(dir))).chain(
        text.as_bytes()
            .chunks(CHUNK)
            .map(|chunk| format!("printf %s {} | base64 -d >> {new}", quote(&STANDARD.encode(chunk)))),
    );
    for command in steps {
        let output = on_node(root, local, node, &command)?;
        if output.code != 0 {
            return Err(InstallError::failed(format!(
                "writing {name} on {}: {}",
                node.name,
                output.detail()
            )));
        }
    }
    let output = on_node(
        root,
        local,
        node,
        &format!("sha256sum {new} && mv -f {new} {}", quote(path)),
    )?;
    let written = output.stdout.split_whitespace().next().unwrap_or_default().to_owned();
    if output.code != 0 || written != sum(text) {
        // Do not leave a half-written file next to the real one.
        let _ = on_node(root, local, node, &format!("rm -f {new}"));
        return Err(InstallError::failed(format!(
            "{name} on {} came out different from what was sent ({})",
            node.name,
            if output.code == 0 { "checksum" } else { "move" }
        )));
    }
    Ok(())
}

pub fn remove(root: &mut dyn Root, local: &str, node: &Node, path: &str) -> Result<(), InstallError> {
    let output = on_node(root, local, node, &format!("rm -f {}", quote(path)))?;
    if output.code != 0 {
        return Err(InstallError::failed(format!(
            "removing {path} on {}: {}",
            node.name,
            output.detail()
        )));
    }
    Ok(())
}
