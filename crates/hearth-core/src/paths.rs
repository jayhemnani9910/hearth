//! Filesystem locations (XDG) and the private runtime directory.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;

fn uid() -> u32 {
    unsafe { libc::getuid() }
}

fn env_dir(var: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home().join(fallback),
    }
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// `$XDG_RUNTIME_DIR/hearth`, or `/tmp/hearth-<uid>` when unset.
pub fn runtime_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("HEARTH_RUNTIME_DIR") {
        return PathBuf::from(d);
    }
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(v) if !v.is_empty() => PathBuf::from(v).join("hearth"),
        _ => PathBuf::from(format!("/tmp/hearth-{}", uid())),
    }
}

/// `$HEARTH_SOCKET`, else `$HEARTH_SERVER` (set inside Hearth terminals),
/// else the default per-user socket.
pub fn socket_path() -> PathBuf {
    for var in ["HEARTH_SOCKET", "HEARTH_SERVER"] {
        if let Some(p) = std::env::var_os(var) {
            if !p.is_empty() {
                return PathBuf::from(p);
            }
        }
    }
    runtime_dir().join("server.sock")
}

pub fn config_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("HEARTH_CONFIG_DIR") {
        return PathBuf::from(d);
    }
    env_dir("XDG_CONFIG_HOME", ".config").join("hearth")
}

pub fn state_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("HEARTH_STATE_DIR") {
        return PathBuf::from(d);
    }
    env_dir("XDG_STATE_HOME", ".local/state").join("hearth")
}

/// Creates (if needed) and validates a directory that only we may use:
/// owned by our uid, not a symlink, mode 0700.
pub fn ensure_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(dir) {
        Ok(m) => {
            if m.file_type().is_symlink() || !m.is_dir() {
                return Err(std::io::Error::other(format!("{} is not a directory", dir.display())));
            }
            if m.uid() != uid() {
                return Err(std::io::Error::other(format!("{} is owned by another user", dir.display())));
            }
            if m.mode() & 0o077 != 0 {
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(dir)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        }
        Err(e) => Err(e),
    }
}

/// Verifies a socket we are about to connect to belongs to us.
pub fn check_socket_owner(path: &std::path::Path) -> std::io::Result<()> {
    let m = std::fs::symlink_metadata(path)?;
    use std::os::unix::fs::FileTypeExt;
    if !m.file_type().is_socket() {
        return Err(std::io::Error::other(format!("{} is not a socket", path.display())));
    }
    if m.uid() != uid() {
        return Err(std::io::Error::other(format!("{} is owned by another user", path.display())));
    }
    Ok(())
}
