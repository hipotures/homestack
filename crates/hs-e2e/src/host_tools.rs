//! Prepare missing QEMU tools before allocating any lab resources.

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::Result;

pub async fn ensure(args: &[String]) -> Result<()> {
    let path = std::env::var_os("PATH").ok_or("PATH is not set")?;
    if ["qemu-system-x86_64", "qemu-img"]
        .iter()
        .all(|name| std::env::split_paths(&path).any(|directory| directory.join(name).is_file()))
    {
        return Ok(());
    }
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .map_err(|error| format!("/dev/kvm is unavailable: {error}; E2E requires KVM"))?;
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map_or_else(
            || std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")),
            |cache| Some(PathBuf::from(cache)),
        )
        .ok_or("HOME is not set")?
        .join("homestack-e2e/tools");
    let output = tokio::process::Command::new("python3")
        .args(["-c", include_str!("host-tools.py")])
        .arg(cache)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?
        .wait_with_output()
        .await?;
    if !output.status.success() {
        return Err("automatic E2E host-tool preparation failed".into());
    }
    let bin = PathBuf::from(String::from_utf8(output.stdout)?.trim());
    if !bin.is_absolute() || !bin.join("qemu-system-x86_64").is_file() {
        return Err("host-tool preparation returned no QEMU directory".into());
    }
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(&path));
    // Re-exec with the private PATH instead of mutating the environment of a
    // multithreaded runtime. No VM or DNS resources exist at this point.
    Err(Command::new(std::env::current_exe()?)
        .args(args)
        .env("PATH", std::env::join_paths(paths)?)
        .exec()
        .into())
}
