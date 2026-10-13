//! The guest inspection of `resize` against a scripted guest: the Python
//! tests `test_resize_inspection.py` and `test_resize_layers.py`, which
//! patch `subprocess.check_output`, `open`, `os.path.realpath`,
//! `os.listdir`, `os.stat` and `shutil.which` around the script.

use std::collections::HashMap;

use hs_core::CoreError;
use hs_core::guest::FileError;
use hs_core::resize_guest::{CryptAuth, FileKind, GuestLayout, GuestSystem, InspectError, Role, inspect};
use serde_json::{Value, json};

type Run = Box<dyn Fn(&[&str]) -> Result<String, String> + Send>;

/// The patched guest: every tool exists, `os.listdir` is empty and
/// `os.stat` finds a regular file.
struct FakeGuest {
    run: Run,
    files: HashMap<String, String>,
    aliases: HashMap<String, String>,
}

impl FakeGuest {
    fn new(run: Run) -> Self {
        Self {
            run,
            files: HashMap::new(),
            aliases: HashMap::new(),
        }
    }
}

impl GuestSystem for FakeGuest {
    async fn has_tool(&mut self, _name: &str) -> Result<bool, CoreError> {
        Ok(true)
    }

    async fn run(&mut self, argv: &[&str]) -> Result<Result<String, String>, CoreError> {
        Ok((self.run)(argv))
    }

    async fn read(&mut self, path: &str) -> Result<Result<String, FileError>, CoreError> {
        Ok(self.files.get(path).cloned().ok_or(FileError::Missing))
    }

    async fn realpaths(&mut self, paths: &[String]) -> Result<Vec<String>, CoreError> {
        Ok(paths
            .iter()
            .map(|path| self.aliases.get(path).cloned().unwrap_or_else(|| path.clone()))
            .collect())
    }

    async fn list_dir(&mut self, _path: &str) -> Result<Vec<String>, CoreError> {
        Ok(Vec::new())
    }

    async fn file_kind(&mut self, _path: &str) -> Result<FileKind, CoreError> {
        Ok(FileKind::Regular)
    }
}

fn row(name: &str, kind: &str, parent: Option<&str>, number: &str, serial: Option<&str>, size: u64) -> Value {
    json!({"name": name, "type": kind, "pkname": parent, "serial": serial, "size": size, "maj:min": number})
}

const GIB: u64 = 1024 * 1024 * 1024;
const LABEL: &str = "HS_HOME_207";

async fn run_inspect(guest: &mut FakeGuest, mount: &str, role: Role) -> Result<GuestLayout, String> {
    match inspect(guest, mount, role, LABEL).await {
        Ok(layout) => Ok(layout),
        Err(InspectError::Guest(message)) => Err(message),
        Err(InspectError::Core(error)) => panic!("unexpected guest failure: {error}"),
    }
}

// --- test_resize_inspection.py ----------------------------------------------

#[derive(Default)]
struct Simple {
    home: bool,
    filesystem: Option<&'static str>,
    root_type: Option<&'static str>,
    source: Option<&'static str>,
    maj_min: Option<&'static str>,
    partition: bool,
}

/// `GuestInspectionTests.inspect`
async fn inspect_simple(options: Simple) -> Result<GuestLayout, String> {
    let mut rows = vec![
        row(
            "/dev/sda",
            options.root_type.unwrap_or("disk"),
            None,
            "8:0",
            None,
            16 * GIB,
        ),
        row("/dev/sdb", "disk", None, "8:16", Some(LABEL), 500 * GIB),
        row("/dev/zram0", "disk", None, "252:0", None, GIB),
    ];
    if options.partition {
        rows.insert(1, row("/dev/sda1", "part", Some("/dev/sda"), "8:1", None, 16 * GIB));
    }
    let mount = if options.home { "/home/user" } else { "/" };
    let findmnt = json!({"filesystems": [{
        "source": options.source.unwrap_or(if options.home { "/dev/sdb" } else { "/dev/sda" }),
        "fstype": options.filesystem.unwrap_or("ext4"),
        "target": mount,
        "options": "rw,relatime",
        "maj:min": options.maj_min.unwrap_or(if options.home { "8:16" } else { "8:0" }),
    }]})
    .to_string();
    let lsblk = json!({ "blockdevices": rows }).to_string();
    let mut guest = FakeGuest::new(Box::new(move |argv: &[&str]| match argv[0] {
        "findmnt" => Ok(findmnt.clone()),
        "lsblk" => Ok(lsblk.clone()),
        "blkid" => Ok(LABEL.to_owned()),
        other => panic!("unexpected command {other}: {argv:?}"),
    }));
    if options.partition {
        guest
            .files
            .insert("/sys/class/block/sda1/partition".to_owned(), "1".to_owned());
        guest
            .files
            .insert("/sys/class/block/sda1/start".to_owned(), "2048".to_owned());
    }
    let role = if options.home { Role::Home } else { Role::Root };
    run_inspect(&mut guest, mount, role).await
}

#[tokio::test]
async fn root_and_home_are_identified_with_zram_present() -> Result<(), String> {
    assert_eq!(inspect_simple(Simple::default()).await?.device, "/dev/sda");
    let home = inspect_simple(Simple {
        home: true,
        ..Simple::default()
    })
    .await?;
    assert_eq!(home.device, "/dev/sdb");
    Ok(())
}

#[tokio::test]
async fn root_source_alias_maps_to_disk_by_major_minor() -> Result<(), String> {
    let layout = inspect_simple(Simple {
        source: Some("/dev/root"),
        ..Simple::default()
    })
    .await?;
    assert_eq!(layout.device, "/dev/sda");
    Ok(())
}

#[tokio::test]
async fn root_partition_source_alias_maps_by_major_minor() -> Result<(), String> {
    let layout = inspect_simple(Simple {
        source: Some("/dev/root"),
        maj_min: Some("8:1"),
        partition: true,
        ..Simple::default()
    })
    .await?;
    assert_eq!(layout.device, "/dev/sda1");
    assert_eq!(layout.parent, "/dev/sda");
    assert_eq!(layout.partition, Some(1));
    Ok(())
}

#[tokio::test]
async fn unknown_major_minor_is_rejected() {
    let error = inspect_simple(Simple {
        maj_min: Some("8:99"),
        ..Simple::default()
    })
    .await
    .err()
    .unwrap_or_default();
    assert!(error.contains("Cannot identify mounted block device"), "{error}");
}

#[tokio::test]
async fn unsupported_filesystem_and_unresolved_lvm_are_rejected() {
    let error = inspect_simple(Simple {
        filesystem: Some("xfs"),
        ..Simple::default()
    })
    .await
    .err()
    .unwrap_or_default();
    assert!(error.contains("ext4"), "{error}");
    let error = inspect_simple(Simple {
        root_type: Some("lvm"),
        ..Simple::default()
    })
    .await
    .err()
    .unwrap_or_default();
    assert!(error.contains("Cannot identify parent"), "{error}");
}

// --- test_resize_layers.py --------------------------------------------------

#[derive(Default)]
struct Layers {
    btrfs: bool,
    key: Option<&'static str>,
    extra_lv: bool,
    multi_btrfs: bool,
}

const CRYPT: &str = "/dev/mapper/dm_crypt-0";
const LV: &str = "/dev/mapper/ubuntu--vg-ubuntu--lv";

/// `LayerInspectionTests.inspect`: Ubuntu's LUKS + LVM + ext4 root, or
/// Btrfs straight on LUKS.
async fn inspect_layers(options: Layers) -> Result<GuestLayout, String> {
    let mut rows = vec![
        row("/dev/sda", "disk", None, "8:0", None, 16 * GIB),
        row("/dev/sda3", "part", Some("/dev/sda"), "8:3", None, 16 * GIB),
        row(CRYPT, "crypt", Some("/dev/sda3"), "253:0", None, 16 * GIB),
    ];
    if !options.btrfs {
        rows.push(row(LV, "lvm", Some(CRYPT), "253:1", None, 16 * GIB));
    }
    rows.push(row("/dev/sdb", "disk", None, "8:16", Some(LABEL), 16 * GIB));
    let findmnt = json!({"filesystems": [{
        "source": if options.btrfs { format!("{CRYPT}[/@]") } else { LV.to_owned() },
        "fstype": if options.btrfs { "btrfs" } else { "ext4" },
        "target": "/",
        "options": "rw",
        "maj:min": if options.btrfs { "0:35" } else { "253:1" },
    }]})
    .to_string();
    let lsblk = json!({ "blockdevices": rows }).to_string();
    let total = if options.multi_btrfs { 2 } else { 1 };
    let extra_lv = options.extra_lv;
    let mut guest = FakeGuest::new(Box::new(move |argv: &[&str]| match argv[0] {
        "findmnt" => Ok(findmnt.clone()),
        "lsblk" => Ok(lsblk.clone()),
        "blkid" => Ok(match argv[2] {
            "UUID" => "uuid-root",
            "PARTUUID" => "part-id",
            "LABEL" => "",
            "TYPE" => "crypto_LUKS",
            other => panic!("unexpected blkid key {other}"),
        }
        .to_owned()),
        "btrfs" => Ok(format!(
            "Label: none uuid: btrfs-uuid\n\tTotal devices {total} FS bytes used 123\n\tdevid 3 size 12345 used 100 path {CRYPT}\n"
        )),
        "lvs" => {
            let item = json!({"lv_path": "/dev/ubuntu-vg/ubuntu-lv", "lv_name": "ubuntu-lv", "vg_name": "ubuntu-vg",
                "lv_size": (10 * GIB).to_string(), "lv_attr": "-wi-ao----", "segtype": "linear"});
            let mut items = vec![item.clone()];
            if extra_lv {
                let mut other = item;
                other["lv_path"] = json!("/dev/ubuntu-vg/other");
                items.push(other);
            }
            Ok(json!({"report": [{"lv": items}]}).to_string())
        }
        "vgs" => Ok(json!({"report": [{"vg": [{"vg_name": "ubuntu-vg", "vg_extent_size": "4194304"}]}]}).to_string()),
        "pvs" => Ok(json!({"report": [{"pv": [{"vg_name": "ubuntu-vg", "pv_name": CRYPT}]}]}).to_string()),
        other => panic!("unexpected command {other}: {argv:?}"),
    }));
    for (alias, real) in [
        (CRYPT, "/dev/dm-0"),
        (LV, "/dev/dm-1"),
        ("/dev/ubuntu-vg/ubuntu-lv", "/dev/dm-1"),
    ] {
        guest.aliases.insert(alias.to_owned(), real.to_owned());
    }
    for (path, content) in [
        (
            "/etc/crypttab",
            format!("dm_crypt-0 UUID=uuid-root {} luks\n", options.key.unwrap_or("none")),
        ),
        ("/sys/class/block/sda3/partition", "3".to_owned()),
        ("/sys/class/block/sda3/start", "3774873".to_owned()),
        ("/sys/class/block/dm-0/dm/name", "dm_crypt-0".to_owned()),
    ] {
        guest.files.insert(path.to_owned(), content);
    }
    run_inspect(&mut guest, "/", Role::Root).await
}

#[tokio::test]
async fn gold_luks_lvm_ext4_detects_file_key() -> Result<(), String> {
    let layout = inspect_layers(Layers {
        key: Some("/etc/cryptsetup-keys.d/dm_crypt-0.key"),
        ..Layers::default()
    })
    .await?;
    let crypt = layout.crypt.ok_or("no LUKS layer")?;
    assert_eq!(crypt.auth, CryptAuth::Keyfile);
    assert_eq!(crypt.device, "/dev/sda3");
    assert_eq!(layout.lvm.ok_or("no LVM layer")?.lv_path, "/dev/ubuntu-vg/ubuntu-lv");
    assert_eq!(layout.partition, Some(3));
    assert_eq!(layout.parent, "/dev/sda");
    Ok(())
}

#[tokio::test]
async fn no_key_requires_passphrase() -> Result<(), String> {
    let crypt = inspect_layers(Layers::default()).await?.crypt.ok_or("no LUKS layer")?;
    assert_eq!(crypt.auth, CryptAuth::Passphrase);
    assert_eq!(crypt.key_file, None);
    Ok(())
}

#[tokio::test]
async fn escaped_keyfile_path_is_decoded() -> Result<(), String> {
    let layout = inspect_layers(Layers {
        key: Some(r"/etc/cryptsetup-keys.d/root\040key"),
        ..Layers::default()
    })
    .await?;
    assert_eq!(
        layout.crypt.ok_or("no LUKS layer")?.key_file.as_deref(),
        Some("/etc/cryptsetup-keys.d/root key")
    );
    Ok(())
}

#[tokio::test]
async fn btrfs_subvolume_with_synthetic_device_number() -> Result<(), String> {
    let layout = inspect_layers(Layers {
        btrfs: true,
        ..Layers::default()
    })
    .await?;
    assert_eq!(layout.filesystem, "btrfs");
    assert_eq!(layout.btrfs_devid, Some(3));
    assert_eq!(layout.lvm, None);
    assert_eq!(layout.device, CRYPT);
    Ok(())
}

#[tokio::test]
async fn shared_vg_and_multidevice_btrfs_are_rejected() {
    let error = inspect_layers(Layers {
        extra_lv: true,
        ..Layers::default()
    })
    .await
    .err()
    .unwrap_or_default();
    assert!(error.contains("one logical volume"), "{error}");
    let error = inspect_layers(Layers {
        btrfs: true,
        multi_btrfs: true,
        ..Layers::default()
    })
    .await
    .err()
    .unwrap_or_default();
    assert!(error.contains("single-device"), "{error}");
}

// --- beyond Python: the script's other refusals -------------------------------

#[tokio::test]
async fn crypttab_rejects_bad_escapes_options_and_relative_keys() {
    for (key, expected) in [
        (r"/etc/key\x", "Unsupported escape in /etc/crypttab"),
        ("key.bin", "Crypttab key file must be an absolute path: key.bin"),
    ] {
        let error = inspect_layers(Layers {
            key: Some(key),
            ..Layers::default()
        })
        .await
        .err()
        .unwrap_or_default();
        assert_eq!(error, expected);
    }
    let error = inspect_layers(Layers {
        key: Some("/etc/key luks,keyscript=/bin/x"),
        ..Layers::default()
    })
    .await
    .err()
    .unwrap_or_default();
    assert!(error.starts_with("Unsupported /etc/crypttab"), "{error}");
}

#[tokio::test]
async fn home_role_on_the_root_disk_is_rejected() {
    let error = inspect_simple(Simple {
        home: true,
        source: Some("/dev/sda"),
        maj_min: Some("8:0"),
        ..Simple::default()
    })
    .await
    .err()
    .unwrap_or_default();
    assert_eq!(error, "Mounted filesystem does not match the selected workspace disk");
}

#[tokio::test]
async fn partition_must_be_the_last_on_its_disk() {
    let rows = vec![
        row("/dev/sda", "disk", None, "8:0", None, 16 * GIB),
        row("/dev/sda1", "part", Some("/dev/sda"), "8:1", None, 8 * GIB),
        row("/dev/sda2", "part", Some("/dev/sda"), "8:2", None, 8 * GIB),
        row("/dev/sdb", "disk", None, "8:16", Some(LABEL), 16 * GIB),
    ];
    let findmnt = json!({"filesystems": [{"source": "/dev/sda1", "fstype": "ext4", "target": "/",
        "options": "rw", "maj:min": "8:1"}]})
    .to_string();
    let lsblk = json!({ "blockdevices": rows }).to_string();
    let mut guest = FakeGuest::new(Box::new(move |argv: &[&str]| match argv[0] {
        "findmnt" => Ok(findmnt.clone()),
        "lsblk" => Ok(lsblk.clone()),
        other => panic!("unexpected command {other}"),
    }));
    for (path, value) in [
        ("/sys/class/block/sda1/partition", "1"),
        ("/sys/class/block/sda1/start", "2048"),
        ("/sys/class/block/sda2/start", "16779264"),
    ] {
        guest.files.insert(path.to_owned(), value.to_owned());
    }
    let error = run_inspect(&mut guest, "/", Role::Root).await.err().unwrap_or_default();
    assert_eq!(error, "Selected partition is not the last partition on disk");
}
