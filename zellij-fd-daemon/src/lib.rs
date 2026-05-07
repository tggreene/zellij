//! Client library for talking to the zellij fd daemon.
//!
//! The fd daemon is a tiny process that holds PTY master file descriptors
//! so they survive zellij binary upgrades. It communicates over a Unix
//! domain socket using a dead-simple text protocol + SCM_RIGHTS fd passing.

use nix::sys::socket::{
    connect, recvmsg, sendmsg, socket, AddressFamily, ControlMessage, ControlMessageOwned,
    MsgFlags, SockAddr, SockFlag, SockType,
};
use nix::sys::uio::IoVec;
use std::collections::BTreeMap;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};

/// Where the daemon socket lives for a given session.
pub fn socket_path(session_name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("zellij-fd-daemon-{}.sock", session_name))
}

/// Client handle to a running fd daemon.
pub struct FdDaemonClient {
    sock: RawFd,
}

impl FdDaemonClient {
    /// Connect to an existing daemon. Returns None if daemon isn't running.
    pub fn connect(session_name: &str) -> Option<Self> {
        let path = socket_path(session_name);
        Self::connect_to_path(&path)
    }

    pub fn connect_to_path(path: &Path) -> Option<Self> {
        let sock = socket(
            AddressFamily::Unix,
            SockType::Stream,
            SockFlag::empty(),
            None,
        )
        .ok()?;

        let addr = SockAddr::new_unix(path).ok()?;
        connect(sock, &addr).ok()?;
        Some(FdDaemonClient { sock })
    }

    /// Store an fd in the daemon under the given key.
    /// The daemon receives a dup of the fd via SCM_RIGHTS.
    pub fn store(&self, key: &str, fd: RawFd) -> Result<(), String> {
        let msg = format!("STORE {}\n", key);
        let iov = [IoVec::from_slice(msg.as_bytes())];
        let fds = [fd];
        sendmsg(
            self.sock,
            &iov,
            &[ControlMessage::ScmRights(&fds)],
            MsgFlags::empty(),
            None::<&SockAddr>,
        )
        .map_err(|e| format!("sendmsg failed: {}", e))?;

        self.read_ok_response()
    }

    /// Retrieve an fd from the daemon by key.
    /// The daemon sends the fd back via SCM_RIGHTS.
    pub fn retrieve(&self, key: &str) -> Result<RawFd, String> {
        let msg = format!("RETRIEVE {}\n", key);
        let iov = [IoVec::from_slice(msg.as_bytes())];
        sendmsg(
            self.sock,
            &iov,
            &[],
            MsgFlags::empty(),
            None::<&SockAddr>,
        )
        .map_err(|e| format!("sendmsg failed: {}", e))?;

        // Read response with fd
        let mut buf = [0u8; 256];
        let iov = [IoVec::from_mut_slice(&mut buf)];
        let mut cmsg_buf = nix::cmsg_space!(RawFd);
        let msg = recvmsg(self.sock, &iov, Some(&mut cmsg_buf), MsgFlags::empty())
            .map_err(|e| format!("recvmsg failed: {}", e))?;

        let response = std::str::from_utf8(&buf[..msg.bytes])
            .map_err(|e| format!("invalid utf8: {}", e))?
            .trim();

        if !response.starts_with("OK") {
            return Err(format!("daemon error: {}", response));
        }

        // Extract the fd from control messages
        for cmsg in msg.cmsgs() {
            if let ControlMessageOwned::ScmRights(fds) = cmsg {
                if let Some(&fd) = fds.first() {
                    return Ok(fd);
                }
            }
        }

        Err("no fd received in response".to_string())
    }

    /// List all keys stored in the daemon.
    pub fn list(&self) -> Result<Vec<String>, String> {
        let msg = b"LIST\n";
        let iov = [IoVec::from_slice(msg)];
        sendmsg(
            self.sock,
            &iov,
            &[],
            MsgFlags::empty(),
            None::<&SockAddr>,
        )
        .map_err(|e| format!("sendmsg failed: {}", e))?;

        let mut buf = [0u8; 4096];
        let iov = [IoVec::from_mut_slice(&mut buf)];
        let msg = recvmsg(self.sock, &iov, None, MsgFlags::empty())
            .map_err(|e| format!("recvmsg failed: {}", e))?;

        let response = std::str::from_utf8(&buf[..msg.bytes])
            .map_err(|e| format!("invalid utf8: {}", e))?;

        let mut keys = Vec::new();
        for line in response.lines() {
            if line == "." {
                break;
            }
            keys.push(line.to_string());
        }
        Ok(keys)
    }

    /// Retrieve all stored fds, consuming them from the daemon.
    /// Returns a map of key -> fd.
    pub fn retrieve_all(&self) -> Result<BTreeMap<String, RawFd>, String> {
        let keys = self.list()?;
        let mut result = BTreeMap::new();
        for key in keys {
            match self.retrieve(&key) {
                Ok(fd) => {
                    result.insert(key, fd);
                }
                Err(e) => {
                    log::warn!("Failed to retrieve fd for key {}: {}", key, e);
                }
            }
        }
        Ok(result)
    }

    /// Delete a key from the daemon (closes the fd on daemon side).
    pub fn delete(&self, key: &str) -> Result<(), String> {
        let msg = format!("DELETE {}\n", key);
        let iov = [IoVec::from_slice(msg.as_bytes())];
        sendmsg(
            self.sock,
            &iov,
            &[],
            MsgFlags::empty(),
            None::<&SockAddr>,
        )
        .map_err(|e| format!("sendmsg failed: {}", e))?;

        self.read_ok_response()
    }

    /// Tell the daemon to shut down.
    pub fn quit(&self) -> Result<(), String> {
        let msg = b"QUIT\n";
        let iov = [IoVec::from_slice(msg)];
        sendmsg(
            self.sock,
            &iov,
            &[],
            MsgFlags::empty(),
            None::<&SockAddr>,
        )
        .map_err(|e| format!("sendmsg failed: {}", e))?;
        // Don't wait for response - daemon is exiting
        Ok(())
    }

    fn read_ok_response(&self) -> Result<(), String> {
        let mut buf = [0u8; 256];
        let iov = [IoVec::from_mut_slice(&mut buf)];
        let msg = recvmsg(self.sock, &iov, None, MsgFlags::empty())
            .map_err(|e| format!("recvmsg failed: {}", e))?;

        let response = std::str::from_utf8(&buf[..msg.bytes])
            .map_err(|e| format!("invalid utf8: {}", e))?
            .trim();

        if response == "OK" {
            Ok(())
        } else {
            Err(format!("daemon error: {}", response))
        }
    }
}

impl Drop for FdDaemonClient {
    fn drop(&mut self) {
        let _ = nix::unistd::close(self.sock);
    }
}

/// Find the fd daemon binary. Checks:
/// 1. Next to current_exe() (works when zellij and daemon are in same dir)
/// 2. Next to the canonical (symlink-resolved) current_exe()
/// 3. In PATH
fn find_daemon_binary() -> Result<std::path::PathBuf, String> {
    let daemon_name = "zellij-fd-daemon";

    // Try next to current exe
    if let Ok(exe) = std::env::current_exe() {
        let candidate = exe.parent()
            .map(|p| p.join(daemon_name));
        if let Some(ref path) = candidate {
            if path.exists() {
                return Ok(path.clone());
            }
        }

        // Try canonicalized path (resolves symlinks)
        if let Ok(canonical) = exe.canonicalize() {
            let candidate = canonical.parent()
                .map(|p| p.join(daemon_name));
            if let Some(ref path) = candidate {
                if path.exists() {
                    return Ok(path.clone());
                }
            }
        }
    }

    // Try PATH
    if let Ok(output) = std::process::Command::new("which")
        .arg(daemon_name)
        .output()
    {
        if output.status.success() {
            let path_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let path = std::path::PathBuf::from(&path_str);
            if path.exists() {
                return Ok(path);
            }
        }
    }

    Err(format!("{} not found next to current exe or in PATH", daemon_name))
}

/// Start the fd daemon as a background process for the given session.
/// Returns Ok(()) if daemon started or was already running.
pub fn ensure_daemon_running(session_name: &str) -> Result<(), String> {
    // Check if already running by trying to connect
    if FdDaemonClient::connect(session_name).is_some() {
        return Ok(());
    }

    let sock_path = socket_path(session_name);
    let daemon_exe = find_daemon_binary()?;

    std::process::Command::new(&daemon_exe)
        .arg(sock_path.to_str().unwrap())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to spawn fd daemon: {}", e))?;

    // Wait briefly for it to start
    for _ in 0..50 {
        std::thread::sleep(std::time::Duration::from_millis(20));
        if FdDaemonClient::connect(session_name).is_some() {
            return Ok(());
        }
    }

    Err("fd daemon failed to start within 1 second".to_string())
}
