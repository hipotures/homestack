"""Disk-layout fixtures, executed only inside an owned disposable E2E VM."""

import json
import os
import pathlib
import shutil
import subprocess
import sys
import time

PASSPHRASE = "hs-resize-fixture-passphrase"


def run(*args, data=None):
    result = subprocess.run(args, input=data, text=True, capture_output=True)
    if result.returncode:
        raise RuntimeError(f"{args[0]} failed: {result.stderr[-3000:]}")
    return result.stdout.strip()


def wait_cloud_init():
    result = subprocess.run(["cloud-init", "status", "--wait", "--format", "json"],
                            text=True, capture_output=True, timeout=240)
    state = json.loads(result.stdout)
    assert result.returncode in (0, 2) and state["status"] == "done", f"Cloud-Init did not complete: {state}"
    for section in [state, *(state[name] for name in ("init-local", "init", "modules-config", "modules-final"))]:
        assert section["errors"] == [], f"Cloud-Init errors: {state}"
        # Proxmox's generated user-data still emits a deprecated `user` field.
        # Preserve those notices; do not accept WARNING/ERROR or fatal failures.
        assert set(section["recoverable_errors"]) <= {"DEPRECATED"}, f"Cloud-Init recoverable errors: {state}"
    return state


def disk_by_serial(serial):
    deadline = time.monotonic() + 30
    while True:
        devices = json.loads(run("lsblk", "-d", "-J", "-p", "-o", "PATH,SERIAL"))["blockdevices"]
        matches = [item["path"] for item in devices if (item.get("serial") or "").strip() == serial]
        assert len(matches) <= 1, f"Ambiguous fixture disk serial {serial}"
        if matches:
            disk = matches[0]
            break
        assert time.monotonic() < deadline, f"Fixture disk serial {serial} did not appear"
        time.sleep(.1)
    assert pathlib.Path(disk).is_block_device(), "Fixture serial does not identify a block device"
    source = run("findmnt", "-nro", "SOURCE", "-M", "/").split("[", 1)[0]
    ancestry = json.loads(run("lsblk", "-s", "-J", "-p", "-o", "PATH", source))
    def paths(items):
        for item in items:
            yield item["path"]
            yield from paths(item.get("children", []))
    assert disk not in set(paths(ancestry["blockdevices"])), "Refusing to format the active root disk"
    return disk


def relabel_home():
    disk = disk_by_serial("HS_HOME_240")
    run("e2label", disk, "HS_HOME_240")
    print(json.dumps({"home_disk": disk}))


def prepare(role, layout):
    assert role in ("root", "home")
    disk = disk_by_serial("HS_ROOT_240" if role == "root" else "HS_HOME_240")
    mount = "/mnt/hst-newroot" if role == "root" else "/home/user"
    assert pathlib.Path(disk).is_block_device()
    if role == "home":
        # Fixture users do not need a login session while their home is replaced.
        run("systemctl", "stop", "user@1000.service")
        subprocess.run(["pkill", "-KILL", "-u", "1000"], capture_output=True)
        mounted = subprocess.run(["findmnt", "-rn", "-M", mount], capture_output=True)
        if mounted.returncode == 0:
            run("umount", mount)
    run("wipefs", "-a", disk)
    device = disk
    if layout in ("ext4-partition", "partition-not-last", "btrfs-multi"):
        size = "4G" if role == "root" else "384M"
        rows = ",,L\n" if layout == "ext4-partition" else f",{size},L\n,,L\n"
        run("sfdisk", "--wipe", "always", disk, data="label: gpt\n" + rows)
        run("udevadm", "settle")
        device = disk + "1"
    encrypted = layout in ("luks-key-lvm", "luks-pass-lvm", "btrfs-luks")
    if encrypted:
        key = pathlib.Path("/root/hst-resize.key")
        key.write_text(PASSPHRASE)
        key.chmod(0o600)
        mapper = "hsrootcrypt" if role == "root" else "hshomecrypt"
        run("cryptsetup", "luksFormat", "--type", "luks2", "--batch-mode", "--pbkdf", "pbkdf2",
            "--iter-time", "20", "--key-file", str(key), device)
        uuid = run("cryptsetup", "luksUUID", device)
        run("cryptsetup", "open", "--key-file", str(key), device, mapper)
        key_field = "none" if layout == "luks-pass-lvm" else str(key)
        with open("/etc/crypttab", "a") as stream:
            stream.write(f"{mapper} UUID={uuid} {key_field} luks\n")
        if role == "root":
            hook = pathlib.Path("/etc/initramfs-tools/scripts/init-premount/hst-resize")
            hook.write_text(hook.read_text().replace("@HST_ROOT_DEVICE@", "/dev/disk/by-uuid/" + uuid))
            run("update-initramfs", "-u", "-k", run("uname", "-r"))
        device = "/dev/mapper/" + mapper
    if layout in ("lvm", "luks-key-lvm", "luks-pass-lvm", "lvm-multi"):
        vg = "hsrootvg" if role == "root" else "hshomevg"
        run("pvcreate", "-y", device)
        run("vgcreate", vg, device)
        if layout == "lvm-multi":
            run("lvcreate", "-L", "16M", "-n", "extra", vg)
        run("lvcreate", "-l", "100%FREE", "-n", "data", vg)
        device = f"/dev/{vg}/data"
    btrfs = layout.startswith("btrfs")
    fs = "btrfs" if btrfs else "ext4"
    label = "HS_ROOT_240" if role == "root" else "HS_HOME_240"
    if btrfs:
        devices = [device, disk + "2"] if layout == "btrfs-multi" else [device]
        run("mkfs.btrfs", "-f", "-L", label, *devices)
    else:
        run("mkfs.ext4", "-F", "-L", label, device)
    pathlib.Path(mount).mkdir(parents=True, exist_ok=True)
    run("mount", device, mount)
    if btrfs:
        run("btrfs", "subvolume", "create", mount + "/@")
        run("umount", mount)
        run("mount", "-o", "subvol=@", device, mount)
    uuid = run("blkid", "-s", "UUID", "-o", "value", device)
    if role == "root":
        run("rsync", "-aHAXx", "--numeric-ids", "--exclude=/mnt/*", "--exclude=/proc/*",
            "--exclude=/sys/*", "--exclude=/dev/*", "--exclude=/run/*", "--exclude=/tmp/*",
            "--exclude=/home/user/*", "/", mount + "/")
        options = "defaults,subvol=@" if btrfs else "defaults"
        pathlib.Path(mount + "/etc/fstab").write_text(
            f"UUID={uuid} / {fs} {options} 0 0\nLABEL=HS_HOME_240 /home/user ext4 defaults 0 2\n")
        run("sync")
        run("umount", mount)
    else:
        os.chown(mount, 1000, 1000)
        # SSH owns this fixture command. Restart the agent so its private
        # namespace inherits the newly mounted filesystem from PID 1.
        run("systemctl", "restart", "qemu-guest-agent.service")
    print(json.dumps({"uuid": uuid, "filesystem": fs, "encrypted": encrypted,
                      "root": f"/dev/{'hsrootvg'}/data" if "lvm" in layout else
                      ("/dev/mapper/hsrootcrypt" if encrypted else "UUID=" + uuid),
                      "rootflags": "subvol=@" if btrfs else "defaults"}))


def authorize(public_key):
    pathlib.Path("/root/.ssh").mkdir(mode=0o700, exist_ok=True)
    authorized = pathlib.Path("/root/.ssh/authorized_keys")
    key_body = public_key.split()[1]
    lines = authorized.read_text().splitlines() if authorized.exists() else []
    authorized.write_text("\n".join(line for line in lines if key_body not in line) + "\n" + public_key + "\n")
    authorized.chmod(0o600)


def baseline(public_key):
    cloud_init = wait_cloud_init()
    disk = disk_by_serial("HS_HOME_241")
    blank = subprocess.run(["blkid", "-p", disk], capture_output=True)
    assert blank.returncode == 2, "Refusing to format a nonblank new baseline home"
    log = open("/root/hst-resize-packages.log", "w")
    pathlib.Path("/etc/cryptsetup-initramfs").mkdir(parents=True, exist_ok=True)
    pathlib.Path("/etc/cryptsetup-initramfs/conf-hook").write_text("CRYPTSETUP=y\n")
    hook = pathlib.Path("/etc/initramfs-tools/scripts/init-premount/hst-resize")
    hook.parent.mkdir(parents=True, exist_ok=True)
    hook.write_text("#!/bin/sh\nPREREQ=\ncase \"$1\" in prereqs) exit 0;; esac\n"
                    "case \" $(cat /proc/cmdline) \" in *' hst.crypt=1 '*)\n"
                    "  n=0; while [ ! -b @HST_ROOT_DEVICE@ ] && [ $n -lt 100 ]; do sleep .1; n=$((n+1)); done\n"
                    "  modprobe dm_crypt\n"
                    f"  printf %s {PASSPHRASE} | cryptsetup open --key-file - @HST_ROOT_DEVICE@ hsrootcrypt\n"
                    "esac\n")
    hook.chmod(0o755)
    env = dict(os.environ, DEBIAN_FRONTEND="noninteractive")
    try:
        subprocess.run(["apt-get", "update"], env=env, stdout=log, stderr=log, check=True)
        subprocess.run(["apt-get", "-o", "Dpkg::Options::=--force-confold", "install", "-y",
                        "--no-install-recommends", "lvm2", "cryptsetup", "cryptsetup-initramfs",
                        "btrfs-progs", "cloud-guest-utils", "rsync"],
                       env=env, stdout=log, stderr=log, check=True)
    except subprocess.CalledProcessError:
        log.flush()
        print(pathlib.Path(log.name).read_text()[-4000:], file=sys.stderr)
        raise
    subprocess.run(["apt-get", "clean"], stdout=log, stderr=log, check=True)
    run("systemctl", "mask", "user@1000.service")
    run("systemctl", "stop", "user@1000.service")
    # Cloud-Init may already have this fixture key with a forced login command.
    authorize(public_key)
    run("mkfs.ext4", "-F", "-L", "HS_HOME_241", disk)
    pathlib.Path("/mnt/hst-home").mkdir(exist_ok=True)
    run("mount", disk, "/mnt/hst-home")
    run("cp", "-a", "/home/user/.", "/mnt/hst-home/")
    run("umount", "/mnt/hst-home")
    with open("/etc/fstab", "a") as stream:
        stream.write("\nLABEL=HS_HOME_241 /home/user ext4 defaults,nofail,x-systemd.device-timeout=1s 0 2\n")
    run("mount", "/home/user")
    print(json.dumps({"kernel": run("uname", "-r"), "cloud_init": cloud_init}))


def snapshot(role):
    mount = "/" if role == "root" else "/home/user"
    source = run("findmnt", "-nro", "SOURCE", "-M", mount).split("[", 1)[0]
    ancestry = json.loads(run("lsblk", "-s", "-J", "-o", "PATH,TYPE", source))
    disks = set()
    def collect(items):
        for item in items:
            if item["type"] == "disk":
                disks.add(item["path"])
            collect(item.get("children", []))
    collect(ancestry["blockdevices"])
    assert len(disks) == 1, ancestry
    disk = disks.pop()
    stat = os.statvfs(mount)
    marker = pathlib.Path(mount) / "hst-resize-marker"
    identity = marker.stat()
    print(json.dumps({"disk_bytes": int(run("blockdev", "--getsize64", disk)),
                      "filesystem_bytes": stat.f_frsize * stat.f_blocks,
                      "marker": marker.read_text(),
                      "filesystem_uuid": run("findmnt", "-nro", "UUID", "-M", mount),
                      "marker_identity": [identity.st_uid, identity.st_gid, identity.st_mode & 0o7777],
                      "mount": json.loads(run("findmnt", "-J", "-M", mount)),
                      "block": json.loads(run("lsblk", "-J", "-b", "-o", "NAME,TYPE,SIZE,FSTYPE,MOUNTPOINTS"))}))


def barrier(action, fs):
    tool = shutil.which("btrfs" if fs == "btrfs" else "resize2fs")
    assert tool
    tool = str(pathlib.Path(tool).resolve())
    real = tool + ".hst-real"
    if action == "install":
        assert not pathlib.Path(real).exists()
        os.rename(tool, real)
        condition = '[ "$1 $2" = "filesystem resize" ]' if fs == "btrfs" else "true"
        pathlib.Path(tool).write_text(
            f"#!/bin/sh\nif {condition}; then\n"
            " echo $$ > /run/hst-resize-blocked\n while :; do sleep 1; done\nfi\n"
            f'exec {real} "$@"\n')
        os.chmod(tool, 0o755)
    else:
        marker = pathlib.Path("/run/hst-resize-blocked")
        if marker.exists():
            os.kill(int(marker.read_text()), 9)
            marker.unlink()
        os.replace(real, tool)
    print(json.dumps({"barrier": action}))


if __name__ == "__main__":
    action, *args = sys.argv[1:]
    if action == "baseline":
        baseline(*args)
    elif action == "cloud-init-ready":
        print(json.dumps(wait_cloud_init()))
    elif action == "relabel-home":
        relabel_home()
    elif action == "authorize":
        authorize(*args)
    elif action == "prepare":
        prepare(*args)
    elif action == "snapshot":
        snapshot(*args)
    elif action == "barrier":
        barrier(*args)
    else:
        raise RuntimeError("Unknown resize fixture action")
