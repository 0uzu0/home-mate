//! Device-side primitives: roots, A/B slots, atomic pointer flips, process
//! restarts, and the management-API facts the activation preflight needs.
//!
//! Nothing here is generic "deployment tooling": every path is the one the
//! validated supervisors already use (`/data/hm/bin/current`,
//! `/data/local/tmp/hm-direct/current`), and the write sequence is the same
//! staging + fsync + rename + chmod that `fwupdate::write_slot` proved on the
//! speaker.

use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::http;
use crate::manifest::marker_version;

pub const CORE_SLOTS: [&str; 2] = ["core_a", "core_b"];
pub const AUDIO_SLOTS: [&str; 2] = ["audio_a", "audio_b"];

#[derive(Clone, Debug)]
pub struct Paths {
    /// `/data/hm`
    pub root: PathBuf,
    /// `/data/local/tmp/hm-direct`
    pub fw_root: PathBuf,
    /// `http://127.0.0.1:8930`
    pub core_url: String,
}

impl Paths {
    pub fn from_env() -> Self {
        Self {
            root: PathBuf::from(
                std::env::var("HM_RELEASE_ROOT").unwrap_or_else(|_| "/data/hm".into()),
            ),
            fw_root: PathBuf::from(
                std::env::var("HM_FW_DIR").unwrap_or_else(|_| "/data/local/tmp/hm-direct".into()),
            ),
            core_url: std::env::var("HM_RELEASE_CORE_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:8930".into())
                .trim_end_matches('/')
                .to_string(),
        }
    }

    /// A device root without environment coupling (host integration tests).
    pub fn for_test(root: PathBuf, fw_root: PathBuf) -> Self {
        Self {
            root,
            fw_root,
            core_url: "http://127.0.0.1:9".into(),
        }
    }

    pub fn releases(&self) -> PathBuf {
        self.root.join("releases")
    }
    pub fn state_path(&self) -> PathBuf {
        self.releases().join("state.json")
    }
    pub fn log_path(&self) -> PathBuf {
        self.releases().join("installer.log")
    }
    pub fn lock_path(&self) -> PathBuf {
        self.releases().join(".lock")
    }
    pub fn core_bin_dir(&self) -> PathBuf {
        self.root.join("bin")
    }
    pub fn web(&self) -> PathBuf {
        self.root.join("web")
    }
    pub fn data_dir(&self) -> PathBuf {
        self.root.join("data")
    }
    /// The keyword-spotter runtime dir the firmware spawns
    /// (`<fw_root>/../hm-kws` = `/data/local/tmp/hm-kws`). A release installs
    /// the bundle here by hard-linking into the verified release dir; `kw.txt`
    /// inside it is NODE configuration and is never touched.
    pub fn kws_dir(&self) -> PathBuf {
        if let Ok(v) = std::env::var("HM_KWS_DIR") {
            return PathBuf::from(v);
        }
        match self.fw_root.parent() {
            Some(p) => p.join("hm-kws"),
            None => PathBuf::from("/data/local/tmp/hm-kws"),
        }
    }

    pub fn model_path(&self) -> PathBuf {
        self.data_dir().join("models").join("silero_vad.onnx")
    }
    pub fn schema_sidecar(&self) -> PathBuf {
        self.data_dir().join("home-mate.schema")
    }
    pub fn snapshots(&self) -> PathBuf {
        self.data_dir().join("db-snapshots")
    }
    pub fn local_token_path(&self) -> PathBuf {
        self.data_dir().join("local-release.token")
    }
    /// Read the machine-local release token shared with `hm-core` (#231).
    /// Attached only to management calls against `self.core_url`, never to
    /// external LAN package downloads.
    pub fn local_token(&self) -> Option<String> {
        if let Ok(env_tok) = std::env::var("HM_RELEASE_TOKEN") {
            let trimmed = env_tok.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        std::fs::read_to_string(self.local_token_path())
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    pub fn ensure_dirs(&self) -> Result<(), String> {
        for d in [
            self.releases(),
            self.core_bin_dir(),
            self.data_dir(),
            self.data_dir().join("models"),
            self.snapshots(),
        ] {
            std::fs::create_dir_all(&d).map_err(|e| format!("mkdir {}: {e}", d.display()))?;
        }
        Ok(())
    }

    pub fn api(&self, path: &str) -> String {
        format!("{}{}", self.core_url, path)
    }

    pub fn core_request(
        &self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<http::Response, String> {
        let token = self.local_token();
        http::request_with_token(method, &self.api(path), body, token.as_deref(), timeout)
    }

    pub fn core_get_json(
        &self,
        path: &str,
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        let token = self.local_token();
        http::get_json_with_token(&self.api(path), token.as_deref(), timeout)
    }

    pub fn core_post_json(
        &self,
        path: &str,
        value: &serde_json::Value,
        timeout: Duration,
    ) -> Result<http::Response, String> {
        let token = self.local_token();
        http::post_json_with_token(&self.api(path), value, token.as_deref(), timeout)
    }

    pub fn status(&self) -> Result<serde_json::Value, String> {
        self.core_get_json("/api/v0/status", Duration::from_secs(3))
    }

    pub fn mode(&self) -> Result<serde_json::Value, String> {
        self.core_get_json("/api/v0/mode", Duration::from_secs(3))
    }

    pub fn facts(&self) -> Result<DeviceFacts, String> {
        // The device identity (model + firmware marker) is reported by the
        // gateway hello and appears in `/status.gateway.devices[]`; `/mode`
        // carries the behavior state (kws/vad) but not the descriptor.
        let status = self.status()?;
        let devs = status
            .get("gateway")
            .and_then(|g| g.get("devices"))
            .and_then(|d| d.as_array())
            .ok_or("no device in /status.gateway.devices (gateway not connected?)")?;
        // Prefer the local loopback / hm-firmware speaker gateway when multiple
        // gateways (e.g. an Android companion client) are connected concurrently.
        let gw_dev = devs
            .iter()
            .find(|d| {
                d.get("loopback").and_then(|v| v.as_bool()).unwrap_or(false)
                    || d.get("firmware")
                        .and_then(|v| v.as_str())
                        .is_some_and(|fw| fw.contains("hm-firmware "))
            })
            .or_else(|| devs.first())
            .ok_or("no device in /status.gateway.devices (gateway not connected?)")?;
        let firmware = gw_dev
            .get("firmware")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let model = gw_dev
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let firmware_version = marker_version(&firmware, "hm-firmware ").unwrap_or_default();
        Ok(DeviceFacts {
            model,
            firmware,
            firmware_version,
            profile: os_profile(),
        })
    }

    /// The schema the last core applied, read from the sidecar it writes right
    /// after migrations. No SQLite dependency in this binary.
    pub fn schema_sidecar_value(&self) -> Option<i64> {
        let raw = std::fs::read_to_string(self.schema_sidecar()).ok()?;
        let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
        v.get("schema").and_then(|s| s.as_i64())
    }
}

#[derive(Clone, Debug)]
pub struct DeviceFacts {
    pub model: String,
    pub firmware: String,
    pub firmware_version: String,
    pub profile: String,
}

/// `dot2d-emos` on the speaker; the raw distro id otherwise (preflight will
/// reject a package whose target profile does not match).
pub fn os_profile() -> String {
    if let Ok(p) = std::env::var("HM_RELEASE_PROFILE") {
        if !p.trim().is_empty() {
            return p.trim().to_string();
        }
    }
    if let Ok(text) = std::fs::read_to_string("/etc/os-release") {
        for line in text.lines() {
            if let Some(id) = line.strip_prefix("ID=") {
                let id = id.trim().trim_matches('"');
                return if id.eq_ignore_ascii_case("emos") {
                    "dot2d-emos".into()
                } else {
                    id.to_string()
                };
            }
        }
    }
    "unknown".into()
}

// --- slots -------------------------------------------------------------------

pub fn read_current(dir: &Path) -> Option<String> {
    std::fs::read_link(dir.join("current"))
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
}

pub fn inactive_slot(dir: &Path, slots: [&str; 2]) -> Option<String> {
    let active = read_current(dir)?;
    slots.iter().find(|s| **s != active).map(|s| s.to_string())
}

/// Copy `src` into `dir/<slot>` through a same-directory staging file, fsync,
/// chmod 0755, atomic rename. Returns the sha256 of what actually landed.
pub fn write_slot_from_file(dir: &Path, slot: &str, src: &Path) -> Result<String, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    let staging = dir.join(format!(".{slot}.staging-{}", std::process::id()));
    // A leftover staging hardlink would share the SOURCE inode; unlink first
    // or a fallback copy would truncate the release file itself.
    let _ = std::fs::remove_file(&staging);

    // Slots and release directories live on the same filesystem (/data).
    // Hard-linking makes the slot an additional name for the verified release
    // file: no second 16 MB copy per release, and the re-hash below is exact.
    // The link and the release file share an inode, so the exec bit is set on
    // the SOURCE (mode only; content and hashes are untouched).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(src, std::fs::Permissions::from_mode(0o755));
    }
    let linked = std::fs::hard_link(src, &staging).is_ok();
    let expected = if linked {
        sha256_file(src)?
    } else {
        let mut hasher = Sha256::new();
        {
            let mut input =
                std::fs::File::open(src).map_err(|e| format!("open {}: {e}", src.display()))?;
            let mut out = std::fs::File::create(&staging)
                .map_err(|e| format!("create {}: {e}", staging.display()))?;
            let mut buf = [0u8; 64 * 1024];
            loop {
                let n = input
                    .read(&mut buf)
                    .map_err(|e| format!("read {}: {e}", src.display()))?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                out.write_all(&buf[..n])
                    .map_err(|e| format!("write {}: {e}", staging.display()))?;
            }
            out.sync_all()
                .map_err(|e| format!("sync {}: {e}", staging.display()))?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755))
                .map_err(|e| format!("chmod {}: {e}", staging.display()))?;
        }
        crate::hexutil::to_hex(&hasher.finalize())
    };
    std::fs::rename(&staging, dir.join(slot)).map_err(|e| format!("rename into {slot}: {e}"))?;
    // Re-hash the slot so "what landed" is measured, not assumed.
    let landed = sha256_file(&dir.join(slot))?;
    if landed != expected {
        return Err(format!(
            "{slot}: written bytes do not hash to what the source did"
        ));
    }
    Ok(landed)
}

/// Atomic symlink replace (temp link + rename). `target` is stored verbatim.
pub fn flip_symlink(link: &Path, target: &Path) -> Result<(), String> {
    let dir = link
        .parent()
        .ok_or_else(|| format!("{} has no parent", link.display()))?;
    let tmp = dir.join(format!(".link.tmp-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, &tmp)
            .map_err(|e| format!("symlink {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, link).map_err(|e| format!("rename link {}: {e}", link.display()))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = target;
        Err("symlink flip requires unix".into())
    }
}

pub fn sha256_file(path: &Path) -> Result<String, String> {
    crate::crypto::sha256_file(path)
}

/// Free bytes on the filesystem holding `path` (None when the platform cannot
/// answer). Used before staging a package and when deciding how many old
/// releases to keep: /data on the speaker is ~1 GB shared with everything else.
pub fn free_bytes(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        unsafe {
            let mut st: libc::statvfs = std::mem::zeroed();
            if libc::statvfs(c.as_ptr(), &mut st) == 0 {
                // f_bfree (not f_bavail): the installer runs as root and may
                // use the reserved blocks; on this /data that is ~25 MB that
                // df shows but f_bavail hides.
                let blocks = (st.f_bfree as u64).max(st.f_bavail as u64);
                return Some(blocks.saturating_mul(st.f_frsize as u64));
            }
        }
        let _ = path;
        None
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

// --- processes ---------------------------------------------------------------

fn read_cmdline(pid: i32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(
        raw.split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect(),
    )
}

pub fn find_pids(mut pred: impl FnMut(&[String]) -> bool) -> Vec<i32> {
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(pid) = name.parse::<i32>() else {
            continue;
        };
        if pid <= 1 {
            continue;
        }
        if let Some(args) = read_cmdline(pid) {
            if pred(&args) {
                out.push(pid);
            }
        }
    }
    out
}

fn sigterm(pid: i32) -> bool {
    unsafe { libc::kill(pid, libc::SIGTERM) == 0 }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartMethod {
    /// The process answered the management endpoint and will exit 0 itself.
    Http,
    /// Fallback: SIGTERM (the supervisor treats the exit per its own rules).
    Signal,
}

impl Paths {
    /// Restart the core. Prefers its own graceful restart endpoint (exit 0 →
    /// supervisor respawns in ~2 s without counting a fast-exit attempt).
    pub fn core_pids(&self) -> Vec<i32> {
        let current = self.core_bin_dir().join("current");
        find_pids(|args| {
            args.first().map_or(false, |a| {
                Path::new(a) == current || a.ends_with("/bin/current")
            })
        })
    }

    pub fn firmware_pids(&self) -> Vec<i32> {
        let fw_root = self.fw_root.display().to_string();
        find_pids(|args| {
            args.first().map_or(false, |a| a.starts_with(&fw_root))
                && args.get(1).map_or(false, |a| a == "firmware")
        })
    }

    pub fn restart_core(&self) -> Result<RestartMethod, String> {
        if let Ok(r) = self.core_request(
            "POST",
            "/api/v0/system/restart",
            None,
            Duration::from_secs(3),
        ) {
            if r.status == 200 {
                return Ok(RestartMethod::Http);
            }
        }
        let current = self.core_bin_dir().join("current");
        let pids = find_pids(|args| {
            args.first().map_or(false, |a| {
                Path::new(a) == current || a.ends_with("/bin/current")
            })
        });
        if pids.is_empty() {
            return Err("core is not reachable over HTTP and no core process matched".into());
        }
        for pid in &pids {
            sigterm(*pid);
        }
        Ok(RestartMethod::Signal)
    }

    /// Restart the audio firmware. TERM makes the firmware exit 0; its
    /// supervisor exits on code 0 and init respawns it from the flipped slot
    /// (the same path `POST /firmware/audio?restart=true` has used since M4).
    /// Returns whether a firmware process was actually signalled. On a host
    /// (no audio supervisor) there is none; the health gate then decides, so a
    /// silently-missing restart on the real device still fails activation.
    pub fn restart_firmware(&self) -> Result<bool, String> {
        let fw_root = self.fw_root.display().to_string();
        let pids = find_pids(|args| {
            args.first().map_or(false, |a| a.starts_with(&fw_root))
                && args.get(1).map_or(false, |a| a == "firmware")
        });
        for pid in &pids {
            sigterm(*pid);
        }
        Ok(!pids.is_empty())
    }
}

/// Poll `cond` until it returns true or the deadline passes.
pub fn wait_until(timeout: Duration, every: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(every);
    }
}

// --- files -------------------------------------------------------------------

/// `flock`-based exclusive lock: one installer/activator at a time.
pub struct FileLock {
    file: std::fs::File,
}

impl FileLock {
    pub fn acquire(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| format!("open lock {}: {e}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc != 0 {
                return Err(format!(
                    "another release job is running (lock held: {})",
                    path.display()
                ));
            }
        }
        Ok(Self { file })
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            unsafe {
                libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }
}

/// Minimal ELF identity check: magic, class, machine, minimum size.
pub fn validate_elf(bytes: &[u8], expect_machine: u16, min_size: u64) -> Result<(), String> {
    if (bytes.len() as u64) < min_size {
        return Err(format!("payload too small: {} bytes", bytes.len()));
    }
    if bytes.len() < 20 || &bytes[0..4] != b"\x7fELF" {
        return Err("not an ELF file".into());
    }
    let class = bytes[4];
    let machine = u16::from_le_bytes([bytes[18], bytes[19]]);
    let expect_class = if expect_machine == 183 { 2 } else { 1 };
    if class != expect_class {
        return Err(format!("ELF class {class}, expected {expect_class}"));
    }
    if machine != expect_machine {
        return Err(format!("ELF machine {machine}, expected {expect_machine}"));
    }
    Ok(())
}

/// Find `marker` and read the printable run that follows (the version/build
/// string the build scripts embed in .rodata).
pub fn extract_marker(bytes: &[u8], marker: &[u8]) -> Option<String> {
    let pos = bytes.windows(marker.len()).position(|w| w == marker)?;
    let rest = &bytes[pos..];
    let end = rest
        .iter()
        .position(|b| *b < 0x20 || *b > 0x7e)
        .unwrap_or(rest.len())
        .min(96);
    Some(String::from_utf8_lossy(&rest[..end]).into_owned())
}

/// Append a line to the bounded installer log (1 MiB, trims to 256 KiB).
pub fn log_line(path: &Path, line: &str) {
    let stamp = state::now_ms();
    let line = format!("{stamp} {line}\n");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > 1024 * 1024 {
            if let Ok(bytes) = std::fs::read(path) {
                let keep = bytes.len().saturating_sub(256 * 1024);
                let _ = std::fs::write(path, &bytes[keep..]);
            }
        }
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

use crate::state;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_helpers_pick_the_non_active_slot() {
        let dir = std::env::temp_dir().join(format!("hm-rel-slot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("core_a", dir.join("current")).unwrap();
        assert_eq!(read_current(&dir).as_deref(), Some("core_a"));
        assert_eq!(inactive_slot(&dir, CORE_SLOTS).as_deref(), Some("core_b"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_slot_is_atomic_and_hashes_what_landed() {
        let dir = std::env::temp_dir().join(format!("hm-rel-write-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.bin");
        std::fs::write(&src, b"payload").unwrap();
        let sha = write_slot_from_file(&dir, "core_b", &src).unwrap();
        assert_eq!(sha, crate::crypto::sha256_bytes(b"payload"));
        assert_eq!(std::fs::read(dir.join("core_b")).unwrap(), b"payload");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("core_b"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_slot_is_a_hardlink_to_the_release_file_when_possible() {
        let dir = std::env::temp_dir().join(format!("hm-rel-link-slot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("rel")).unwrap();
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        let src = dir.join("rel/core-aarch64");
        std::fs::write(&src, b"payload").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let sha = write_slot_from_file(&dir.join("bin"), "core_b", &src).unwrap();
        assert_eq!(sha, crate::crypto::sha256_bytes(b"payload"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let a = std::fs::metadata(&src).unwrap();
            let b = std::fs::metadata(dir.join("bin/core_b")).unwrap();
            assert_eq!(
                a.ino(),
                b.ino(),
                "slot should share the release file's inode"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn flip_symlink_replaces_in_one_rename() {
        let dir = std::env::temp_dir().join(format!("hm-rel-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let link = dir.join("current");
        flip_symlink(&link, Path::new("releases/1-a")).unwrap();
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            Path::new("releases/1-a")
        );
        flip_symlink(&link, Path::new("releases/2-b")).unwrap();
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            Path::new("releases/2-b")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn elf_validation_accepts_a_tiny_fake_header() {
        let mut elf = vec![0u8; 64 * 1024];
        elf[0..4].copy_from_slice(b"\x7fELF");
        elf[4] = 2;
        elf[18] = 183;
        elf[19] = 0;
        validate_elf(&elf, 183, 1024).unwrap();
        elf[4] = 1;
        validate_elf(&elf, 183, 1024).unwrap_err();
    }
}
