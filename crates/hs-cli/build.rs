//! The commit this client is built from and the image's patch number, for
//! the server image's tag (`<major>.<minor>.<patch>-<commit>`,
//! `docs/migration/phase-13c.md` section 8). The patch is the number of
//! commits since `version` last changed in the workspace's `Cargo.toml`, as
//! the CI counts it; a new major or minor version starts it again.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|text| !text.is_empty())
}

fn main() {
    if let Some(commit) = git(&["rev-parse", "HEAD"]) {
        println!("cargo:rustc-env=HOMESTACK_COMMIT={commit}");
    }
    if let Some(top) = git(&["rev-parse", "--show-toplevel"]) {
        let manifest = format!("{top}/Cargo.toml");
        if let Some(since) = git(&["log", "-1", "--format=%H", "-G", "^version = ", "--", &manifest])
            && let Some(count) = git(&["rev-list", "--count", &format!("{since}..HEAD")])
        {
            println!("cargo:rustc-env=HOMESTACK_PATCH={count}");
        }
        println!("cargo:rerun-if-changed={manifest}");
    }
    for path in ["HEAD", "packed-refs"] {
        if let Some(file) = git(&["rev-parse", "--git-path", path]) {
            println!("cargo:rerun-if-changed={file}");
        }
    }
    if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"])
        && let Some(file) = git(&["rev-parse", "--git-path", &reference])
    {
        println!("cargo:rerun-if-changed={file}");
    }
}
