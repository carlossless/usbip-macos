//! Helpers for end-to-end tests that attach devices through the real binary on macOS.

use std::{
    process::{Child, Command, ExitStatus, Stdio},
    sync::{Mutex, MutexGuard},
    time::{Duration, Instant},
};

use nusb::{DeviceInfo, MaybeFuture};

pub const TIMEOUT: Duration = Duration::from_secs(5);

// One virtual controller per client and devices identified by VID/PID: tests must not overlap.
static EXCLUSIVE: Mutex<()> = Mutex::new(());

/// Returns a guard serialising end-to-end tests, or `None` if they are disabled.
pub fn enabled(var: &str) -> Option<MutexGuard<'static, ()>> {
    if std::env::var(var).is_ok_and(|v| !v.is_empty() && v != "0") {
        Some(EXCLUSIVE.lock().unwrap_or_else(|e| e.into_inner()))
    } else {
        eprintln!("skipped: set {var} to run");
        None
    }
}

extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

/// A running `usbip-macos attach`, stopped with SIGTERM when dropped.
pub struct Client(Child);

impl Client {
    pub fn attach(address: &str, busid: &str) -> Self {
        let (host, port) = address.rsplit_once(':').expect("host:port");
        Self(
            Command::new(env!("CARGO_BIN_EXE_usbip-macos"))
                .args(["-r", host, "-p", port, "attach", "--busid", busid])
                .env(
                    "RUST_LOG",
                    std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into()),
                )
                .stdin(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }

    pub fn try_wait(&mut self) -> Option<ExitStatus> {
        self.0.try_wait().unwrap()
    }

    pub fn terminate(&mut self) -> ExitStatus {
        unsafe { kill(self.0.id() as i32, 15) };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.try_wait() {
                return status;
            }
            assert!(Instant::now() < deadline, "client did not exit on SIGTERM");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Waits for the attached device to enumerate.
    pub fn device(&mut self, vid: u16, pid: u16) -> DeviceInfo {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(info) = find_device(vid, pid) {
                return info;
            }
            if let Some(status) = self.try_wait() {
                panic!("client exited early with {status}");
            }
            assert!(
                Instant::now() < deadline,
                "device {vid:04x}:{pid:04x} never enumerated"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if self.try_wait().is_none() {
            unsafe { kill(self.0.id() as i32, 15) };
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.0.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

pub fn find_device(vid: u16, pid: u16) -> Option<DeviceInfo> {
    nusb::list_devices()
        .wait()
        .ok()?
        .find(|d| d.vendor_id() == vid && d.product_id() == pid)
}

pub fn wait_for(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub fn wait_gone(vid: u16, pid: u16) {
    wait_for("device to go away", Duration::from_secs(10), || {
        find_device(vid, pid).is_none()
    });
}

/// Runs a shell command, returning stdout; panics with stderr on failure.
pub fn sh(cmd: &str) -> String {
    let out = Command::new("sh").arg("-c").arg(cmd).output().unwrap();
    assert!(
        out.status.success(),
        "{cmd}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Compiles `tests/macos/<name>.swift` once per test run and returns the binary.
pub fn swift_probe(name: &str) -> std::path::PathBuf {
    static BUILT: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let out = root.join("target").join("swift").join(name);
    let mut built = BUILT.lock().unwrap_or_else(|e| e.into_inner());
    if !built.iter().any(|b| b == name) {
        std::fs::create_dir_all(out.parent().unwrap()).unwrap();
        let src = root
            .join("tests")
            .join("macos")
            .join(format!("{name}.swift"));
        let status = Command::new("swiftc")
            .args(["-O", "-o"])
            .arg(&out)
            .arg(&src)
            .status()
            .expect("swiftc");
        assert!(status.success(), "compiling {name}.swift");
        built.push(name.into());
    }
    out
}

/// Runs a command with a timeout, returning (success, stdout).
pub fn run_for(timeout: Duration, cmd: &mut Command) -> (bool, String) {
    let mut child = cmd.stdout(Stdio::piped()).spawn().unwrap();
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let mut out = String::new();
            std::io::Read::read_to_string(child.stdout.as_mut().unwrap(), &mut out).unwrap();
            return (status.success(), out);
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return (false, "timed out".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
