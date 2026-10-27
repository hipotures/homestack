//! Host-visible cgroup constraints, including all ancestors up to the root.
use crate::Trace;
use serde_json::{Value, json};
use std::io;
use std::path::Path;
use std::time::Instant;

pub(crate) fn cpu_count(text: &str) -> Option<u64> {
    if text.is_empty() {
        return None;
    }
    let mut previous = None;
    text.split(',').try_fold(0_u64, |count, range| {
        let (first, last) = range.split_once('-').unwrap_or((range, range));
        let (first, last) = (first.parse::<u64>().ok()?, last.parse::<u64>().ok()?);
        if previous.is_some_and(|end| first <= end) {
            return None;
        }
        previous = Some(last);
        count.checked_add(last.checked_sub(first)?.checked_add(1)?)
    })
}

fn read(path: &Path, trace: &mut Trace) -> Result<Option<String>, String> {
    match trace.read_io(path) {
        Ok(text) => Ok(Some(text)),
        // Interfaces are absent at the root or when a controller is disabled.
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let dir = path.parent().ok_or("cgroup interface has no parent")?;
            std::fs::metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            Ok(None)
        }
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn cpu(root: &Path, path: &Path, trace: &mut Trace) -> Result<f64, String> {
    // Online host CPUs bound capacity even if the cpuset controller is disabled.
    // The agent process's own affinity must not constrain a guest's capacity.
    let online = Path::new("/sys/devices/system/cpu/online");
    let text = trace.read(online).map_err(|e| format!("{}: {e}", online.display()))?;
    let mut capacity = cpu_count(text.trim()).ok_or("invalid host online CPU list")? as f64;
    for dir in path.ancestors().take_while(|dir| dir.starts_with(root)) {
        let file = dir.join("cpuset.cpus.effective");
        if let Some(text) = read(&file, trace)? {
            let count = cpu_count(text.trim()).ok_or_else(|| format!("{}: invalid CPU list", file.display()))?;
            capacity = capacity.min(count as f64);
        }
        let file = dir.join("cpu.max");
        if let Some(text) = read(&file, trace)? {
            let words: Vec<_> = text.split_whitespace().collect();
            let invalid = || format!("{}: invalid CPU quota/period", file.display());
            if words.len() != 2 {
                return Err(invalid());
            }
            let period = words[1].parse::<u64>().ok().filter(|n| *n > 0).ok_or_else(invalid)?;
            if words[0] != "max" {
                let quota = words[0].parse::<u64>().ok().filter(|n| *n > 0).ok_or_else(invalid)?;
                capacity = capacity.min(quota as f64 / period as f64);
            }
        }
    }
    Ok(capacity)
}

fn memory(root: &Path, path: &Path, trace: &mut Trace) -> Result<Value, String> {
    let mut limit: Option<u64> = None;
    for dir in path.ancestors().take_while(|dir| dir.starts_with(root)) {
        let file = dir.join("memory.max");
        if let Some(text) = read(&file, trace)?
            && text.trim() != "max"
        {
            let value = text
                .trim()
                .parse::<u64>()
                .map_err(|_| format!("{}: invalid memory limit", file.display()))?;
            limit = Some(limit.map_or(value, |limit| limit.min(value)));
        }
    }
    Ok(limit.map_or_else(|| json!("max"), |limit| json!(limit)))
}

pub(crate) fn collect(root: &Path, path: &Path, trace: &mut Trace) -> Value {
    let started = Instant::now();
    let result = (|| {
        let root = root.canonicalize().map_err(|e| format!("{}: {e}", root.display()))?;
        let path = path.canonicalize().map_err(|e| format!("{}: {e}", path.display()))?;
        if !path.starts_with(&root) {
            return Err("cgroup is outside the configured cgroup root".to_owned());
        }
        Ok((cpu(&root, &path, trace), memory(&root, &path, trace)))
    })();
    let (cpu, memory) = match result {
        Ok(values) => values,
        Err(error) => (Err(error.clone()), Err(error)),
    };
    let value = json!({
        "cpu_capacity_cpus": cpu.as_ref().ok(),
        "memory_limit_bytes": memory.as_ref().ok(),
        "read_errors": {"cpu": cpu.as_ref().err(), "memory": memory.as_ref().err()}
    });
    trace.record(&format!("effective_limits:{}", path.display()), 0, started, &value);
    value
}
