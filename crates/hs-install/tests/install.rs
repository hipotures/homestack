//! The scenarios of `deploy/pve/tests/run.sh` (the shell tests of
//! `homestack-pve.sh`) on the same fake Proxmox, through the Rust installer:
//! single node and cluster, idempotence, adoption of the hand-made state of
//! phases 0-4, uninstall, pools with VMs, snippets in use, an unreachable
//! node, and the checks that stop an install.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;

use common::{Fake, ROLES_TARGET, sorted_lines, standard};
use hs_install::{Command, InstallError, Settings, apply, make_plan, probe};

fn contains(run: &common::Run, text: &str) {
    assert!(run.out.contains(text), "output lacks {text:?}:\n{}", run.out);
}

fn lacks(run: &common::Run, text: &str) {
    assert!(!run.out.contains(text), "output has {text:?}:\n{}", run.out);
}

fn expect_acl(fake: &Fake, expected: &str) {
    let want = sorted_lines(expected);
    let got = fake.homestack_acl();
    assert_eq!(got, want, "ACL differs");
}

fn expect_roles(fake: &Fake, expected: &str) {
    let got: Vec<String> = sorted_lines(
        &fake
            .read("roles.tsv")
            .lines()
            .filter(|line| line.split('\t').nth(2) == Some("0"))
            .map(|line| line.rsplitn(2, '\t').last().unwrap_or_default().to_owned())
            .collect::<Vec<_>>()
            .join("\n"),
    );
    assert_eq!(got, sorted_lines(expected), "roles differ");
}

fn expect_pools(fake: &Fake, expected: &str) {
    let got: Vec<String> = sorted_lines(
        &fake
            .read("pools.tsv")
            .lines()
            .map(|line| line.split('\t').take(2).collect::<Vec<_>>().join("\t"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    assert_eq!(got, sorted_lines(expected), "pools differ");
}

const SINGLE_NODE_ACL: &str = "/nodes\ttoken\thomestack@pve!server\tPVEAuditor
/nodes\ttoken\thomestack@pve!deploy\tPVEAuditor
/nodes\ttoken\thomestack@pve!deploy\tHomeStackDeployNodes
/nodes\tuser\thomestack@pve\tPVEAuditor
/nodes\tuser\thomestack@pve\tHomeStackDeployNodes
/sdn\ttoken\thomestack@pve!server\tPVEAuditor
/sdn\ttoken\thomestack@pve!deploy\tPVEAuditor
/sdn\tuser\thomestack@pve\tPVEAuditor
/storage\ttoken\thomestack@pve!server\tPVEAuditor
/storage\ttoken\thomestack@pve!deploy\tPVEAuditor
/storage\tuser\thomestack@pve\tPVEAuditor
/pool/homestack\ttoken\thomestack@pve!server\tHomeStack
/pool/homestack\tuser\thomestack@pve\tHomeStack
/pool/homestack-gold\ttoken\thomestack@pve!server\tHomeStackGold
/pool/homestack-gold\tuser\thomestack@pve\tHomeStackGold
/pool/homestack-service\ttoken\thomestack@pve!deploy\tHomeStackDeploy
/pool/homestack-service\tuser\thomestack@pve\tHomeStackDeploy
/sdn/zones/localnetwork/vmbr0\ttoken\thomestack@pve!server\tPVESDNUser
/sdn/zones/localnetwork/vmbr0\ttoken\thomestack@pve!deploy\tPVESDNUser
/sdn/zones/localnetwork/vmbr0\tuser\thomestack@pve\tPVESDNUser
/storage/local\ttoken\thomestack@pve!server\tHomeStackStorage
/storage/local\ttoken\thomestack@pve!deploy\tHomeStackDeployTemplates
/storage/local\tuser\thomestack@pve\tHomeStackStorage
/storage/local\tuser\thomestack@pve\tHomeStackDeployTemplates
/storage/local-lvm\ttoken\thomestack@pve!server\tHomeStackStorage
/storage/local-lvm\ttoken\thomestack@pve!deploy\tHomeStackStorage
/storage/local-lvm\tuser\thomestack@pve\tHomeStackStorage";

#[test]
fn install_on_a_single_node() {
    let fake = Fake::new(&["pve1"]);
    let run = fake.install(&standard(false));
    assert!(run.ok, "{}", run.out);
    contains(&run, "single node");
    expect_acl(&fake, SINGLE_NODE_ACL);
    expect_roles(&fake, ROLES_TARGET);
    assert!(
        fake.homestack_acl()
            .iter()
            .all(|line| !line.starts_with("/\t") && !line.starts_with("/vms")),
        "a token or the user can see VMs outside the HomeStack pools"
    );
    expect_pools(&fake, "homestack\t\nhomestack-gold\t101\nhomestack-service\t");
    assert!(
        fake.read("users").lines().any(|line| line == "homestack@pve"),
        "user not created"
    );
    assert!(fake.read("tokens.tsv").contains("homestack@pve\tserver\t1\n"));
    assert!(fake.read("tokens.tsv").contains("homestack@pve\tdeploy\t1\n"));
    // The secrets are on the desktop, nowhere on the node.
    assert_eq!(fake.token_files(), ["pve-token", "pve-token-deploy"]);
    assert_eq!(
        fs::read_to_string(fake.desktop.join("pve-token")).expect("token file"),
        "homestack@pve!server=fake-secret-server\n"
    );
    assert_eq!(
        fs::read_to_string(fake.desktop.join("pve-token-deploy")).expect("token file"),
        "homestack@pve!deploy=fake-secret-deploy\n"
    );
    for name in ["pve-token", "pve-token-deploy"] {
        let mode = fs::metadata(fake.desktop.join(name))
            .expect("token file")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "{name} is not 0600");
    }
    lacks(&run, "fake-secret");
    assert!(
        !fake.dir.join("homestack-pve-token-server").exists(),
        "a secret was left on the node"
    );
    assert!(fake.has_snippets("local"), "snippets missing");
    assert_eq!(
        fs::read_to_string(fake.snippet_dir("local").join("gold-user.yaml")).expect("owner"),
        "owner\n"
    );
    assert!(!fake.dir.join("ssh.log").exists(), "ssh used on a single node");

    fake.clear_calls();
    let again = fake.install(&standard(false));
    assert!(again.ok, "{}", again.out);
    assert_eq!(fake.changes(), 0, "second install changed: {}", fake.read("calls.log"));
    contains(&again, "Nothing to change");
}

#[test]
#[ignore = "slow (owner, 2026-10-15): run with cargo test -- --ignored"]
fn install_in_a_cluster_and_uninstall() {
    let fake = Fake::new(&["pve1", "pve2", "pve3"]);
    let mut settings = standard(false);
    settings.storages = vec!["local-lvm".into(), "worker-roots".into()];
    let run = fake.install(&settings);
    assert!(run.ok, "{}", run.out);
    contains(&run, "cluster of 3 nodes");
    for node in ["local", "pve2", "pve3"] {
        assert!(fake.has_snippets(node), "snippets missing on {node}");
    }
    assert!(
        fake.read("acl.tsv").contains("/storage/worker-roots"),
        "worker-roots has no ACL"
    );

    fake.clear_calls();
    let again = fake.install(&settings);
    assert!(again.ok, "{}", again.out);
    assert_eq!(fake.changes(), 0, "second install changed: {}", fake.read("calls.log"));
    contains(&again, "Nothing to change");

    let mut removal = Settings {
        snippet_storage: Some("local".into()),
        ..Settings::default()
    };
    removal.test = false;
    let gone = fake.uninstall(&removal);
    assert!(gone.ok, "{}", gone.out);
    assert!(fake.homestack_acl().is_empty(), "ACL left: {:?}", fake.homestack_acl());
    expect_roles(&fake, "");
    expect_pools(&fake, "");
    assert!(!fake.read("users").contains("homestack"), "user left");
    assert!(fake.read("tokens.tsv").is_empty(), "tokens left");
    for node in ["local", "pve2", "pve3"] {
        assert!(!fake.has_any_snippet(node), "snippets left on {node}");
    }
    assert_eq!(
        fs::read_to_string(fake.snippet_dir("local").join("gold-user.yaml")).expect("owner"),
        "owner\n"
    );
    assert!(
        fake.read("vms.tsv").lines().any(|line| line.starts_with("101\t")),
        "Gold removed"
    );
    assert!(!fake.read("calls.log").contains("/vms"), "touched a non-HomeStack ACL");
    // The token files on the desktop are not Proxmox's to remove; the plan says so.
    contains(&gone, "pve-token");
    assert_eq!(fake.token_files(), ["pve-token", "pve-token-deploy"]);
}

#[test]
#[ignore = "slow (owner, 2026-10-15): run with cargo test -- --ignored"]
fn adopt_the_hand_made_state_with_the_test_part() {
    let fake = Fake::today();
    let mut settings = standard(true);
    settings.storages = vec!["local-lvm".into(), "worker-roots".into()];
    let run = fake.install(&settings);
    assert!(run.ok, "{}", run.out);
    expect_acl(
        &fake,
        "/\ttoken\thomestack@pve!server\tPVEAuditor
/\tuser\thomestack@pve\tPVEAuditor
/nodes\ttoken\thomestack@pve!deploy\tPVEAuditor
/nodes\ttoken\thomestack@pve!deploy\tHomeStackDeployNodes
/nodes\tuser\thomestack@pve\tHomeStackDeployNodes
/sdn\ttoken\thomestack@pve!deploy\tPVEAuditor
/storage\ttoken\thomestack@pve!deploy\tPVEAuditor
/pool/homestack-service\ttoken\thomestack@pve!deploy\tHomeStackDeploy
/pool/homestack-service\tuser\thomestack@pve\tHomeStackDeploy
/pool/homestack-service\tuser\thomestack@pve\tPVEAuditor
/sdn/zones/localnetwork/vmbr0\ttoken\thomestack@pve!deploy\tPVESDNUser
/storage/local\ttoken\thomestack@pve!deploy\tHomeStackDeployTemplates
/storage/local\tuser\thomestack@pve\tHomeStackDeployTemplates
/storage/local-lvm\ttoken\thomestack@pve!deploy\tHomeStackStorage
/nodes\ttoken\thomestack@pve!test\tPVEAuditor
/nodes\tuser\thomestack@pve\tPVEAuditor
/sdn\ttoken\thomestack@pve!test\tPVEAuditor
/sdn\tuser\thomestack@pve\tPVEAuditor
/storage\ttoken\thomestack@pve!test\tPVEAuditor
/storage\tuser\thomestack@pve\tPVEAuditor
/pool/homestack-gold\ttoken\thomestack@pve!test\tHomeStackGold
/pool/homestack-gold\tuser\thomestack@pve\tHomeStackGold
/pool/homestack-gold\tuser\thomestack@pve\tPVEAuditor
/pool/homestack-test\ttoken\thomestack@pve!test\tHomeStack
/pool/homestack-test\tuser\thomestack@pve\tHomeStack
/pool/homestack-test\tuser\thomestack@pve\tPVEAuditor
/sdn/zones/localnetwork/vmbr0\ttoken\thomestack@pve!test\tPVESDNUser
/sdn/zones/localnetwork/vmbr0\tuser\thomestack@pve\tPVEAuditor
/sdn/zones/localnetwork/vmbr0\tuser\thomestack@pve\tPVESDNUser
/storage/local\ttoken\thomestack@pve!test\tHomeStackStorage
/storage/local\tuser\thomestack@pve\tHomeStackStorage
/storage/local\tuser\thomestack@pve\tPVEAuditor
/storage/local-lvm\ttoken\thomestack@pve!test\tHomeStackStorage
/storage/local-lvm\tuser\thomestack@pve\tHomeStackStorage
/storage/local-lvm\tuser\thomestack@pve\tPVEAuditor
/storage/worker-roots\ttoken\thomestack@pve!test\tHomeStackStorage
/storage/worker-roots\tuser\thomestack@pve\tHomeStackStorage
/storage/worker-roots\tuser\thomestack@pve\tPVEAuditor
/vms\ttoken\thomestack@pve!server\tHomeStackServerDisks
/vms\ttoken\thomestack@pve!server\tPVEAuditor
/vms\tuser\thomestack@pve\tHomeStackServerDisks
/vms\tuser\thomestack@pve\tPVEAuditor
/vms/101\ttoken\thomestack@pve!server\tHomeStackServerDisks
/vms/101\ttoken\thomestack@pve!server\tHomeStackServerGoldGuest
/vms/101\ttoken\thomestack@pve!server\tPVEAuditor
/vms/101\tuser\thomestack@pve\tHomeStackServerDisks
/vms/101\tuser\thomestack@pve\tHomeStackServerGoldGuest
/vms/101\tuser\thomestack@pve\tPVEAuditor",
    );
    expect_roles(
        &fake,
        &format!(
            "{ROLES_TARGET}\nHomeStackServerDisks\tVM.Config.Disk\nHomeStackServerGoldGuest\tVM.GuestAgent.FileRead,VM.GuestAgent.Unrestricted"
        ),
    );
    expect_pools(&fake, "homestack-gold\t101\nhomestack-service\t\nhomestack-test\t");
    let calls = fake.read("calls.log");
    assert!(
        !calls.contains("token add homestack@pve server") && !calls.contains("token add homestack@pve test"),
        "an existing token was recreated"
    );
    assert_eq!(
        fake.token_files(),
        ["pve-token-deploy"],
        "a token file other than the new deploy token's"
    );
    assert!(
        fake.read("acl.tsv").contains("group"),
        "the owner's group ACL was removed"
    );
    for node in ["local", "pve1", "pve3"] {
        assert!(fake.has_snippets(node), "snippets missing on {node}");
    }

    fake.clear_calls();
    let again = fake.install(&settings);
    assert!(again.ok, "{}", again.out);
    assert_eq!(
        fake.changes(),
        0,
        "second install --test changed: {}",
        fake.read("calls.log")
    );

    let server_before: Vec<String> = fake
        .homestack_acl()
        .into_iter()
        .filter(|line| line.contains("homestack@pve!server"))
        .collect();
    let removal = Settings {
        test: true,
        ..Settings::default()
    };
    let gone = fake.uninstall(&removal);
    assert!(gone.ok, "{}", gone.out);
    let server_after: Vec<String> = fake
        .homestack_acl()
        .into_iter()
        .filter(|line| line.contains("homestack@pve!server"))
        .collect();
    assert_eq!(server_after, server_before, "uninstall --test changed the server token");
    let acl = fake.homestack_acl().join("\n");
    assert!(!acl.contains("homestack@pve!test"), "test token entries left");
    assert!(!fake.read("tokens.tsv").contains("\ttest\t"), "test token left");
    assert!(!acl.contains("homestack-test"), "user entries on the test pool left");
    expect_pools(&fake, "homestack-gold\t101\nhomestack-service\t");
    assert!(
        acl.contains("homestack@pve!deploy"),
        "uninstall --test removed the deploy token's entries"
    );
    for node in ["local", "pve1", "pve3"] {
        assert!(
            fake.has_snippets(node),
            "uninstall --test removed the snippets of {node}"
        );
    }
    assert!(
        fake.read("users").lines().any(|l| l == "homestack@pve"),
        "user removed by uninstall --test"
    );
}

fn pool_member(fake: &Fake, pool: &str, vmid: &str) {
    let text = fake.read("pools.tsv");
    let changed: Vec<String> = text
        .lines()
        .map(|line| {
            let mut fields: Vec<&str> = line.split('\t').collect();
            if fields.first() == Some(&pool) {
                fields[1] = vmid;
            }
            fields.join("\t")
        })
        .collect();
    fake.write("pools.tsv", &(changed.join("\n") + "\n"));
}

#[test]
fn a_pool_with_vms_and_snippets_in_use_stay() {
    let fake = Fake::new(&["pve1", "pve2"]);
    assert!(fake.install(&standard(false)).ok);
    fake.append(
        "vms.tsv",
        "240\tpve2\tws\thomestack-ws\tvirtio=BC:24:11:00:00:03,bridge=vmbr0\tvendor=local:snippets/homestack-vendor-keep.yaml\n",
    );
    pool_member(&fake, "homestack", "240");
    let removal = Settings {
        snippet_storage: Some("local".into()),
        ..Settings::default()
    };
    let run = fake.uninstall(&removal);
    assert!(run.ok, "{}", run.out);
    contains(&run, "homestack: 240");
    assert!(
        fake.read("pools.tsv").contains("homestack\t240"),
        "pool with a VM was deleted"
    );
    assert!(!fake.read("pools.tsv").contains("homestack-gold"), "Gold pool left");
    for node in ["local", "pve2"] {
        assert_eq!(
            fs::read_to_string(fake.snippet_dir(node).join("homestack-vendor-keep.yaml")).ok(),
            Some(common::cloud_init("homestack-vendor-keep.yaml")),
            "the snippet in use was removed from {node}"
        );
        assert!(
            !fake.snippet_dir(node).join("homestack-vendor-create.yaml").exists(),
            "the unused snippet was kept on {node}"
        );
    }
    contains(&run, "homestack-vendor-keep.yaml is used by VM 240");
    assert!(fake.read("vms.tsv").contains("240\t"), "VM removed");

    let forced = fake.uninstall(&Settings { force: true, ..removal });
    assert!(forced.ok, "{}", forced.out);
    for node in ["local", "pve2"] {
        assert!(!fake.has_any_snippet(node), "--force left snippets on {node}");
    }
    assert!(
        fake.read("pools.tsv").contains("homestack\t240"),
        "--force deleted a pool with a VM"
    );
}

#[test]
fn the_service_pool_with_containers_stays() {
    let fake = Fake::new(&["pve1", "pve2"]);
    assert!(fake.install(&standard(false)).ok);
    pool_member(&fake, "homestack-service", "998");
    let run = fake.uninstall(&Settings {
        snippet_storage: Some("local".into()),
        ..Settings::default()
    });
    assert!(run.ok, "{}", run.out);
    contains(&run, "homestack-service: 998");
    assert!(
        fake.read("pools.tsv").contains("homestack-service\t998"),
        "pool with a server container was deleted"
    );
    assert!(!fake.read("tokens.tsv").contains("\tdeploy\t"), "deploy token left");
    assert!(fake.homestack_acl().is_empty(), "ACL entries left");
}

#[test]
fn an_unreachable_node_gets_the_rest_done_and_the_run_fails() {
    let fake = Fake::new(&["pve1", "pve2", "pve3"]);
    fake.write("offline-ssh", "pve3\n");
    let run = fake.install(&standard(false));
    assert!(!run.ok, "{}", run.out);
    contains(&run, "pve3");
    assert!(fake.has_snippets("pve2"));
    assert!(fake.read("users").contains("homestack@pve"), "the rest was not done");

    fake.write("offline-ssh", "");
    fake.clear_calls();
    let again = fake.install(&standard(false));
    assert!(again.ok, "{}", again.out);
    assert!(fake.has_snippets("pve3"));
    assert_eq!(fake.changes(), 0, "rerun changed Proxmox objects again");
    contains(&again, "on pve3");
    lacks(&again, "yaml on pve2");
    lacks(&again, "yaml on pve1");
}

#[test]
fn an_offline_node_in_a_cluster_stops_the_run() {
    let fake = Fake::new(&["pve1", "pve2"]);
    fake.replace("nodes.tsv", "pve2\t192.0.2.2\t1\t0", "pve2\t192.0.2.2\t0\t0");
    let run = fake.install(&standard(false));
    assert!(!run.ok, "{}", run.out);
    contains(&run, "pve2 is offline");
}

fn stops(fake: &Fake, settings: &Settings, expect: &[&str]) {
    let run = fake.install(settings);
    assert!(!run.ok, "should have failed:\n{}", run.out);
    for text in expect {
        contains(&run, text);
    }
    assert_eq!(fake.changes(), 0, "changed Proxmox before stopping");
}

#[test]
fn checks_stop_the_install_before_any_change() {
    let fake = Fake::new(&["pve1"]);
    fake.replace("storage.json", "iso,vztmpl,backup,snippets", "iso,vztmpl,backup");
    stops(
        &fake,
        &standard(false),
        &["storage local does not allow content type snippets"],
    );

    let fake = Fake::new(&["pve1"]);
    let mut settings = standard(false);
    settings.storages = vec!["missing".into()];
    stops(&fake, &settings, &["storage missing does not exist"]);

    let fake = Fake::new(&["pve1"]);
    fake.replace("vms.tsv", "homestack-gold", "");
    stops(&fake, &standard(false), &["No VM is tagged homestack-gold"]);

    let fake = Fake::new(&["pve1"]);
    fake.write("pools.tsv", "other\t101\tsomeone else\n");
    stops(&fake, &standard(false), &["Gold 101 is in pool other"]);

    let fake = Fake::new(&["pve1"]);
    fake.write("version-pve1", "{\"version\":\"8.4.1\",\"release\":\"8.4\"}\n");
    stops(&fake, &standard(false), &["pve1 runs PVE 8.4.1"]);

    let fake = Fake::new(&["pve1"]);
    fake.write("options.json", "{\"registered-tags\":\"homestack-ws;other\"}\n");
    stops(&fake, &standard(false), &["tag homestack-ws"]);

    let fake = Fake::new(&["pve1"]);
    let mut settings = standard(false);
    settings.storages.clear();
    stops(&fake, &settings, &["--storages"]);

    // With one possible storage each, they are taken instead of asked.
    let fake = Fake::new(&["pve1"]);
    let mut settings = standard(false);
    settings.service_storages.clear();
    settings.template_storage = None;
    let run = fake.run(Command::Install, &settings, false);
    assert!(run.ok, "{}", run.out);
    let plan = run.plan.expect("a plan");
    assert_eq!(plan.settings.service_storages, ["local-lvm"]);
    assert_eq!(plan.settings.template_storage.as_deref(), Some("local"));

    let fake = Fake::new(&["pve1"]);
    let mut settings = standard(false);
    settings.service_storages = vec!["worker-roots".into()];
    settings.template_storage = Some("local-lvm".into());
    stops(
        &fake,
        &settings,
        &[
            "storage worker-roots does not allow content type rootdir",
            "storage local-lvm does not allow content type vztmpl",
        ],
    );
}

#[test]
fn warnings_do_not_stop_the_install() {
    let fake = Fake::new(&["pve1"]);
    fake.write("trusted", "");
    let run = fake.install(&standard(false));
    assert!(run.ok, "{}", run.out);
    contains(&run, "warning: pve1: the API certificate does not verify");
    contains(&run, "/etc/pve/pve-root-ca.pem");
}

#[test]
fn a_plan_without_confirmation_changes_nothing() {
    let fake = Fake::new(&["pve1", "pve2"]);
    let run = fake.run(Command::Install, &standard(false), false);
    assert!(run.ok, "{}", run.out);
    assert_eq!(fake.changes(), 0, "the plan changed Proxmox");
    contains(&run, "pveum user add homestack@pve");
    assert!(!fake.has_any_snippet("pve2"));
    assert!(fake.token_files().is_empty());
    let plan = run.plan.expect("a plan");
    assert!(!plan.steps.is_empty());
}

#[test]
fn an_outdated_snippet_is_written_again() {
    let fake = Fake::new(&["pve1", "pve2"]);
    assert!(fake.install(&standard(false)).ok);
    fs::write(
        fake.snippet_dir("pve2").join("homestack-vendor-keep.yaml"),
        "old version\n",
    )
    .expect("old snippet");
    let plan = fake.run(Command::Install, &standard(false), false);
    assert!(
        !plan.plan.expect("a plan").steps.is_empty(),
        "the plan missed an outdated snippet"
    );
    assert!(fake.install(&standard(false)).ok);
    assert!(fake.has_snippets("pve2"));
}

// --- the access: root, Proxmox, cluster or not ---------------------------

#[test]
fn the_access_must_be_root() {
    let fake = Fake::new(&["pve1"]);
    fake.write("uid", "1000\n");
    let run = fake.install(&standard(false));
    assert!(!run.ok);
    contains(&run, "not root");
    contains(&run, "uid 1000");
    assert_eq!(fake.changes(), 0);
}

#[test]
fn the_node_must_be_proxmox() {
    let fake = Fake::new(&["pve1"]);
    fs::remove_file(fake.dir.join("bin/pvesh")).expect("remove pvesh");
    let run = fake.install(&standard(false));
    assert!(!run.ok);
    contains(&run, "not a Proxmox VE node");
    contains(&run, "pvesh");
}

#[test]
fn the_probe_tells_a_single_node_from_a_cluster() {
    let single = Fake::new(&["pve1"]);
    let access = probe(&mut single.shell()).expect("probe");
    assert!(!access.topology.cluster);
    assert_eq!(access.topology.describe(), "single node");
    assert_eq!(access.host, "pve1");
    assert_eq!(access.version, "9.2.21");
    assert_eq!(access.topology.quorate, None);

    let cluster = Fake::new(&["pve2", "pve1", "pve3"]);
    let access = probe(&mut cluster.shell()).expect("probe");
    assert!(access.topology.cluster);
    assert_eq!(access.topology.describe(), "cluster of 3 nodes (pve1 pve2 pve3)");
    assert_eq!(access.host, "pve2");
    assert_eq!(access.topology.local_node().map(|n| n.name.as_str()), Some("pve2"));
    assert_eq!(access.topology.quorate, Some(true));
}

#[test]
fn the_nodes_come_in_the_order_of_their_numbers() {
    // GET /cluster/status lists the nodes in any order; the servers apiN
    // follow the nodes, so one order keeps them where they were.
    let cluster = Fake::new(&["pve3", "pve10", "pve1", "pve2"]);
    let access = probe(&mut cluster.shell()).expect("probe");
    assert_eq!(access.topology.describe(), "cluster of 4 nodes (pve1 pve2 pve3 pve10)");
    assert_eq!(access.host, "pve3");
}

#[test]
fn a_cluster_without_quorum_stops_the_run() {
    let fake = Fake::new(&["pve1", "pve2"]);
    fake.write("no-quorum", "");
    let run = fake.install(&standard(false));
    assert!(!run.ok);
    contains(&run, "no quorum");
    assert_eq!(fake.changes(), 0);
}

// --- the token files on the desktop --------------------------------------

#[test]
fn an_existing_token_file_is_kept_aside_never_overwritten() {
    // A file left by an earlier installation whose token is gone: it is
    // renamed, not overwritten, and the new secret takes its name.
    let fake = Fake::new(&["pve1"]);
    fs::write(fake.desktop.join("pve-token"), "keep me\n").expect("existing file");
    let run = fake.install(&standard(false));
    assert!(run.ok, "{}", run.out);
    contains(&run, "pve-token.replaced-");
    assert_eq!(
        fs::read_to_string(fake.desktop.join("pve-token")).expect("file"),
        "homestack@pve!server=fake-secret-server\n"
    );
    let kept: Vec<String> = fake
        .token_files()
        .into_iter()
        .filter(|name| name.starts_with("pve-token.replaced-"))
        .collect();
    assert_eq!(kept.len(), 1, "{:?}", fake.token_files());
    assert_eq!(
        fs::read_to_string(fake.desktop.join(&kept[0])).expect("kept file"),
        "keep me\n"
    );
}

#[test]
fn a_token_whose_file_cannot_be_written_is_deleted_again() {
    let fake = Fake::new(&["pve1"]);
    let settings = Settings {
        token_dir: fake.desktop.clone(),
        ..standard(false)
    };
    let mut root = fake.shell();
    let access = probe(&mut root).expect("probe");
    let plan = make_plan(&mut root, &access, Command::Install, &settings).expect("plan");
    // The directory stops being writable between the plan and the run.
    fs::set_permissions(&fake.desktop, fs::Permissions::from_mode(0o500)).expect("chmod");
    let mut lines = Vec::new();
    let result = apply(&mut root, &plan, &settings, &mut |line| lines.push(line.to_owned()));
    fs::set_permissions(&fake.desktop, fs::Permissions::from_mode(0o700)).expect("chmod back");
    match result {
        Err(InstallError::Failed(message)) => assert!(message.contains("pve-token"), "{message}"),
        other => panic!("expected a failure, got {other:?}"),
    }
    assert!(
        !fake.read("tokens.tsv").contains("\tserver\t"),
        "the token whose secret was lost is still there"
    );
    assert!(!lines.join("\n").contains("fake-secret"));
}

#[test]
fn the_token_directory_must_be_writable_before_anything_changes() {
    let fake = Fake::new(&["pve1"]);
    let settings = Settings {
        token_dir: fake.desktop.join("missing/deeper"),
        ..standard(false)
    };
    let mut root = fake.shell();
    let access = probe(&mut root).expect("probe");
    // A missing directory is made; a file in its place is refused.
    fs::write(fake.desktop.join("missing"), "a file").expect("file");
    let error = make_plan(&mut root, &access, Command::Install, &settings).expect_err("refused");
    assert!(error.to_string().contains("missing"), "{error}");
    assert_eq!(fake.changes(), 0);
}

// --- what the user can choose ---------------------------------------------

fn missing(run: &common::Run) -> Vec<hs_install::Question> {
    match &run.error {
        Some(InstallError::Missing(questions)) => questions.clone(),
        other => panic!("expected missing options, got {other:?}\n{}", run.out),
    }
}

#[test]
fn gold_is_a_question_when_it_is_not_one_tagged_vm() {
    // Several tagged.
    let fake = Fake::new(&["pve1"]);
    fake.append(
        "vms.tsv",
        "111\tpve1\tgold-backup\thomestack-gold\tvirtio=BC:24:11:00:00:09,bridge=vmbr0\t\n",
    );
    let run = fake.install(&standard(false));
    let questions = missing(&run);
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0].option, "--gold");
    let values: Vec<&str> = questions[0].choices.iter().map(|c| c.value.as_str()).collect();
    assert_eq!(values, ["101", "111"]);
    let flat = run.out.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(flat.contains("Several VMs are tagged homestack-gold"), "{}", run.out);
    assert!(
        questions[0].choices[1].detail.contains("gold-backup") && questions[0].choices[1].detail.contains("pve1"),
        "{:?}",
        questions[0].choices[1]
    );
    // None tagged: every QEMU VM.
    let fake = Fake::new(&["pve1"]);
    fake.replace("vms.tsv", "homestack-gold", "");
    let run = fake.install(&standard(false));
    let questions = missing(&run);
    let values: Vec<&str> = questions[0].choices.iter().map(|c| c.value.as_str()).collect();
    assert_eq!(values, ["100", "101"]);
    let flat = run.out.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(flat.contains("No VM is tagged homestack-gold"), "{}", run.out);
}

#[test]
fn a_wrong_choice_is_an_error_that_lists_the_right_ones_briefly() {
    let fake = Fake::new(&["pve1"]);
    let mut settings = standard(false);
    settings.snippet_storage = Some("local-lvm".into());
    settings.storages = vec!["nowhere".into()];
    let run = fake.install(&settings);
    assert!(!run.ok);
    assert!(matches!(run.error, Some(InstallError::Checks { .. })), "{}", run.out);
    assert!(
        run.out
            .contains("storage local-lvm does not allow content type snippets"),
        "{}",
        run.out
    );
    assert!(run.out.contains("storage nowhere does not exist"), "{}", run.out);
    assert!(
        run.out
            .lines()
            .filter(|l| l.starts_with("error:"))
            .all(|l| l.chars().count() <= 140),
        "{}",
        run.out
    );
}

// --- Gold: the VM named hs-gold by default ---------------------------------

fn tags_of(fake: &Fake, vmid: &str) -> String {
    fake.read("vms.tsv")
        .lines()
        .find(|line| line.starts_with(&format!("{vmid}\t")))
        .and_then(|line| line.split('\t').nth(3))
        .unwrap_or_default()
        .to_owned()
}

#[test]
fn gold_defaults_to_the_vm_named_hs_gold_which_is_tagged_and_pooled() {
    let fake = Fake::new(&["pve1"]);
    // The owner's VM 101 is the tagged one; make another, untagged, named hs-gold.
    fake.replace("vms.tsv", "101\tpve1\tgold\thomestack-gold", "101\tpve1\tgold\t");
    fake.append(
        "vms.tsv",
        "150\tpve1\ths-gold\tother\tvirtio=BC:24:11:00:00:07,bridge=vmbr0\t\n",
    );
    let run = fake.install(&standard(false));
    assert!(run.ok, "{}", run.out);
    contains(&run, "tag Gold 150 homestack-gold");
    assert_eq!(tags_of(&fake, "150"), "other;homestack-gold", "existing tags are kept");
    assert!(
        fake.read("pools.tsv").contains("homestack-gold\t150"),
        "not in the Gold pool: {}",
        fake.read("pools.tsv")
    );
    // The owner's other VM is not touched.
    assert_eq!(tags_of(&fake, "101"), "");

    fake.clear_calls();
    let again = fake.install(&standard(false));
    assert!(again.ok, "{}", again.out);
    assert_eq!(fake.changes(), 0, "second install changed: {}", fake.read("calls.log"));
    contains(&again, "Nothing to change");
}

#[test]
fn a_tagged_gold_is_used_before_the_name_and_an_explicit_gold_before_both() {
    // One tagged VM wins over a VM named hs-gold.
    let fake = Fake::new(&["pve1"]);
    fake.append(
        "vms.tsv",
        "150\tpve1\ths-gold\t\tvirtio=BC:24:11:00:00:07,bridge=vmbr0\t\n",
    );
    let run = fake.install(&standard(false));
    assert!(run.ok, "{}", run.out);
    assert!(
        fake.read("pools.tsv").contains("homestack-gold\t101"),
        "{}",
        fake.read("pools.tsv")
    );
    assert_eq!(tags_of(&fake, "150"), "");

    // --gold wins over both, and is tagged.
    let fake = Fake::new(&["pve1"]);
    let mut settings = standard(false);
    settings.gold = Some(100);
    let run = fake.install(&settings);
    assert!(run.ok, "{}", run.out);
    assert_eq!(tags_of(&fake, "100"), "homestack-gold");
    assert!(fake.read("pools.tsv").contains("homestack-gold\t100"));
}

#[test]
fn hs_gold_wins_over_other_tagged_vms_with_a_warning() {
    let fake = Fake::new(&["pve1"]);
    fake.append(
        "vms.tsv",
        "150\tpve1\ths-gold\t\tvirtio=BC:24:11:00:00:07,bridge=vmbr0\t\n",
    );
    fake.append(
        "vms.tsv",
        "111\tpve1\tgold-backup\thomestack-gold\tvirtio=BC:24:11:00:00:09,bridge=vmbr0\t\n",
    );
    let run = fake.install(&standard(false));
    assert!(run.ok, "{}", run.out);
    assert_eq!(tags_of(&fake, "150"), "homestack-gold");
    assert!(
        run.out.contains("warning:") && run.out.contains("101") && run.out.contains("111"),
        "no warning about the other tagged VMs:\n{}",
        run.out
    );
}

#[test]
fn a_gold_with_a_workspace_role_tag_is_refused_before_any_change() {
    let fake = Fake::new(&["pve1"]);
    fake.replace(
        "vms.tsv",
        "101\tpve1\tgold\thomestack-gold",
        "101\tpve1\ths-gold\thomestack-gold;homestack-ws",
    );
    let run = fake.install(&standard(false));
    assert!(!run.ok);
    contains(&run, "homestack-ws");
    contains(&run, "Gold 101");
    assert_eq!(fake.changes(), 0);
}

#[test]
fn uninstall_leaves_the_tag_on_gold() {
    let fake = Fake::new(&["pve1"]);
    fake.append(
        "vms.tsv",
        "150\tpve1\ths-gold\t\tvirtio=BC:24:11:00:00:07,bridge=vmbr0\t\n",
    );
    fake.replace("vms.tsv", "101\tpve1\tgold\thomestack-gold", "101\tpve1\tgold\t");
    assert!(fake.install(&standard(false)).ok);
    let removal = Settings {
        snippet_storage: Some("local".into()),
        ..Settings::default()
    };
    assert!(fake.uninstall(&removal).ok);
    assert_eq!(
        tags_of(&fake, "150"),
        "homestack-gold",
        "Gold is the owner's machine: its tag stays"
    );
    assert!(
        !fake.read("pools.tsv").contains("homestack-gold"),
        "the Gold pool is gone"
    );
}

#[test]
fn only_what_can_be_chosen_is_offered_and_a_single_possibility_is_taken() {
    // Two storages can hold disk images; snippets, container disks and the
    // image each have one possible storage, which is taken without asking.
    let fake = Fake::new(&["pve1", "pve2"]);
    let run = fake.install(&Settings::default());
    let questions = missing(&run);
    let options: Vec<&str> = questions.iter().map(|q| q.option).collect();
    assert_eq!(options, ["--storages"]);
    let values: Vec<&str> = questions[0].choices.iter().map(|c| c.value.as_str()).collect();
    assert_eq!(
        values,
        ["local-lvm", "worker-roots"],
        "only storages that hold disk images"
    );
    assert!(questions[0].multiple);
    assert!(
        questions[0].note.split_whitespace().count() >= 8,
        "{:?}",
        questions[0].note
    );

    // Once the one question is answered, the plan says what was taken.
    let settings = Settings {
        storages: vec!["local-lvm".into()],
        ..Settings::default()
    };
    let run = fake.run(Command::Install, &settings, false);
    let plan = run.plan.expect("a plan");
    assert_eq!(plan.settings.snippet_storage.as_deref(), Some("local"));
    assert_eq!(plan.settings.service_storages, ["local-lvm"]);
    assert_eq!(plan.settings.template_storage.as_deref(), Some("local"));
    assert_eq!(plan.taken.len(), 3, "{:?}", plan.taken);
    assert_eq!(plan.gold.as_deref(), Some("101 gold (pve1)"));
}

#[test]
fn block_storages_are_ticked_at_first_and_local_for_single_files() {
    let fake = Fake::new(&["pve1"]);
    fake.write(
        "storage.json",
        r#"[{"storage":"local","type":"dir","content":"iso,vztmpl,snippets"},
 {"storage":"shared-nfs","type":"nfs","shared":1,"content":"images,rootdir,snippets,vztmpl"},
 {"storage":"backup-a","type":"dir","nodes":"pve1","content":"images,rootdir,vztmpl,snippets"},
 {"storage":"local-lvm","type":"lvmthin","content":"images,rootdir"},
 {"storage":"local-zfs","type":"zfspool","content":"images,rootdir"},
 {"storage":"off","type":"lvmthin","disable":1,"content":"images,rootdir"}]
"#,
    );
    let questions = missing(&fake.install(&Settings::default()));
    let question = |option: &str| questions.iter().find(|q| q.option == option).expect(option).clone();
    let ticked = |option: &str| -> Vec<String> {
        let q = question(option);
        q.defaults.iter().map(|i| q.choices[*i].value.clone()).collect()
    };
    let offered: Vec<String> = question("--storages").choices.iter().map(|c| c.value.clone()).collect();
    assert_eq!(
        offered,
        ["backup-a", "local-lvm", "local-zfs", "shared-nfs"],
        "a disabled storage is not offered"
    );
    assert_eq!(ticked("--storages"), ["local-lvm", "local-zfs"]);
    assert_eq!(ticked("--service-storages"), ["local-lvm", "local-zfs"]);
    assert_eq!(ticked("--snippet-storage"), ["local"]);
    assert_eq!(ticked("--template-storage"), ["local"]);
    // The printed list marks them.
    let text = fake.install(&Settings::default()).out;
    assert!(
        text.lines().any(|l| l.contains("local-lvm") && l.contains("(default)")),
        "{text}"
    );
    assert!(text.lines().all(|l| l.chars().count() <= 100), "{text}");
}
