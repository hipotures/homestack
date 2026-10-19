//! The latest API release reachable from this client's source revision.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|text| !text.is_empty())
}

fn version(tag: &str) -> Option<(u64, u64, u64)> {
    let parts = tag.split('.').collect::<Vec<_>>();
    if parts.len() != 3
        || parts.iter().any(|part| {
            part.is_empty()
                || !part.bytes().all(|byte| byte.is_ascii_digit())
                || (part.len() > 1 && part.starts_with('0'))
        })
    {
        return None;
    }
    Some((parts[0].parse().ok()?, parts[1].parse().ok()?, parts[2].parse().ok()?))
}

fn main() {
    if let Some(tags) = git(&["tag", "--merged", "HEAD"])
        && let Some(tag) = tags
            .lines()
            .filter_map(|tag| version(tag).map(|version| (version, tag)))
            .max()
    {
        println!("cargo:rustc-env=HOMESTACK_API_VERSION={}", tag.1);
    }
    for path in ["HEAD", "packed-refs", "refs/tags"] {
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
