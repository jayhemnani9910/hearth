//! PTY creation and child spawn with a proper controlling terminal.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShellMode {
    /// Execute argv directly, no shell.
    None,
    /// `$SHELL -l` (login + interactive).
    Login,
    /// `$SHELL -i`.
    Interactive,
}

#[derive(Debug, Clone)]
pub struct SpawnSpec {
    /// Empty argv means "the user's shell".
    pub argv: Vec<String>,
    pub shell: ShellMode,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub cols: u16,
    pub rows: u16,
}

pub struct Spawned {
    pub master: OwnedFd,
    pub pid: i32,
}

pub fn user_shell() -> String {
    if let Ok(s) = std::env::var("SHELL") {
        if !s.is_empty() {
            return s;
        }
    }
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if !pw.is_null() && !(*pw).pw_shell.is_null() {
            return std::ffi::CStr::from_ptr((*pw).pw_shell).to_string_lossy().into_owned();
        }
    }
    "/bin/sh".into()
}

pub fn set_size(fd: i32, cols: u16, rows: u16, px_w: u16, px_h: u16) -> io::Result<()> {
    let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: px_w, ws_ypixel: px_h };
    if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Foreground process group of the terminal, used for process metadata.
pub fn foreground_pgrp(fd: i32) -> Option<i32> {
    let p = unsafe { libc::tcgetpgrp(fd) };
    (p > 0).then_some(p)
}

pub fn spawn(spec: &SpawnSpec) -> io::Result<Spawned> {
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let ws = libc::winsize { ws_row: spec.rows, ws_col: spec.cols, ws_xpixel: 0, ws_ypixel: 0 };
    if unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null(), &ws) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };
    for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    // Enable UTF-8 input processing on the line discipline.
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(slave.as_raw_fd(), &mut t) == 0 {
            t.c_iflag |= libc::IUTF8;
            libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &t);
        }
    }

    let shell = user_shell();
    let mut cmd = if spec.argv.is_empty() {
        let mut c = Command::new(&shell);
        let base = std::path::Path::new(&shell).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or("sh".into());
        match spec.shell {
            // Login shell by the classic argv[0] convention.
            ShellMode::Login | ShellMode::None => {
                c.arg0(format!("-{base}"));
            }
            ShellMode::Interactive => {
                c.arg("-i");
            }
        }
        c
    } else {
        match spec.shell {
            ShellMode::None => {
                let mut c = Command::new(&spec.argv[0]);
                c.args(&spec.argv[1..]);
                c
            }
            ShellMode::Login | ShellMode::Interactive => {
                let mut c = Command::new(&shell);
                c.arg(if spec.shell == ShellMode::Login { "-lc" } else { "-ic" });
                c.arg(shell_join(&spec.argv));
                c
            }
        }
    };
    let dup = |fd: &OwnedFd| -> io::Result<Stdio> { Ok(Stdio::from(File::from(fd.try_clone()?))) };
    cmd.stdin(dup(&slave)?).stdout(dup(&slave)?).stderr(dup(&slave)?);
    if let Some(cwd) = &spec.cwd {
        if cwd.is_dir() {
            cmd.current_dir(cwd);
        } else {
            cmd.current_dir(hearth_core::paths::home());
        }
    } else {
        cmd.current_dir(hearth_core::paths::home());
    }
    // Strip variables that belong to the daemon's own environment.
    for k in ["HEARTH_SOCKET_FD", "LISTEN_FDS", "LISTEN_PID", "LISTEN_FDNAMES", "NOTIFY_SOCKET", "INVOCATION_ID"] {
        cmd.env_remove(k);
    }
    cmd.env("TERM", "xterm-256color").env("COLORTERM", "truecolor").env("TERM_PROGRAM", "hearth");
    for (k, v) in &spec.env {
        cmd.env(OsString::from(k), OsString::from(v));
    }
    unsafe {
        // Only async-signal-safe calls between fork and exec.
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            // Restore default signal dispositions the daemon may have changed.
            for sig in [libc::SIGPIPE, libc::SIGCHLD, libc::SIGHUP, libc::SIGINT, libc::SIGTERM, libc::SIGQUIT] {
                libc::signal(sig, libc::SIG_DFL);
            }
            let empty: libc::sigset_t = std::mem::zeroed();
            libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    drop(slave);
    let pid = child.id() as i32;
    // We reap with waitpid ourselves; forget the std handle.
    std::mem::forget(child);
    Ok(Spawned { master, pid })
}

/// POSIX shell quoting for `-c` strings.
pub fn shell_join(argv: &[String]) -> String {
    if argv.len() == 1 {
        // A single argument is taken as a shell command line.
        return argv[0].clone();
    }
    argv.iter()
        .map(|a| {
            if !a.is_empty() && a.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_./=:,@%+".contains(&b)) {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Blocking wait for a child; returns (exit code, signal).
pub fn wait(pid: i32) -> (Option<i32>, Option<i32>) {
    let mut status = 0;
    loop {
        let r = unsafe { libc::waitpid(pid, &mut status, 0) };
        if r == pid {
            break;
        }
        if r < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return (None, None);
        }
    }
    if libc::WIFEXITED(status) {
        (Some(libc::WEXITSTATUS(status)), None)
    } else if libc::WIFSIGNALED(status) {
        (None, Some(libc::WTERMSIG(status)))
    } else {
        (None, None)
    }
}

pub fn proc_cwd(pid: i32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok().map(|p| p.to_string_lossy().into_owned())
}

pub fn proc_name(pid: i32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).ok().map(|s| s.trim().to_string())
}

/// Process start time in clock ticks since boot (field 22 of /proc/pid/stat),
/// used together with the PID to prove a process is the same one.
pub fn proc_start_time(pid: i32) -> Option<u64> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = &s[s.rfind(')')? + 2..];
    after.split_whitespace().nth(19)?.parse().ok()
}
