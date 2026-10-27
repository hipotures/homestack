use crate::Trace;
use rustix::fs::{FileType, Mode, OFlags, ResolveFlags};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn error(message: impl ToString) -> Value {
    json!({"filesystems": null, "error": message.to_string()})
}

fn unescape(s: &str) -> Result<String, String> {
    let bytes = s.as_bytes();
    let mut result = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            let digits = bytes.get(i + 1..i + 4).ok_or("invalid mountinfo escape")?;
            if !digits.iter().all(|c| (b'0'..=b'7').contains(c)) {
                return Err("invalid mountinfo escape".into());
            }
            let n =
                (u16::from(digits[0] - b'0') * 64) + (u16::from(digits[1] - b'0') * 8) + u16::from(digits[2] - b'0');
            result.push(u8::try_from(n).map_err(|_| "invalid mountinfo escape")?);
            i += 4;
        } else {
            result.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(result).map_err(|e| e.to_string())
}

fn data_mount(mount: &str, kind: &str) -> bool {
    if mount == "/" || mount == "/home/app" {
        return true;
    }
    if ["/proc", "/sys", "/dev", "/run"]
        .iter()
        .any(|base| mount == *base || mount.starts_with(&format!("{base}/")))
    {
        return false;
    }
    !matches!(
        kind,
        "proc"
            | "sysfs"
            | "tmpfs"
            | "devtmpfs"
            | "devpts"
            | "cgroup"
            | "cgroup2"
            | "mqueue"
            | "securityfs"
            | "debugfs"
            | "tracefs"
            | "pstore"
            | "hugetlbfs"
            | "configfs"
            | "fusectl"
            | "autofs"
    )
}

fn init_pid(group: &Path, trace: &mut Trace, depth: u32) -> Option<u32> {
    if depth > 32 {
        return None;
    }
    let procs = trace.read(&group.join("cgroup.procs")).ok()?;
    for pid in procs.split_whitespace().filter_map(|p| p.parse::<u32>().ok()) {
        let status = match trace.read(Path::new(&format!("/proc/{pid}/status"))) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if status.lines().any(|line| {
            line.strip_prefix("NSpid:").is_some_and(|ids| {
                let ids: Vec<_> = ids.split_whitespace().collect();
                ids.len() >= 2 && ids.last() == Some(&"1")
            })
        }) {
            return Some(pid);
        }
    }
    let mut dirs: Vec<_> = fs::read_dir(group)
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs.into_iter().find_map(|dir| init_pid(&dir, trace, depth + 1))
}

// Resolve untrusted guest mount paths relative to a pinned root descriptor.
// RESOLVE_IN_ROOT confines absolute symlinks and '..' to that root.
fn stat(root: &OwnedFd, mount: &str) -> Result<Value, String> {
    let fd = rustix::fs::openat2(
        root,
        mount,
        OFlags::PATH | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::IN_ROOT | ResolveFlags::NO_MAGICLINKS,
    )
    .map_err(|e| e.to_string())?;
    let metadata = rustix::fs::fstat(&fd).map_err(|e| e.to_string())?;
    if FileType::from_raw_mode(metadata.st_mode) != FileType::Directory {
        return Err("not a directory mount".into());
    }
    let s = rustix::fs::statvfs(format!("/proc/self/fd/{}", fd.as_raw_fd())).map_err(|e| e.to_string())?;
    let bytes = |blocks: u64| blocks.checked_mul(s.f_frsize).ok_or("filesystem size overflow");
    Ok(
        json!({"total_bytes": bytes(s.f_blocks)?, "used_bytes": bytes(s.f_blocks.checked_sub(s.f_bfree).ok_or("invalid free blocks")?)?,
        "free_bytes": bytes(s.f_bfree)?, "available_bytes": bytes(s.f_bavail)?,
        "inodes_total": s.f_files, "inodes_free": s.f_ffree}),
    )
}

fn lxc_inner(group: &Path, trace: &mut Trace) -> Result<Value, String> {
    let pid = init_pid(group, trace, 0).ok_or("cannot find container init PID in cgroup subtree")?;
    let root = rustix::fs::open(
        format!("/proc/{pid}/root"),
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| e.to_string())?;
    let text = trace.read(Path::new(&format!("/proc/{pid}/mountinfo")))?;
    let mut mounts = BTreeMap::new();
    for line in text.lines() {
        let (left, right) = line.split_once(" - ").ok_or("invalid mountinfo record")?;
        let fields: Vec<_> = left.split_whitespace().collect();
        if fields.len() < 6 {
            return Err("invalid mountinfo fields".into());
        }
        let rest: Vec<_> = right.split_whitespace().collect();
        if rest.len() < 2 {
            return Err("invalid mountinfo filesystem fields".into());
        }
        let mount = unescape(fields[4])?;
        mounts.insert(mount, (unescape(rest[0])?, unescape(rest[1])?, unescape(fields[3])?));
    }
    let mut results = Vec::new();
    for (mount, (kind, source, subroot)) in mounts {
        if !data_mount(&mount, &kind) {
            continue;
        }
        let started = Instant::now();
        let source_path = format!("/proc/{pid}/root{mount}");
        let mut record = json!({"mount": mount, "filesystem": kind, "source": source, "filesystem_root": subroot,
            "mounted": true, "total_bytes": null, "used_bytes": null, "free_bytes": null, "available_bytes": null, "error": null});
        match stat(&root, &mount) {
            Ok(values) => {
                if let (Some(record), Some(values)) = (record.as_object_mut(), values.as_object()) {
                    record.extend(values.clone());
                }
            }
            Err(e) => {
                record["error"] = json!(e);
            }
        }
        trace.record(&format!("statvfs:{source_path}"), 0, started, &record);
        results.push(record);
    }
    if !results.iter().any(|r| r["mount"] == "/home/app") {
        results.push(json!({"mount": "/home/app", "mounted": false, "total_bytes": null,
            "used_bytes": null, "free_bytes": null, "available_bytes": null, "error": "not_mounted"}));
    }
    Ok(json!({"source": "proc_mountinfo_statvfs", "init_pid": pid, "filesystems": results, "error": null}))
}

pub fn lxc(group: &Path, trace: &mut Trace) -> Value {
    lxc_inner(group, trace).unwrap_or_else(error)
}

fn response(
    stream: &mut BufReader<UnixStream>,
    deadline: Instant,
    trace: &mut Trace,
    label: &str,
    sync_token: Option<u64>,
) -> Result<Value, String> {
    let started = Instant::now();
    let mut count = 0;
    let mut bytes = Vec::new();
    let mut seen_sentinel = false;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or("guest agent timeout")?;
        stream
            .get_ref()
            .set_read_timeout(Some(remaining))
            .map_err(|e| e.to_string())?;
        let chunk = stream.fill_buf().map_err(|e| e.to_string())?;
        if chunk.is_empty() {
            return Err("guest agent closed connection".into());
        }
        let newline = chunk.iter().position(|b| *b == b'\n');
        let size = newline.map_or(chunk.len(), |p| p + 1);
        count += size;
        if count > 4 * 1024 * 1024 {
            return Err("guest agent response exceeds 4 MiB".into());
        }
        for byte in &chunk[..size] {
            if *byte == 0xff {
                bytes.clear();
                seen_sentinel = true;
            } else {
                bytes.push(*byte);
            }
        }
        stream.consume(size);
        if newline.is_none() {
            continue;
        }
        // Flushing the agent parser can itself produce an error response.
        // Ignore all pre-sentinel data and unrelated replies until our sync
        // token arrives, as required by the QGA synchronization protocol.
        if sync_token.is_some() && !seen_sentinel {
            if trace.debug {
                eprintln!(
                    "[debug] sync_discard source={label} bytes={} reason=before_sentinel",
                    bytes.len()
                );
            }
            bytes.clear();
            continue;
        }
        if bytes.iter().all(u8::is_ascii_whitespace) {
            bytes.clear();
            continue;
        }
        let value: Value = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(e) if sync_token.is_some() => {
                if trace.debug {
                    eprintln!("[debug] sync_discard source={label} bytes={} reason={e}", bytes.len());
                }
                bytes.clear();
                continue;
            }
            Err(e) => return Err(e.to_string()),
        };
        if sync_token.is_some_and(|token| value["return"].as_u64() != Some(token)) {
            if trace.debug {
                eprintln!(
                    "[debug] sync_discard source={label} bytes={} reason=unmatched_token reply={value}",
                    bytes.len()
                );
            }
            bytes.clear();
            continue;
        }
        trace.record(label, count, started, &value);
        return Ok(value);
    }
}

pub fn qemu(id: u32, trace: &mut Trace) -> Value {
    qemu_socket(&format!("/run/qemu-server/{id}.qga"), trace)
}

fn qemu_socket(socket: &str, trace: &mut Trace) -> Value {
    let started = Instant::now();
    let result = (|| -> Result<Value, String> {
        let mut stream = UnixStream::connect(socket).map_err(|e| e.to_string())?;
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .map_err(|e| e.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(3);
        let token = (SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos()
            % (1_u128 << 52)) as u64;
        let request = format!(
            "{}\n",
            json!({"execute": "guest-sync-delimited", "arguments": {"id": token}})
        );
        // The protocol sentinel is a raw byte, not UTF-8 U+00FF.
        stream.write_all(&[0xff]).map_err(|e| e.to_string())?;
        stream.write_all(request.as_bytes()).map_err(|e| e.to_string())?;
        let mut stream = BufReader::new(stream);
        response(
            &mut stream,
            deadline,
            trace,
            &format!("{socket}:guest-sync-delimited"),
            Some(token),
        )?;
        stream
            .get_mut()
            .write_all(b"{\"execute\":\"guest-get-fsinfo\"}\n")
            .map_err(|e| e.to_string())?;
        let value = response(
            &mut stream,
            deadline,
            trace,
            &format!("{socket}:guest-get-fsinfo"),
            None,
        )?;
        if let Some(e) = value.get("error") {
            return Err(e.to_string());
        }
        let filesystems = value["return"]
            .as_array()
            .ok_or("guest-get-fsinfo did not return an array")?;
        let filesystems: Vec<_> = filesystems
            .iter()
            .map(|fs| {
                let used = fs["used-bytes"].as_u64();
                let total = fs["total-bytes"].as_u64();
                let privileged = fs["total-bytes-privileged"].as_u64();
                json!({"mount": fs["mountpoint"], "filesystem": fs["type"], "name": fs["name"], "mounted": true,
                "used_bytes": used, "total_bytes": privileged,
                "free_bytes": privileged.zip(used).and_then(|(t,u)| t.checked_sub(u)),
                "available_bytes": total.zip(used).and_then(|(t,u)| t.checked_sub(u)),
                "total_bytes_unprivileged": total, "guest_agent": fs})
            })
            .collect();
        Ok(json!({"source": socket, "filesystems": filesystems, "error": null}))
    })();
    let result = result.unwrap_or_else(error);
    if trace.debug {
        eprintln!(
            "[debug] guest_filesystems source={socket} elapsed_usec={} error={}",
            started.elapsed().as_micros(),
            result["error"]
        );
    }
    result
}
