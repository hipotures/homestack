//! The commit this client is built from, for the server image's tag
//! (`<version>-<commit>`, `docs/migration/phase-13c.md` section 8).

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
