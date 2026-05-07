use std::io::{Read, Write};
use std::os::unix::io::FromRawFd;
use std::process::Command;

fn daemon_binary() -> std::path::PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // remove test binary name
    path.pop(); // remove deps/
    path.push("zellij-fd-daemon");
    path
}

fn wait_for_daemon(sock_path: &std::path::Path) -> zellij_fd_daemon::FdDaemonClient {
    for _ in 0..50 {
        if let Some(client) = zellij_fd_daemon::FdDaemonClient::connect_to_path(sock_path) {
            return client;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("daemon didn't start in time");
}

struct DaemonGuard {
    child: Option<std::process::Child>,
}

impl DaemonGuard {
    fn start(sock_path: &std::path::Path) -> Self {
        let child = Command::new(daemon_binary())
            .arg(sock_path)
            .arg("--foreground")
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .expect("failed to start daemon");
        DaemonGuard { child: Some(child) }
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn test_store_and_retrieve_fd() {
    let tmp = tempfile::tempdir().unwrap();
    let sock_path = tmp.path().join("test.sock");
    let _daemon = DaemonGuard::start(&sock_path);

    let client = wait_for_daemon(&sock_path);

    // Create a pipe
    let (read_fd, write_fd) = nix::unistd::pipe().unwrap();

    // Store the write fd in the daemon
    client.store("terminal_0", write_fd).unwrap();

    // Write through our local copy
    unsafe {
        let mut f = std::fs::File::from_raw_fd(write_fd);
        f.write_all(b"hello before").unwrap();
        std::mem::forget(f);
    }

    // List keys
    let keys = client.list().unwrap();
    assert_eq!(keys, vec!["terminal_0"]);

    // Retrieve - gives us a NEW fd number pointing to same pipe
    let retrieved_fd = client.retrieve("terminal_0").unwrap();
    assert_ne!(retrieved_fd, write_fd);

    // Write through the retrieved fd
    unsafe {
        let mut f = std::fs::File::from_raw_fd(retrieved_fd);
        f.write_all(b" and after").unwrap();
        std::mem::forget(f);
    }

    // Close write ends so read gets EOF
    // Daemon also holds a copy, so delete it there too
    client.delete("terminal_0").unwrap();
    let _ = nix::unistd::close(write_fd);
    let _ = nix::unistd::close(retrieved_fd);

    // Read everything from the read end
    let mut f = unsafe { std::fs::File::from_raw_fd(read_fd) };
    let mut buf = String::new();
    f.read_to_string(&mut buf).unwrap();
    assert_eq!(buf, "hello before and after");
}

#[test]
fn test_hot_reload_simulation() {
    // The key test: store fds, disconnect, reconnect, retrieve
    let tmp = tempfile::tempdir().unwrap();
    let sock_path = tmp.path().join("test2.sock");
    let _daemon = DaemonGuard::start(&sock_path);

    // "Old zellij" connects and stores fds
    let (r0, w0) = nix::unistd::pipe().unwrap();
    let (r1, w1) = nix::unistd::pipe().unwrap();

    {
        let old_client = wait_for_daemon(&sock_path);
        old_client.store("terminal_0", w0).unwrap();
        old_client.store("terminal_1", w1).unwrap();
        // old_client drops here - simulating old zellij exiting
    }

    // Brief pause to let daemon process the disconnect
    std::thread::sleep(std::time::Duration::from_millis(50));

    // "New zellij" connects and retrieves
    {
        let new_client = wait_for_daemon(&sock_path);
        let keys = new_client.list().unwrap();
        assert_eq!(keys, vec!["terminal_0", "terminal_1"]);

        let new_w0 = new_client.retrieve("terminal_0").unwrap();
        let new_w1 = new_client.retrieve("terminal_1").unwrap();

        // Write through the retrieved fds
        unsafe {
            let mut f = std::fs::File::from_raw_fd(new_w0);
            f.write_all(b"pane0").unwrap();
            std::mem::forget(f);
            let mut f = std::fs::File::from_raw_fd(new_w1);
            f.write_all(b"pane1").unwrap();
            std::mem::forget(f);
        }

        // Close all write ends including daemon's copies
        new_client.delete("terminal_0").unwrap();
        new_client.delete("terminal_1").unwrap();
        let _ = nix::unistd::close(w0);
        let _ = nix::unistd::close(w1);
        let _ = nix::unistd::close(new_w0);
        let _ = nix::unistd::close(new_w1);

        let mut f0 = unsafe { std::fs::File::from_raw_fd(r0) };
        let mut buf0 = String::new();
        f0.read_to_string(&mut buf0).unwrap();
        assert_eq!(buf0, "pane0");

        let mut f1 = unsafe { std::fs::File::from_raw_fd(r1) };
        let mut buf1 = String::new();
        f1.read_to_string(&mut buf1).unwrap();
        assert_eq!(buf1, "pane1");
    }
}

#[test]
fn test_pty_fd_survives_daemon_transfer() {
    // Simulate what happens with a real PTY: open a pty, store the master fd,
    // retrieve it, and verify read/write still works
    use nix::fcntl::{fcntl, FcntlArg, OFlag};

    let tmp = tempfile::tempdir().unwrap();
    let sock_path = tmp.path().join("pty_test.sock");
    let _daemon = DaemonGuard::start(&sock_path);

    // Open a real PTY and set raw mode
    let pty = nix::pty::openpty(None, None).expect("openpty failed");
    let master_fd = pty.master;
    let slave_fd = pty.slave;

    // Set master to non-blocking so reads don't hang
    let flags = fcntl(master_fd, FcntlArg::F_GETFL).unwrap();
    fcntl(master_fd, FcntlArg::F_SETFL(OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK)).unwrap();

    // Store master fd with daemon (simulating old zellij)
    {
        let client = wait_for_daemon(&sock_path);
        client.store("terminal_0", master_fd).unwrap();
    }

    std::thread::sleep(std::time::Duration::from_millis(50));

    // Retrieve master fd (simulating new zellij)
    let retrieved_master = {
        let client = wait_for_daemon(&sock_path);
        client.retrieve("terminal_0").unwrap()
    };

    // Set retrieved fd to non-blocking too
    let flags = fcntl(retrieved_master, FcntlArg::F_GETFL).unwrap();
    fcntl(retrieved_master, FcntlArg::F_SETFL(OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK)).unwrap();

    // Write to slave with newline (PTY line discipline needs it)
    nix::unistd::write(slave_fd, b"hello\n").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));

    let mut buf = [0u8; 256];
    let n = nix::unistd::read(retrieved_master, &mut buf).expect("read from retrieved master failed");
    assert!(n > 0, "expected data from pty master after transfer");

    // Write to retrieved master (input to terminal)
    nix::unistd::write(retrieved_master, b"input\r").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));

    let n = nix::unistd::read(slave_fd, &mut buf).expect("read from slave failed");
    assert!(n > 0, "expected data on slave after writing to retrieved master");

    // Clean up
    let _ = nix::unistd::close(master_fd);
    let _ = nix::unistd::close(slave_fd);
    let _ = nix::unistd::close(retrieved_master);
}

#[test]
fn test_ensure_daemon_running() {
    let session = "ensure-daemon-test";
    let sock = zellij_fd_daemon::socket_path(session);
    let _ = std::fs::remove_file(&sock);

    // Should start daemon automatically
    zellij_fd_daemon::ensure_daemon_running(session).expect("ensure_daemon_running failed");

    // Should be able to connect and use it
    let client = zellij_fd_daemon::FdDaemonClient::connect(session)
        .expect("failed to connect after ensure_daemon_running");

    let (r, w) = nix::unistd::pipe().unwrap();
    client.store("test_fd", w).unwrap();

    let keys = client.list().unwrap();
    assert_eq!(keys, vec!["test_fd"]);

    // Clean up
    let client2 = zellij_fd_daemon::FdDaemonClient::connect(session).unwrap();
    let _ = client2.quit();
    let _ = nix::unistd::close(r);
    let _ = nix::unistd::close(w);
    let _ = std::fs::remove_file(&sock);
}
