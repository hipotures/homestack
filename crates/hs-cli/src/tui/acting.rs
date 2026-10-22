//! The keys of the action box and how it follows the work (phase 13b,
//! step 2): which form each action has, what it sends, and how the box
//! moves through plan, run and result.

use crossterm::event::KeyCode;
use hs_model::status::StatusEntry;
use serde_json::{Value, json};

use super::action::{
    Action, Decision, Field, Form, Input, Kind, M_MAX, M_STEP, MIB_PER_GIB, Part, PlanFocus, Progress, Stage, Start,
};
use super::reinstall::Images;
use super::state::{App, Request};
use crate::app::valid_name;

const EXISTING_KEYS: &str = "Apply the keys already configured in the VM";
/// The pools' final address octets; their subnet comes from server configuration.
const WORKSPACE_POOLS: [(&str, (u32, u32)); 2] = [("homestack", (10, 199)), ("homestack-test", (200, 254))];

impl App {
    /// The desktop's public keys, offered by add key.
    pub fn set_keys(&mut self, keys: Vec<super::action::KeyFile>) {
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

    /// The pools a new workspace can go to, as the status lists them: their
    /// IDs and names (comments in lower case); the server's pool alone when
    /// it lists neither.
    fn pools(&self) -> Vec<(String, String)> {
        let groups = self.groups();
        let listed: Vec<(String, String)> = WORKSPACE_POOLS
            .iter()
            .filter_map(|(id, _)| groups.iter().find(|(pool, _)| pool == id).cloned())
            .collect();
        if listed.is_empty() {
            let pool = self.server_pool().to_owned();
            return vec![(pool.clone(), pool)];
        }
        listed
    }

    /// The pool the server makes workspaces in when it is not told one.
    fn server_pool(&self) -> &str {
        self.pool.as_deref().unwrap_or(WORKSPACE_POOLS[0].0)
    }

    /// The pool the form chooses first: the group shown when it is one of
    /// `pools`, else the workspaces.
    fn first_pool(&self, pools: &[(String, String)]) -> usize {
        let at = |id: &str| pools.iter().position(|(pool, _)| pool == id);
        self.group
            .as_deref()
            .and_then(at)
            .or_else(|| at(WORKSPACE_POOLS[0].0))
            .unwrap_or(0)
    }

    /// The ID of the pool the new form has chosen.
    pub(super) fn chosen_pool(&self, form: &Form) -> String {
        let name = form
            .fields
            .iter()
            .find(|field| field.label == "Pool")
            .map(super::action::Field::value)
            .unwrap_or_default();
        self.pools()
            .into_iter()
            .find(|(_, pool_name)| *pool_name == name)
            .map_or_else(|| self.server_pool().to_owned(), |(id, _)| id)
    }

    /// Use the first workload /24 after the network's first /24 when there
    /// is room; a /24 or smaller uses its own third octet.
    fn pool_vmids(&self, pool: &str) -> Option<(u32, u32)> {
        let network = self.network.as_ref()?;
        let gateway: std::net::Ipv4Addr = network.gateway.parse().ok()?;
        let mask = u32::MAX.checked_shl(32 - u32::from(network.cidr)).unwrap_or(0);
        let start = std::net::Ipv4Addr::from(u32::from(gateway) & mask);
        let third = u32::from(start.octets()[2]) + u32::from(network.cidr < 24);
        let (first, last) = WORKSPACE_POOLS
            .iter()
            .find(|(id, _)| *id == pool)
            .map_or(WORKSPACE_POOLS[0].1, |(_, octets)| *octets);
        Some(((third * 1000 + first).max(100), third * 1000 + last))
    }

    /// The VMID a new workspace in `pool` gets: the first of the pool's
    /// range that no VM or container of the HomeStack pools has.
    pub(super) fn free_vmid(&self, pool: &str) -> Option<u32> {
        let used: Vec<u32> = self
            .status
            .iter()
            .flat_map(|status| {
                status
                    .workspaces
                    .iter()
                    .chain(&status.others)
                    .map(|entry| entry.vmid)
                    .chain(status.gold.as_ref().map(|gold| gold.vmid))
            })
            .collect();
        let (first, last) = self.pool_vmids(pool)?;
        let occupied: Vec<_> = used.iter().filter_map(|vmid| self.address_of(*vmid)).collect();
        (first..=last).find(|vmid| {
            !used.contains(vmid)
                && self.address_of(*vmid).is_some_and(|address| {
                    !occupied.contains(&address)
                        && self
                            .network
                            .as_ref()
                            .is_some_and(|network| address.to_string() != network.gateway)
                })
        })
    }

    /// Use the same configured address mapping as server-side validation.
    pub(super) fn address_of(&self, vmid: u32) -> Option<std::net::Ipv4Addr> {
        hs_model::address::address_of(vmid, self.network.as_ref()?)
    }

    /// The MiB free on the storage the form makes the home on (without a
    /// node or storage in the form, the server's: Gold's node and its first).
    pub(super) fn home_room(&self, form: &Form) -> Option<u64> {
        let chosen = |label: &str| {
            form.fields
                .iter()
                .find(|field| field.label == label)
                .map(super::action::Field::value)
                .filter(|value| !value.is_empty())
        };
        let node = chosen("Node").or_else(|| self.gold_node())?;
        let storage = chosen("Storage").or_else(|| self.storages_on(&node).into_iter().next())?;
        self.status
            .as_ref()?
            .summary
            .storage_layouts
            .iter()
            .find(|layout| layout.node == node && layout.storage == storage)?
            .available_bytes
            .map(|bytes| bytes / (1024 * 1024))
    }

    /// How big a home of `mib` is: of the storage's free space above 10 %
    /// a warning, above 50 % a danger; without it above 100G and 500G.
    pub(super) fn home_level(&self, form: &Form, mib: u64) -> Level {
        let (warning, danger) = match self.home_room(form) {
            Some(room) => (room / 10, room / 2),
            None => (100 * MIB_PER_GIB, 500 * MIB_PER_GIB),
        };
        if mib > danger {
            Level::Danger
        } else if mib > warning {
            Level::Warning
        } else {
            Level::Fine
        }
    }

    /// The size in GiB of the selected workspace's root or home disk, from
    /// its details (or, for the home, its row).
    fn disk_gib(&self, root: bool) -> Option<u64> {
        const GIB: u64 = 1024 * 1024 * 1024;
        let role = if root { "root" } else { "home" };
        let from_details = self.selected_details().and_then(|details| {
            details
                .volumes
                .iter()
                .find(|volume| volume.role == role)
                .and_then(|volume| volume.size.as_deref())
                .and_then(|size| {
                    let size = size.trim();
                    size.strip_suffix('T')
                        .and_then(|t| t.parse::<u64>().ok())
                        .map(|t| t * 1024)
                        .or_else(|| size.strip_suffix('G').and_then(|g| g.parse::<u64>().ok()))
                })
        });
        from_details.or_else(|| {
            (!root)
                .then(|| self.selected_entry()?.home.as_ref()?.size_bytes)
                .flatten()
                .map(|bytes| bytes / GIB)
        })
    }

    /// The configured Gold first, then the other Gold VMs listed in status.
    fn golds(&self) -> Vec<&StatusEntry> {
        self.status
            .iter()
            .flat_map(|status| status.gold.iter().chain(&status.others))
            .filter(|entry| entry.role == "GOLD")
            .collect()
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
            // t: stop a running workspace, start a stopped one.
            't' if self.selected_entry().is_some_and(|entry| entry.status == "running") => Kind::Stop,
            't' => Kind::Start,
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
        // New is about no workspace, whatever is selected.
        let entry = self.selected_entry().cloned().filter(|_| kind != Kind::New);
        if kind != Kind::New && entry.is_none() {
            return None;
        }
        if kind == Kind::Addkey
            && entry
                .as_ref()
                .is_some_and(|entry| entry.role == "ALIEN" && entry.status != "running")
        {
            self.log("", "Add key over SSH requires the alien VM to be running.");
            return None;
        }
        let vmid = entry.as_ref().map(|entry| entry.vmid);
        let name = entry.as_ref().and_then(|entry| entry.name.clone()).unwrap_or_default();
        if kind == Kind::Resize && self.wait_for_details(super::state::DetailsAction::Resize) {
            return None;
        }
        let fields = match kind {
            Kind::New => {
                let nodes = self.nodes();
                let first_node = self
                    .gold_node()
                    .and_then(|gold_node| nodes.iter().position(|node| *node == gold_node))
                    .unwrap_or(0);
                let storages = nodes
                    .get(first_node)
                    .map(|node| self.storages_on(node))
                    .unwrap_or_default();
                let mut node = Field::choice("Node", nodes, "Where the VM is made; ← → or a click on ‹ › to choose.");
                if let Input::Choice { index, .. } = &mut node.input {
                    *index = first_node;
                }
                let pools = self.pools();
                let first = self.first_pool(&pools);
                let mut pool = Field::choice(
                    "Pool",
                    pools.into_iter().map(|(_, name)| name).collect(),
                    "The workspace's VMID and address follow from its pool; ← → to choose.",
                );
                if let Input::Choice { index, .. } = &mut pool.input {
                    *index = first;
                }
                vec![
                    pool,
                    Field::text(
                        "Name",
                        "",
                        "A letter first, then letters, digits, '.' or '-'; it is also the SSH alias.",
                    ),
                    node,
                    Field::choice(
                        "Storage",
                        storages,
                        "Where its disks are made; ← → or a click on ‹ › to choose.",
                    ),
                    Field::amount(
                        "Home size",
                        self.home_default,
                        "← → or the wheel change it; ⇥ or ↑ ↓: G or M.",
                    ),
                    Field::choice(
                        "Gold",
                        self.golds()
                            .iter()
                            .map(|entry| format!("{} ({})", entry.vmid, entry.name.as_deref().unwrap_or("unnamed")))
                            .collect(),
                        "The source VM to clone; ← → or a click on ‹ › to choose.",
                    ),
                ]
            }
            Kind::Resize => vec![
                Field::choice(
                    "Disk",
                    vec!["home".to_owned(), "root".to_owned()],
                    "Disks only grow; ← → to choose.",
                ),
                Field::size(
                    "New size",
                    self.disk_gib(false).unwrap_or(1),
                    "← → 1 GiB, PgUp PgDn 10 GiB; never below the size now. The filesystem grows online.",
                ),
            ],
            Kind::Addkey => {
                let mut fields = vec![Field {
                    label: "",
                    input: Input::Check {
                        checked: true,
                        value: String::new(),
                        caption: EXISTING_KEYS.to_owned(),
                    },
                    note: "↑ ↓ move · Space or click selects · Enter reviews all selected keys.".to_owned(),
                }];
                if entry.as_ref().is_some_and(|entry| entry.role == "ALIEN") {
                    fields.clear();
                }
                fields.extend(self.keys.iter().map(|key| Field {
                    label: "",
                    input: Input::Check {
                        checked: false,
                        value: key.path.clone(),
                        caption: format!("{}  ({})", key.name, key.path),
                    },
                    note: "↑ ↓ move · Space or click selects · Enter reviews all selected keys.".to_owned(),
                }));
                if fields.is_empty() {
                    self.log(
                        "",
                        "No public key files found in ~/.ssh; add a public key file before copying keys.",
                    );
                    return None;
                }
                fields
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
            Kind::Refresh | Kind::Destroy | Kind::Start | Kind::Stop | Kind::Reinstall => Vec::new(),
        };
        let mut action = Action {
            kind,
            vmid,
            name,
            stage: Stage::Asking,
            hidden: false,
            targets: Vec::new(),
        };
        if fields.is_empty() {
            let start = Start {
                kind,
                vmid,
                parameters: json!({"vmid": vmid}),
                key_files: Vec::new(),
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
        let mut key_files = Vec::new();
        let parameters: Value = match action.kind {
            Kind::New => {
                let pool = self.chosen_pool(form);
                let Some(id) = self.free_vmid(&pool) else {
                    return Err(match self.pool_vmids(&pool) {
                        Some((first, last)) => {
                            format!("No free VMID has a usable address in the pool's range ({first}-{last}).")
                        }
                        None => {
                            "The server network is not available yet; wait for its configuration to load.".to_owned()
                        }
                    });
                };
                let name = value("Name");
                if !valid_name(&name) {
                    return Err(format!(
                        "Invalid name {name:?}: the first character is a letter, then letters, digits, '.' or '-'."
                    ));
                }
                let gold_vmid = value("Gold")
                    .split_whitespace()
                    .next()
                    .and_then(|vmid| vmid.parse::<u32>().ok())
                    .ok_or("No Gold VM is available; add a Gold VM to the Gold pool.")?;
                let mut parameters = json!({"vmid": id, "name": name, "gold_vmid": gold_vmid});
                if pool != self.server_pool() {
                    parameters["pool"] = json!(pool);
                }
                for (label, key) in [("Node", "node"), ("Storage", "storage")] {
                    let chosen = value(label);
                    if !chosen.is_empty() {
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
                let grown = form.fields.iter().find_map(|field| match field.input {
                    Input::Size { gib, min } => Some((gib, min)),
                    _ => None,
                });
                if let Some((gib, min)) = grown
                    && gib <= min
                {
                    return Err(format!("Choose a size above the {min}G it has now (→ or PgUp)."));
                }
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
                let selected: Vec<_> = form
                    .fields
                    .iter()
                    .filter(|field| matches!(field.input, Input::Check { checked: true, .. }))
                    .collect();
                if selected.is_empty() {
                    return Err("Select at least one key, or apply the VM's configured keys.".to_owned());
                }
                key_files = selected
                    .iter()
                    .map(|field| field.value())
                    .filter(|path| !path.is_empty())
                    .collect();
                if self.selected_entry().is_some_and(|entry| entry.role == "ALIEN") {
                    let entry = self.selected_entry().ok_or("No alien VM selected")?;
                    if entry.status != "running" {
                        return Err("Add key over SSH requires the alien VM to be running.".to_owned());
                    }
                    let alias = entry
                        .name
                        .as_deref()
                        .filter(|name| valid_name(name))
                        .ok_or("The alien VM has no valid SSH alias")?;
                    json!({"vmid":vmid,"alien_ssh":alias})
                } else {
                    json!({"vmid": vmid, "restart": false})
                }
            }
            Kind::Migrate => {
                let node = value("Node");
                let storage = value("Storage");
                if node.is_empty() || storage.is_empty() {
                    return Err("There is no other node with a storage for workspace disks.".to_owned());
                }
                json!({"vmid": vmid, "target_node": node, "target_storage": storage})
            }
            Kind::Refresh | Kind::Destroy | Kind::Start | Kind::Stop => json!({"vmid": vmid}),
            Kind::Reinstall => self.reinstall_parameters(form)?,
        };
        Ok(Start {
            kind: action.kind,
            vmid,
            parameters,
            key_files,
        })
    }

    /// Enter on a server container: the reinstall box for it, or for every
    /// marked one when it is marked; the images are read first.
    pub(super) fn open_reinstall(&mut self) -> Option<Request> {
        let entry = self
            .selected_entry()
            .filter(|entry| self.image_container(entry))?
            .clone();
        if let Some(action) = &self.action {
            let line = format!("one action at a time: {} is still open", action.title());
            self.log("", &line);
            return None;
        }
        let servers: Vec<(u32, String)> = if self.marked.contains(&entry.vmid) {
            self.status
                .iter()
                .flat_map(|status| status.gold.iter().chain(&status.workspaces).chain(&status.others))
                .filter(|other| {
                    self.marked.contains(&other.vmid)
                        && self.image_container(other)
                        && is_site(other) == is_site(&entry)
                        && self.common_service(other) == self.common_service(&entry)
                })
                .map(|other| (other.vmid, other.name.clone().unwrap_or_default()))
                .collect()
        } else {
            vec![(entry.vmid, entry.name.clone().unwrap_or_default())]
        };
        self.open_image_update(servers)
    }

    /// The existing image selector shared by API, Site and registered services.
    pub fn open_image_update(&mut self, mut servers: Vec<(u32, String)>) -> Option<Request> {
        if self.action.is_some() || self.setup.is_some() {
            return None;
        }
        self.overlay = None;
        servers.sort_unstable();
        servers.dedup();
        let targets: Vec<u32> = servers.iter().map(|(vmid, _)| *vmid).collect();
        let names: Vec<String> = servers.into_iter().map(|(_, name)| name).collect();
        self.images = None;
        self.action = Some(Action {
            kind: Kind::Reinstall,
            vmid: None,
            name: names.join(", "),
            stage: Stage::Form(Form {
                fields: vec![Field::choice(
                    "Image",
                    Vec::new(),
                    "← → choose the image; ↵ shows what is done.",
                )],
                focus: 0,
                error: None,
            }),
            hidden: false,
            targets: targets.clone(),
        });
        Some(Request::Images(targets))
    }

    /// The images for the reinstall box of `targets`: its choice, newest
    /// first.
    pub fn set_images(&mut self, targets: &[u32], images: Result<Images, String>) {
        let Some(action) = self
            .action
            .as_mut()
            .filter(|action| action.kind == Kind::Reinstall && action.targets == targets)
        else {
            return;
        };
        if let (Stage::Form(form), Ok(images)) = (&mut action.stage, &images)
            && let Some(Field {
                input: Input::Choice { options, index },
                note,
                ..
            }) = form.fields.first_mut()
        {
            if images.resume.is_some() {
                *note = "↵ reviews the interrupted update; confirmation resumes it.".to_owned();
            }
            *options = images
                .published
                .iter()
                .map(|image| format!("{}  {}", image.tag(), image.made))
                .collect();
            *index = 0;
        }
        self.images = Some(images);
    }

    /// What reinstall sends: the image, the servers and the plan's lines.
    fn reinstall_parameters(&self, form: &Form) -> Result<Value, String> {
        let images = match &self.images {
            None => return Err("The images are still being read.".to_owned()),
            Some(Err(error)) => return Err(error.clone()),
            Some(Ok(images)) => images,
        };
        let index = form.fields.first().map_or(0, Field::index);
        let image = images
            .published
            .get(index)
            .ok_or_else(|| "The registry lists no image.".to_owned())?;
        if let Some(identity) = &images.common {
            let ids: Vec<u32> = images.servers.iter().map(|s| s.vmid).collect();
            return Ok(json!({"image":image.image,"instances":ids,"service":identity}));
        }
        let mut plan = vec![
            format!(
                "Update {} containers from {}",
                if images.sites.is_empty() {
                    "API"
                } else {
                    "Pangolin Site"
                },
                image.tag()
            ),
            format!("made {}", image.made),
            String::new(),
        ];
        let width = images
            .servers
            .iter()
            .map(|server| server.name.chars().count())
            .max()
            .unwrap_or(0);
        for server in &images.servers {
            let now = server
                .image
                .as_ref()
                .map_or_else(|| "not made by homestack-deploy".to_owned(), |now| now.tag().to_owned());
            plan.push(format!(
                "{:<width$}  CT {}  {now} → {}",
                server.name,
                server.vmid,
                image.tag()
            ));
        }
        plan.push(String::new());
        if !images.sites.is_empty() {
            plan.push("Site identities and credentials are retained; the network and MAC are preserved.".to_owned());
            plan.push("Root SSH access to a Proxmox node restores credentials over standard input.".to_owned());
            plan.push("Each connector is briefly offline while its container is recreated.".to_owned());
            return Ok(json!({"image": image.image, "servers": images.sites, "plan": plan, "sites": true}));
        }
        let names: Vec<&str> = images.servers.iter().map(|server| server.name.as_str()).collect();
        Ok(json!({"image": image.image, "servers": names, "plan": plan}))
    }

    /// Space on a server container: mark it, or unmark it.
    pub(super) fn toggle_mark(&mut self) {
        if let Some(vmid) = self
            .selected_entry()
            .filter(|entry| self.image_container(entry))
            .map(|entry| entry.vmid)
            && !self.marked.remove(&vmid)
        {
            self.marked.insert(vmid);
        }
    }

    /// A choice moved: the storages follow the node.
    fn follow_choice(&self, kind: Kind, form: &mut Form) {
        if kind == Kind::Resize {
            // The size starts again at the chosen disk's.
            let root = form
                .fields
                .iter()
                .find(|field| field.label == "Disk")
                .is_some_and(|field| field.value() == "root");
            let now = self.disk_gib(root).unwrap_or(1);
            if let Some(field) = form.fields.iter_mut().find(|field| field.label == "New size") {
                field.input = Input::Size { gib: now, min: now };
            }
            return;
        }
        let node = form
            .fields
            .iter()
            .find(|field| field.label == "Node")
            .map(super::action::Field::value)
            .unwrap_or_default();
        let node = if node.is_empty() {
            self.gold_node().unwrap_or_default()
        } else {
            node
        };
        let storages = self.storages_on(&node);
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
        let part = amount_part(form);
        match code {
            KeyCode::Char(' ') if action.kind == Kind::Addkey => {
                let existing_option = form
                    .fields
                    .first()
                    .is_some_and(|field| matches!(&field.input, Input::Check { value, .. } if value.is_empty()));
                if let Some(Field {
                    input: Input::Check { checked, .. },
                    ..
                }) = form.fields.get_mut(form.focus)
                {
                    *checked = !*checked;
                    if *checked {
                        for (index, field) in form.fields.iter_mut().enumerate() {
                            if existing_option
                                && index != form.focus
                                && (index == 0 || form.focus == 0)
                                && let Input::Check { checked, .. } = &mut field.input
                            {
                                *checked = false;
                            }
                        }
                    }
                    form.error = None;
                }
            }
            // An amount's G and M are places of their own for Tab and
            // BackTab; ↑ ↓ switch between them round.
            KeyCode::Tab if part == Some(Part::G) => set_part(form, Part::M),
            KeyCode::BackTab if part == Some(Part::M) => set_part(form, Part::G),
            KeyCode::Up | KeyCode::Down if part.is_some() => {
                set_part(form, if part == Some(Part::G) { Part::M } else { Part::G });
            }
            KeyCode::Tab | KeyCode::Down => {
                form.focus = (form.focus + 1) % count;
                set_part(form, Part::G);
            }
            KeyCode::BackTab | KeyCode::Up => {
                form.focus = (form.focus + count - 1) % count;
                set_part(form, if code == KeyCode::BackTab { Part::M } else { Part::G });
            }
            KeyCode::Left | KeyCode::Right | KeyCode::PageUp | KeyCode::PageDown
                if matches!(
                    form.fields.get(form.focus),
                    Some(Field {
                        input: Input::Size { .. },
                        ..
                    })
                ) =>
            {
                if let Some(Field {
                    input: Input::Size { gib, min },
                    ..
                }) = form.fields.get_mut(form.focus)
                {
                    let step = if matches!(code, KeyCode::PageUp | KeyCode::PageDown) {
                        10
                    } else {
                        1
                    };
                    *gib = if matches!(code, KeyCode::Right | KeyCode::PageUp) {
                        gib.saturating_add(step)
                    } else {
                        gib.saturating_sub(step).max(*min)
                    };
                }
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char('-' | '+' | ' ')
                if matches!(
                    form.fields.get(form.focus),
                    Some(Field {
                        input: Input::Amount { .. },
                        ..
                    })
                ) =>
            {
                let room = self.home_room(form);
                if let Some(field) = form.fields.get_mut(form.focus) {
                    match code {
                        KeyCode::Char(' ') => {
                            if let Input::Amount { part, .. } = &mut field.input {
                                *part = if *part == Part::G { Part::M } else { Part::G };
                            }
                        }
                        _ => step(
                            &mut field.input,
                            matches!(code, KeyCode::Right | KeyCode::Char('+')),
                            room,
                        ),
                    }
                }
            }
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
                // Enter applies in every plan (owner, 2026-10-15), destroy too;
                // only a plan that cannot be applied starts on cancel.
                } else if blocked.is_some() {
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
                Stage::Done { ok, lines, scroll: 0 }
            }
        };
    }
}

/// How big a new home is for the storage it goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Fine,
    Warning,
    Danger,
}

/// One step of an amount's chosen part, up or down: the M part carries
/// into the G part. Never to nothing, never above `room` MiB.
fn step(input: &mut Input, up: bool, room: Option<u64>) {
    let Input::Amount { g, m, part, set } = input else {
        return;
    };
    let (mut new_g, mut new_m) = (*g, *m);
    match (*part, up) {
        (Part::G, true) => new_g += 1,
        (Part::G, false) if new_g > 0 => new_g -= 1,
        (Part::M, true) if new_m < M_MAX => new_m += M_STEP,
        (Part::M, true) => (new_g, new_m) = (new_g + 1, 0),
        (Part::M, false) if new_m > 0 => new_m -= M_STEP,
        (Part::M, false) if new_g > 0 => (new_g, new_m) = (new_g - 1, M_MAX),
        _ => return,
    }
    let mib = new_g * MIB_PER_GIB + new_m;
    if mib == 0 || room.is_some_and(|room| mib > room) {
        return;
    }
    (*g, *m, *set) = (new_g, new_m, true);
}

/// The chosen part of the focused field when it is an amount.
fn amount_part(form: &Form) -> Option<Part> {
    match form.fields.get(form.focus)?.input {
        Input::Amount { part, .. } => Some(part),
        _ => None,
    }
}

/// Choose `chosen` of the focused field when it is an amount.
fn set_part(form: &mut Form, chosen: Part) {
    if let Some(Field {
        input: Input::Amount { part, .. },
        ..
    }) = form.fields.get_mut(form.focus)
    {
        *part = chosen;
    }
}

/// A HomeStack API server: a container tagged as one whose CTID
/// `deploy.toml` saves as an instance (`api`). The servers' pool alone
/// proves nothing: other services and the Site connectors share it.
#[must_use]
pub fn is_server(entry: &StatusEntry, api: &std::collections::BTreeSet<u32>) -> bool {
    entry.role == "CT"
        && api.contains(&entry.vmid)
        && entry.tags.iter().any(|tag| tag == hs_deploy::node::SERVER_TAG)
        && !entry.tags.iter().any(|tag| tag == super::pangolin_sites::TAG)
}

/// A managed Pangolin Site connector.
#[must_use]
pub fn is_site(entry: &StatusEntry) -> bool {
    entry.role == "CT" && entry.tags.iter().any(|tag| tag == super::pangolin_sites::TAG)
}

impl App {
    /// Containers with a managed image replacement workflow.
    #[must_use]
    pub fn image_container(&self, entry: &StatusEntry) -> bool {
        self.common_service(entry).is_some() || is_server(entry, &self.api_instances) || is_site(entry)
    }

    pub fn common_service(&self, entry: &StatusEntry) -> Option<(String, String)> {
        if entry.role != "CT" {
            return None;
        }
        let (record, instance) = self.services.as_ref().ok()?.instance(entry.vmid)?;
        if instance.node != entry.node
            || !matches!(
                instance.stage,
                crate::services::registry::Stage::Ready | crate::services::registry::Stage::Created
            )
        {
            return None;
        }
        Some((record.repository.clone(), record.service.clone()))
    }
}
