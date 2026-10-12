//! In-memory cluster state behind the simulated API.

use std::collections::{BTreeMap, HashMap};

use hs_model::NodeName;

use crate::guest::ExecResult;
use crate::ops::Op;
use crate::scenario::{
    ClusterSpec, FaultSpec, NodeSpec, StorageSpec, TaskSpec, TokenSpec, VersionSpec, VmSpec, VolumeSpec,
};

pub struct ClusterState {
    pub version: VersionSpec,
    pub nodes: BTreeMap<NodeName, NodeSpec>,
    pub storages: Vec<StorageSpec>,
    pub volumes: Vec<VolumeSpec>,
    pub vms: BTreeMap<u32, VmSpec>,
    /// `exec` results by (VMID, PID).
    pub exec: HashMap<(u32, u32), ExecResult>,
    next_pid: u32,
    tokens: Vec<TokenSpec>,
    tasks: Vec<Task>,
    faults: Vec<(Op, FaultSpec)>,
    calls: HashMap<Op, u32>,
    pub ops: Vec<Op>,
}

pub struct Task {
    pub upid: String,
    pub spec: TaskSpec,
    pub pid: u32,
    pub pstart: u64,
    pub starttime: u64,
    pub polls: u32,
}

/// Simulated start time of the first task; later tasks start one second apart.
const FIRST_TASK_START: u64 = 1_790_000_000;

impl ClusterState {
    pub fn new(spec: ClusterSpec) -> Result<Self, String> {
        let mut nodes = BTreeMap::new();
        for node in spec.nodes {
            let name = node.name.clone();
            if nodes.insert(name.clone(), node).is_some() {
                return Err(format!("node {name} is defined twice"));
            }
        }
        let mut faults = Vec::new();
        for fault in spec.faults {
            let op = fault.op.parse::<Op>().map_err(|error| error.to_string())?;
            if fault.occurrence == 0 {
                return Err(format!("fault for {op}: occurrence counts from 1"));
            }
            faults.push((op, fault));
        }
        let known_storage = |id: &str| spec.storages.iter().any(|storage| storage.storage == id);
        for node in nodes.values() {
            if let Some(storage) = node.storages.iter().find(|s| !known_storage(&s.storage)) {
                return Err(format!(
                    "node {}: storage {} has no [[storages]] definition",
                    node.name, storage.storage
                ));
            }
        }
        for volume in &spec.volumes {
            if !nodes.contains_key(&volume.node) || !known_storage(&volume.storage) {
                return Err(format!("volume {}: unknown node or storage", volume.volid));
            }
        }
        let mut vms = BTreeMap::new();
        for vm in spec.vms {
            if !nodes.contains_key(&vm.node) {
                return Err(format!("VM {} is on unknown node {}", vm.vmid, vm.node));
            }
            let vmid = vm.vmid;
            if vms.insert(vmid, vm).is_some() {
                return Err(format!("VM {vmid} is defined twice"));
            }
        }
        let mut state = Self {
            version: spec.version,
            nodes,
            storages: spec.storages,
            volumes: spec.volumes,
            vms,
            exec: HashMap::new(),
            next_pid: 3000,
            tokens: spec.tokens,
            tasks: Vec::new(),
            faults,
            calls: HashMap::new(),
            ops: Vec::new(),
        };
        for task in spec.tasks {
            if !state.nodes.contains_key(&task.node) {
                return Err(format!("task on unknown node {}", task.node));
            }
            state.add_task(task);
        }
        Ok(state)
    }

    /// The token whose `PVEAPIToken=` header value this is.
    pub fn authenticate(&self, header_value: &str) -> Option<&TokenSpec> {
        let (id, secret) = header_value.strip_prefix("PVEAPIToken=")?.split_once('=')?;
        self.tokens
            .iter()
            .find(|token| token.id == id && token.secret == secret)
    }

    /// Record a call and return the injected failure for it, if any.
    pub fn record(&mut self, op: Op) -> Option<(u16, String)> {
        let count = self.calls.entry(op.clone()).or_insert(0);
        *count += 1;
        let count = *count;
        let fault = self
            .faults
            .iter()
            .find(|(fault_op, fault)| *fault_op == op && fault.occurrence == count)
            .map(|(_, fault)| (fault.status, fault.message.clone()));
        self.ops.push(op);
        fault
    }

    pub fn add_task(&mut self, spec: TaskSpec) -> String {
        let index = u32::try_from(self.tasks.len()).unwrap_or(u32::MAX);
        let pid = 100_000_u32.saturating_add(index);
        let starttime = FIRST_TASK_START.saturating_add(u64::from(index));
        let pstart = 5_000_000_u64.saturating_add(u64::from(index));
        let upid = format!(
            "UPID:{}:{pid:08X}:{pstart:08X}:{starttime:08X}:{}:{}:{}:",
            spec.node, spec.kind, spec.id, spec.user
        );
        self.tasks.push(Task {
            upid: upid.clone(),
            spec,
            pid,
            pstart,
            starttime,
            polls: 0,
        });
        upid
    }

    pub fn task_mut(&mut self, node: &NodeName, upid: &str) -> Option<&mut Task> {
        self.tasks
            .iter_mut()
            .find(|task| task.upid == upid && task.spec.node == *node)
    }

    /// Store an `exec` result and return its PID.
    pub fn add_exec(&mut self, vmid: u32, result: ExecResult) -> u32 {
        let pid = self.next_pid;
        self.next_pid = self.next_pid.saturating_add(1);
        self.exec.insert((vmid, pid), result);
        pid
    }

    pub fn task_upids(&self) -> Vec<String> {
        self.tasks.iter().map(|task| task.upid.clone()).collect()
    }
}

impl Task {
    pub fn done(&self) -> bool {
        self.polls >= self.spec.polls_until_done
    }

    /// Log lines visible now: all of them once the task is done, otherwise a
    /// share proportional to the polls so far.
    pub fn visible_log(&self) -> &[String] {
        let log = &self.spec.log;
        if self.done() {
            return log;
        }
        let total = u64::try_from(log.len()).unwrap_or(u64::MAX);
        let shown = total.saturating_mul(u64::from(self.polls)) / u64::from(self.spec.polls_until_done.max(1));
        let shown = usize::try_from(shown).unwrap_or(log.len()).min(log.len());
        &log[..shown]
    }
}

/// Every privilege of PVE 9.2.21: those of the Administrator role
/// (`GET /access/roles/Administrator`). `*` in a scenario ACL grants them all.
pub const PRIVILEGES: [&str; 47] = [
    "Datastore.Allocate",
    "Datastore.AllocateSpace",
    "Datastore.AllocateTemplate",
    "Datastore.Audit",
    "Group.Allocate",
    "Mapping.Audit",
    "Mapping.Modify",
    "Mapping.Use",
    "Permissions.Modify",
    "Pool.Allocate",
    "Pool.Audit",
    "Realm.Allocate",
    "Realm.AllocateUser",
    "SDN.Allocate",
    "SDN.Audit",
    "SDN.Use",
    "Sys.AccessNetwork",
    "Sys.Audit",
    "Sys.Console",
    "Sys.Incoming",
    "Sys.Modify",
    "Sys.PowerMgmt",
    "Sys.Syslog",
    "User.Modify",
    "VM.Allocate",
    "VM.Audit",
    "VM.Backup",
    "VM.Clone",
    "VM.Config.CDROM",
    "VM.Config.CPU",
    "VM.Config.Cloudinit",
    "VM.Config.Disk",
    "VM.Config.HWType",
    "VM.Config.Memory",
    "VM.Config.Network",
    "VM.Config.Options",
    "VM.Console",
    "VM.GuestAgent.Audit",
    "VM.GuestAgent.FileRead",
    "VM.GuestAgent.FileSystemMgmt",
    "VM.GuestAgent.FileWrite",
    "VM.GuestAgent.Unrestricted",
    "VM.Migrate",
    "VM.PowerMgmt",
    "VM.Replicate",
    "VM.Snapshot",
    "VM.Snapshot.Rollback",
];

/// Whether `token` holds any of `privileges` on `path`.
pub fn has_any_privilege(token: &TokenSpec, path: &str, privileges: &[&str]) -> bool {
    privileges.iter().any(|privilege| has_privilege(token, path, privilege))
}

/// The VMID owning a volume, from its name (`vm-<vmid>-...` or
/// `<vmid>/vm-<vmid>-...`), as Proxmox's `parse_volname` derives it.
#[must_use]
pub fn volume_owner(volid: &str) -> Option<u32> {
    let name = volid.split_once(':')?.1;
    let base = name.rsplit('/').next()?;
    let digits = base.strip_prefix("vm-").or_else(|| base.strip_prefix("base-"))?;
    let end = digits.find('-')?;
    digits[..end].parse().ok()
}

/// Whether `token` holds `privilege` on `path`, directly or inherited from a
/// parent path. `*` grants everything, like `root@pam`.
pub fn has_privilege(token: &TokenSpec, path: &str, privilege: &str) -> bool {
    token.acl.iter().any(|(acl_path, privileges)| {
        path_covers(acl_path, path) && privileges.iter().any(|granted| granted == privilege || granted == "*")
    })
}

fn path_covers(acl_path: &str, path: &str) -> bool {
    let acl_path = acl_path.trim_end_matches('/');
    acl_path.is_empty() || path == acl_path || path.strip_prefix(acl_path).is_some_and(|rest| rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(acl: &[(&str, &[&str])]) -> TokenSpec {
        TokenSpec {
            id: "u@pve!t".to_owned(),
            secret: "s".to_owned(),
            acl: acl
                .iter()
                .map(|(path, privileges)| ((*path).to_owned(), privileges.iter().map(|p| (*p).to_owned()).collect()))
                .collect(),
        }
    }

    #[test]
    fn privileges_propagate_to_child_paths_only() {
        let root = token(&[("/", &["Sys.Audit"])]);
        assert!(has_privilege(&root, "/nodes/a", "Sys.Audit"));
        assert!(!has_privilege(&root, "/nodes/a", "VM.Audit"));

        let nodes = token(&[("/nodes/a", &["Sys.Audit"])]);
        assert!(has_privilege(&nodes, "/nodes/a", "Sys.Audit"));
        assert!(!has_privilege(&nodes, "/nodes/ab", "Sys.Audit"));
        assert!(!has_privilege(&nodes, "/nodes", "Sys.Audit"));

        let root = token(&[("/", &["*"])]);
        assert!(has_privilege(&root, "/nodes/a", "Sys.Modify"));
    }
}
