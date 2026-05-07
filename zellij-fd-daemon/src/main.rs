//! The zellij fd daemon - a tiny process that holds PTY master file
//! descriptors so they survive zellij binary upgrades.
//!
//! Protocol (text over Unix socket + SCM_RIGHTS):
//!   STORE <key>    + fd via SCM_RIGHTS  → OK / ERR ...
//!   RETRIEVE <key>                       → OK + fd via SCM_RIGHTS / ERR ...
//!   LIST                                 → key1\nkey2\n.\n
//!   DELETE <key>                          → OK / ERR ...
//!   QUIT                                 → daemon exits

use nix::poll::{poll, PollFd, PollFlags};
use nix::sys::socket::{
    accept, bind, listen, recvmsg, sendmsg, socket, AddressFamily, ControlMessage,
    ControlMessageOwned, MsgFlags, SockAddr, SockFlag, SockType,
};
use nix::sys::uio::IoVec;
use std::collections::HashMap;
use std::os::unix::io::RawFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// If no client connects within this window we assume the hot reload
/// was aborted (server crashed mid-flight, user killed everything, etc.)
/// and exit so we don't leak PTY masters indefinitely.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

struct Daemon {
    listener: RawFd,
    socket_path: PathBuf,
    fds: HashMap<String, RawFd>,
}

impl Daemon {
    fn new(socket_path: PathBuf) -> Result<Self, String> {
        // Clean up stale socket
        let _ = std::fs::remove_file(&socket_path);

        let listener = socket(
            AddressFamily::Unix,
            SockType::Stream,
            SockFlag::empty(),
            None,
        )
        .map_err(|e| format!("socket(): {}", e))?;

        let addr = SockAddr::new_unix(&socket_path)
            .map_err(|e| format!("SockAddr::new_unix({:?}): {}", socket_path, e))?;

        bind(listener, &addr).map_err(|e| format!("bind(): {}", e))?;
        listen(listener, 5).map_err(|e| format!("listen(): {}", e))?;

        Ok(Daemon {
            listener,
            socket_path,
            fds: HashMap::new(),
        })
    }

    fn run(&mut self) {
        eprintln!(
            "fd-daemon: listening on {:?} (pid {}, idle timeout {}s)",
            self.socket_path,
            std::process::id(),
            IDLE_TIMEOUT.as_secs()
        );

        let mut last_activity = Instant::now();

        loop {
            let elapsed = last_activity.elapsed();
            if elapsed >= IDLE_TIMEOUT {
                eprintln!(
                    "fd-daemon: idle for {}s with {} fds held, exiting to avoid leak",
                    elapsed.as_secs(),
                    self.fds.len()
                );
                break;
            }
            let remaining = IDLE_TIMEOUT - elapsed;
            // poll(2) takes timeout in millis; cap at i32::MAX
            let timeout_ms = remaining.as_millis().min(i32::MAX as u128) as i32;

            let mut fds = [PollFd::new(self.listener, PollFlags::POLLIN)];
            let ready = match poll(&mut fds, timeout_ms) {
                Ok(n) => n,
                Err(nix::errno::Errno::EINTR) => continue,
                Err(e) => {
                    eprintln!("fd-daemon: poll error: {}", e);
                    break;
                },
            };
            if ready == 0 {
                // timeout — loop will check IDLE_TIMEOUT at top
                continue;
            }

            let client = match accept(self.listener) {
                Ok(fd) => fd,
                Err(e) => {
                    eprintln!("fd-daemon: accept error: {}", e);
                    continue;
                },
            };

            last_activity = Instant::now();

            if self.handle_client(client) {
                // QUIT received
                break;
            }
        }

        self.cleanup();
    }

    /// Handle a single client connection. Returns true if daemon should exit.
    fn handle_client(&mut self, client: RawFd) -> bool {
        let mut should_quit = false;

        loop {
            let mut buf = [0u8; 4096];
            let iov = [IoVec::from_mut_slice(&mut buf)];
            let mut cmsg_buf = nix::cmsg_space!([RawFd; 1]);

            let msg = match recvmsg(client, &iov, Some(&mut cmsg_buf), MsgFlags::empty()) {
                Ok(msg) => msg,
                Err(_) => break,
            };

            if msg.bytes == 0 {
                break; // Client disconnected
            }

            // Extract any passed fds
            let mut received_fds = Vec::new();
            for cmsg in msg.cmsgs() {
                if let ControlMessageOwned::ScmRights(fds) = cmsg {
                    received_fds.extend_from_slice(&fds);
                }
            }

            let request = match std::str::from_utf8(&buf[..msg.bytes]) {
                Ok(s) => s.trim().to_string(),
                Err(_) => {
                    Self::send_response(client, "ERR invalid utf8");
                    continue;
                }
            };

            // Handle multiple commands in one read (shouldn't happen but be safe)
            for line in request.lines() {
                let parts: Vec<&str> = line.splitn(2, ' ').collect();
                let cmd = parts[0];
                let arg = parts.get(1).map(|s| s.trim());

                match cmd {
                    "STORE" => {
                        if let Some(key) = arg {
                            if let Some(&fd) = received_fds.first() {
                                eprintln!("fd-daemon: STORE {} (fd={})", key, fd);
                                // If we already have this key, close the old fd
                                if let Some(old_fd) = self.fds.insert(key.to_string(), fd) {
                                    let _ = nix::unistd::close(old_fd);
                                }
                                Self::send_response(client, "OK");
                            } else {
                                Self::send_response(client, "ERR no fd received");
                            }
                        } else {
                            Self::send_response(client, "ERR missing key");
                        }
                    }

                    "RETRIEVE" => {
                        if let Some(key) = arg {
                            if let Some(&fd) = self.fds.get(key) {
                                eprintln!("fd-daemon: RETRIEVE {} (fd={})", key, fd);
                                // Send fd via SCM_RIGHTS - receiver gets a dup, we keep ours
                                let resp = b"OK\n";
                                let iov = [IoVec::from_slice(resp)];
                                let fds_to_send = [fd];
                                let _ = sendmsg(
                                    client,
                                    &iov,
                                    &[ControlMessage::ScmRights(&fds_to_send)],
                                    MsgFlags::empty(),
                                    None::<&SockAddr>,
                                );
                                // Daemon keeps its copy - it's the permanent owner
                            } else {
                                Self::send_response(client, "ERR key not found");
                            }
                        } else {
                            Self::send_response(client, "ERR missing key");
                        }
                    }

                    "LIST" => {
                        let mut response = String::new();
                        // Sort keys for deterministic ordering
                        let mut keys: Vec<&String> = self.fds.keys().collect();
                        keys.sort();
                        for key in keys {
                            response.push_str(key);
                            response.push('\n');
                        }
                        response.push_str(".\n");
                        Self::send_response(client, &response);
                    }

                    "DELETE" => {
                        if let Some(key) = arg {
                            if let Some(fd) = self.fds.remove(key) {
                                eprintln!("fd-daemon: DELETE {} (fd={})", key, fd);
                                let _ = nix::unistd::close(fd);
                                Self::send_response(client, "OK");
                            } else {
                                Self::send_response(client, "ERR key not found");
                            }
                        } else {
                            Self::send_response(client, "ERR missing key");
                        }
                    }

                    "QUIT" => {
                        eprintln!("fd-daemon: QUIT received, shutting down");
                        Self::send_response(client, "OK");
                        should_quit = true;
                    }

                    "PING" => {
                        Self::send_response(client, "PONG");
                    }

                    _ => {
                        Self::send_response(client, &format!("ERR unknown command: {}", cmd));
                    }
                }
            }

            if should_quit {
                break;
            }
        }

        let _ = nix::unistd::close(client);
        should_quit
    }

    fn send_response(client: RawFd, msg: &str) {
        let data = if msg.ends_with('\n') {
            msg.to_string()
        } else {
            format!("{}\n", msg)
        };
        let iov = [IoVec::from_slice(data.as_bytes())];
        let _ = sendmsg(
            client,
            &iov,
            &[],
            MsgFlags::empty(),
            None::<&SockAddr>,
        );
    }

    fn cleanup(&mut self) {
        // Close all held fds
        for (key, fd) in self.fds.drain() {
            eprintln!("fd-daemon: closing fd {} (key={})", fd, key);
            let _ = nix::unistd::close(fd);
        }
        let _ = nix::unistd::close(self.listener);
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn daemonize() {
    // Double-fork to detach from parent
    unsafe {
        let pid = libc::fork();
        if pid < 0 {
            eprintln!("fd-daemon: first fork failed");
            std::process::exit(1);
        }
        if pid > 0 {
            // Parent exits
            std::process::exit(0);
        }
        // Child continues - create new session
        libc::setsid();

        let pid = libc::fork();
        if pid < 0 {
            eprintln!("fd-daemon: second fork failed");
            std::process::exit(1);
        }
        if pid > 0 {
            // First child exits
            std::process::exit(0);
        }
        // Grandchild continues as daemon
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        eprintln!("Usage: zellij-fd-daemon <socket-path> [--foreground]");
        std::process::exit(1);
    }

    let socket_path = PathBuf::from(&args[1]);
    let foreground = args.get(2).map(|s| s == "--foreground").unwrap_or(false);

    if !foreground {
        daemonize();
    }

    let mut daemon = match Daemon::new(socket_path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("fd-daemon: failed to start: {}", e);
            std::process::exit(1);
        }
    };

    daemon.run();
}
