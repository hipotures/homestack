//! The running daemon's mode, switched through a root-only Unix socket: a
//! request is one line naming a mode, or empty to ask for the current one.
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::Duration;

/// Serve requests on `path`; each switch to one of `modes` is sent to
/// `switches`, and `current` answers what the daemon runs.
pub fn listen(
    path: &Path,
    modes: Vec<String>,
    current: Arc<Mutex<String>>,
    switches: mpsc::Sender<String>,
) -> Result<(), String> {
    // A previous daemon's socket; the daemon lock rules out a live one.
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).map_err(|e| format!("{}: {e}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    std::thread::Builder::new()
        .name("control".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = serve(&stream, &modes, &current, &switches);
            }
        })
        .map_err(|e| format!("cannot start the control socket: {e}"))?;
    Ok(())
}

fn serve(
    stream: &UnixStream,
    modes: &[String],
    current: &Mutex<String>,
    switches: &mpsc::Sender<String>,
) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut line = String::new();
    BufReader::new(stream).take(256).read_line(&mut line)?;
    let wanted = line.trim();
    let mut current = current.lock().unwrap_or_else(PoisonError::into_inner);
    let reply = if wanted.is_empty() {
        current.clone()
    } else if modes.iter().any(|mode| mode == wanted) {
        wanted.clone_into(&mut current);
        let _ = switches.send(wanted.to_owned());
        current.clone()
    } else {
        format!("error: unknown mode {wanted}; modes: {}", modes.join(", "))
    };
    writeln!(&mut &*stream, "{reply}")
}

/// Ask the daemon at `path` to switch to `mode`, or for its current one.
pub fn request(path: &Path, mode: Option<&str>) -> Result<String, String> {
    let mut stream =
        UnixStream::connect(path).map_err(|e| format!("cannot reach the daemon at {}: {e}", path.display()))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    writeln!(stream, "{}", mode.unwrap_or_default()).map_err(|e| e.to_string())?;
    let mut reply = String::new();
    BufReader::new(stream)
        .read_line(&mut reply)
        .map_err(|e| e.to_string())?;
    let reply = reply.trim();
    match reply.strip_prefix("error: ") {
        Some(error) => Err(error.to_owned()),
        None if reply.is_empty() => Err("the daemon gave no answer".into()),
        None => Ok(reply.to_owned()),
    }
}
