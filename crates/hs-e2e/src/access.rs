//! Concrete client access instructions for local and remote runners.

use std::net::IpAddr;

use crate::{Options, Result, phases::shell_quote};

pub fn validate_destination(destination: &str) -> Result<()> {
    if destination.is_empty()
        || destination.starts_with('-')
        || destination.chars().any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return Err("--runner-ssh must be an SSH destination or configured host alias".into());
    }
    Ok(())
}

fn session_destination(user: &str, connection: &str) -> Option<(String, u16)> {
    let fields: Vec<_> = connection.split_whitespace().collect();
    if fields.len() != 4 || user.is_empty() {
        return None;
    }
    let address: IpAddr = fields[2].parse().ok()?;
    let port: u16 = fields[3].parse().ok()?;
    if address.is_unspecified() || port == 0 {
        return None;
    }
    let destination = format!("{user}@{address}");
    validate_destination(&destination).ok()?;
    Some((destination, port))
}

pub fn configure(options: &mut Options) {
    if options.runner_ssh.is_none()
        && let (Ok(user), Ok(connection)) = (std::env::var("USER"), std::env::var("SSH_CONNECTION"))
        && let Some((destination, port)) = session_destination(&user, &connection)
    {
        options.runner_ssh = Some(destination);
        options.runner_ssh_port = options.runner_ssh_port.or(Some(port));
    }
}

pub fn tunnel(options: &Options, api_port: u16) -> Option<String> {
    if !options.web_bind.is_loopback() {
        return None;
    }
    let destination = options.runner_ssh.as_deref()?;
    let port = options
        .runner_ssh_port
        .map(|port| format!(" -p {port}"))
        .unwrap_or_default();
    Some(format!(
        "ssh -N -o ExitOnForwardFailure=yes -L 127.0.0.1:18006:{}:{api_port}{port} -- {}",
        options.web_bind,
        shell_quote(destination)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_session_uses_the_actual_server_address_and_port() {
        assert_eq!(
            session_destination("user", "192.168.1.50 53123 192.168.1.100 2222"),
            Some(("user@192.168.1.100".into(), 2222))
        );
        assert!(session_destination("user", "invalid").is_none());
        assert!(session_destination("user", "192.168.1.50 53123 0.0.0.0 22").is_none());
    }

    #[test]
    fn local_runs_have_no_tunnel_and_explicit_targets_produce_complete_commands() -> Result<()> {
        let options = Options::parse(&["run".into()], "test")?;
        assert!(tunnel(&options, 33721).is_none());
        let options = Options::parse(
            &[
                "run".into(),
                "--runner-ssh".into(),
                "user@192.168.1.100".into(),
                "--runner-ssh-port".into(),
                "2222".into(),
            ],
            "test",
        )?;
        assert_eq!(
            tunnel(&options, 33721).as_deref(),
            Some(
                "ssh -N -o ExitOnForwardFailure=yes -L 127.0.0.1:18006:127.0.0.1:33721 -p 2222 -- 'user@192.168.1.100'"
            )
        );
        for invalid in ["", "-oProxyCommand=bad", "user@host\ncommand"] {
            assert!(validate_destination(invalid).is_err());
        }
        Ok(())
    }
}
