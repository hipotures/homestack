//! A HomeStack profile must retain access to the user's existing Herdr session.

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn herdr_keeps_desktop_paths_session_and_arguments_inside_a_profile() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::Builder::new().prefix("herdr profile's ").tempdir()?;
    let executable = dir.path().join("desktop herdr");
    std::fs::write(
        &executable,
        "#!/bin/sh\nprintf '%s\\n' \"$HOME\" \"$XDG_CONFIG_HOME\" \"$HERDR_SOCKET_PATH\" \"$HERDR_SESSION\" \"$@\"\n",
    )?;
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))?;
    let home = dir.path().join("desktop home");
    let config_home = dir.path().join("desktop config");
    let wrapper = dir.path().join("profile-herdr");
    std::fs::write(
        &wrapper,
        hs_config::profiles::herdr_wrapper(&executable, &home, &config_home),
    )?;
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700))?;
    let output = Command::new(&wrapper)
        .env("HOME", dir.path().join("profile/home"))
        .env("XDG_CONFIG_HOME", dir.path().join("profile/config"))
        .env("HERDR_SOCKET_PATH", "/chosen/herdr.sock")
        .env("HERDR_SESSION", "chosen-session")
        .args(["pane", "read", "PVE/lab pane"])
        .output()?;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        String::from_utf8(output.stdout)?.lines().collect::<Vec<_>>(),
        [
            home.to_str().ok_or("home is not UTF-8")?,
            config_home.to_str().ok_or("config home is not UTF-8")?,
            "/chosen/herdr.sock",
            "chosen-session",
            "pane",
            "read",
            "PVE/lab pane",
        ]
    );
    Ok(())
}
