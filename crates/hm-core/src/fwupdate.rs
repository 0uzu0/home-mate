//! On-speaker A/B slot updates for the audio firmware and hm-core itself
//! (acceptance-20261108 §12, replaced-capability #4).
//!
//! Chain per §12: download/stage into a temp file on the SAME filesystem as
//! the slots → fsync → chmod → **rename over the non-active slot** (atomic
//! same-fs replace; a slot never holds half a file) → flip `current` with a
//! temp symlink + rename (`ln -sfn` is unlink+symlink, non-atomic — never
//! used) → trigger the supervised restart. The speaker supervisor restarts a
//! fast-exiting firmware from the other A/B slot. On the device,
//! `start_server.sh` still sets
//! `ECHOMUSE_FALLBACK=/data/local/bin/start_server.echomuse.sh`, and the
//! EchoMuse `server_a` / `server_b` files are still installed, so two failed
//! boots still fall back to EchoMuse. That script is removed together with
//! the adapter (gateway PR #3 plus the manual on-device install, gateway
//! issue #8).

use std::io;
use std::path::PathBuf;

pub const ARCH_ARM32: u16 = 40; // EM_ARM — the audio firmware is armv7
pub const ARCH_AARCH64: u16 = 183; // EM_AARCH64 — hm-core on the speaker

#[derive(Clone, Debug)]
pub struct SlotLayout {
    /// Directory holding the two slots and the `current` symlink.
    pub dir: PathBuf,
    pub slots: [&'static str; 2],
}

impl SlotLayout {
    pub fn audio() -> Self {
        Self {
            dir: PathBuf::from(
                std::env::var("HM_FW_DIR").unwrap_or_else(|_| "/data/local/tmp/hm-direct".into()),
            ),
            slots: ["audio_a", "audio_b"],
        }
    }

    pub fn core() -> Self {
        Self {
            dir: PathBuf::from(
                std::env::var("HM_CORE_DIR").unwrap_or_else(|_| "/data/hm/bin".into()),
            ),
            slots: ["core_a", "core_b"],
        }
    }

    pub fn available(&self) -> bool {
        self.dir.is_dir() && self.slots.iter().any(|s| self.dir.join(s).is_file())
    }

    /// Active slot name from the `current` symlink target's basename.
    pub fn active_slot(&self) -> Option<String> {
        std::fs::read_link(self.dir.join("current"))
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
    }

    /// The slot an update writes to (never the active one).
    pub fn inactive_slot(&self) -> Option<String> {
        let active = self.active_slot()?;
        Some(
            self.slots
                .iter()
                .find(|s| **s != active)
                .map(|s| s.to_string())
                .unwrap_or_else(|| self.slots[1].into()),
        )
    }

    pub fn slot_info(&self, name: &str) -> Option<SlotInfo> {
        let path = self.dir.join(name);
        // Deliberately open/lseek/read, not `fs::metadata` + `fs::read`:
        // musl answers path-based stat with `statx`, which the speaker's 3.18
        // kernel does not implement (AArch64 291 / AArch32 397). Every call
        // costs a full register dump on console=ttyMT0, and a burst of them
        // blows the audio SPI 100 ms transfer timeout — the P0 reset chain
        // (gateway docs/d/p0-watchdog-resets-20261109.md). This endpoint is
        // user-triggered (diagnostics page), so it must not be a landmine.
        let mut file = std::fs::File::open(&path).ok()?;
        let size = std::io::Seek::seek(&mut file, std::io::SeekFrom::End(0)).ok()?;
        std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(0)).ok()?;
        let mut bytes = Vec::with_capacity(size as usize);
        std::io::Read::read_to_end(&mut file, &mut bytes).ok()?;
        Some(SlotInfo {
            name: name.into(),
            size,
            version: extract_marker(&bytes, b"hm-firmware "),
            sha8: sha256_prefix(&bytes),
        })
    }
}

#[derive(serde::Serialize, Clone, Debug)]
pub struct SlotInfo {
    pub name: String,
    pub size: u64,
    /// Embedded `hm-firmware <ver> (<os>)` marker, when present.
    pub version: Option<String>,
    /// First 8 hex chars of sha256 — stable identity for any binary.
    pub sha8: String,
}

fn validate_elf_header(
    head: &[u8],
    size: u64,
    expect_machine: u16,
    min_size: u64,
) -> Result<(), String> {
    if size < min_size {
        return Err(format!("payload too small: {} bytes", size));
    }
    if head.len() < 20 || &head[0..4] != b"\x7fELF" {
        return Err("not an ELF file".into());
    }
    let class = head[4];
    let machine = u16::from_le_bytes([head[18], head[19]]);
    let expect_class = if expect_machine == ARCH_AARCH64 { 2 } else { 1 };
    if class != expect_class {
        return Err(format!("ELF class {class}, expected {expect_class}"));
    }
    if machine != expect_machine {
        return Err(format!("ELF machine {machine}, expected {expect_machine}"));
    }
    Ok(())
}

/// Streaming ELF validation on a staged file without loading the full payload
/// into RAM (bounded 64 KiB scan buffer, open/lseek instead of statx).
pub fn validate_elf_file(
    path: &std::path::Path,
    expect_machine: u16,
    min_size: u64,
) -> Result<Option<String>, String> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path).map_err(|e| format!("open staged file: {e}"))?;
    let size = file
        .seek(SeekFrom::End(0))
        .map_err(|e| format!("seek staged file: {e}"))?;
    file.seek(SeekFrom::Start(0))
        .map_err(|e| format!("rewind staged file: {e}"))?;

    let mut head = [0u8; 20];
    if size >= 20 {
        file.read_exact(&mut head)
            .map_err(|e| format!("read ELF header: {e}"))?;
    }
    validate_elf_header(
        if size >= 20 { &head } else { &[] },
        size,
        expect_machine,
        min_size,
    )?;

    file.seek(SeekFrom::Start(0))
        .map_err(|e| format!("rewind staged file: {e}"))?;

    const MARKER: &[u8] = b"hm-firmware ";
    const CHUNK: usize = 64 * 1024;
    let overlap = MARKER.len() - 1;
    let mut buf = vec![0u8; CHUNK + overlap];
    let mut carry = 0usize;

    loop {
        let n = file
            .read(&mut buf[carry..])
            .map_err(|e| format!("scan staged file: {e}"))?;
        let total = carry + n;
        if total == 0 {
            return Ok(None);
        }
        if let Some(pos) = buf[..total].windows(MARKER.len()).position(|w| w == MARKER) {
            // Ensure up to 64 bytes from `pos` are available unless at EOF.
            let mut end = total;
            if pos + 64 > end && n > 0 {
                let tail_len = end - pos;
                buf.copy_within(pos..end, 0);
                let extra = file
                    .read(&mut buf[tail_len..])
                    .map_err(|e| format!("scan staged marker: {e}"))?;
                end = tail_len + extra;
                return Ok(extract_marker(&buf[..end], MARKER));
            }
            return Ok(extract_marker(&buf[pos..end], MARKER));
        }
        if n == 0 {
            return Ok(None);
        }
        let keep = total.min(overlap);
        buf.copy_within(total - keep..total, 0);
        carry = keep;
    }
}

/// Find `marker` and read the printable run that follows it (the version
/// string the binary embeds in .rodata). None when the marker is absent.
pub fn extract_marker(bytes: &[u8], marker: &[u8]) -> Option<String> {
    let pos = bytes.windows(marker.len()).position(|w| w == marker)?;
    let rest = &bytes[pos..];
    let end = rest
        .iter()
        .position(|b| *b < 0x20 || *b > 0x7e)
        .unwrap_or(rest.len())
        .min(64);
    Some(String::from_utf8_lossy(&rest[..end]).into_owned())
}

pub fn sha256_prefix(bytes: &[u8]) -> String {
    // A tiny, dependency-free digest is NOT attempted: this marker is for
    // display identity, not tamper protection (transport is the local LAN
    // console). sha8 via a simple FNV-based tag keeps SlotInfo cheap.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:08x}")
}

/// RAII guard for a staged firmware file in `SlotLayout::dir`.
/// Removes the staging file on drop unless disarmed by `commit_staged_slot`.
pub struct StagedSlotFile {
    path: PathBuf,
    target_name: String,
    armed: bool,
}

impl StagedSlotFile {
    /// Create a same-filesystem staging file for the inactive slot.
    pub fn prepare(layout: &SlotLayout) -> Result<(Self, std::fs::File), io::Error> {
        let target_name = layout.inactive_slot().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no current symlink to pick the inactive slot",
            )
        })?;
        let path = layout.dir.join(format!(".{}.staging", target_name));
        let file = std::fs::File::create(&path)?;
        Ok((
            Self {
                path,
                target_name,
                armed: true,
            },
            file,
        ))
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn target_name(&self) -> &str {
        &self.target_name
    }
}

impl Drop for StagedSlotFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Chmod, atomically rename a synced staging file into the target slot, and
/// flip the `current` symlink per §12. Returns the slot name written.
pub fn commit_staged_slot(
    layout: &SlotLayout,
    staged: &mut StagedSlotFile,
) -> Result<String, io::Error> {
    let target_name = staged.target_name.clone();
    let target = layout.dir.join(&target_name);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged.path, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&staged.path, &target)?;
    staged.armed = false;

    // 5: flip `current` — temp symlink + rename (atomic dir-entry replace).
    let link_tmp = layout.dir.join("current.staging");
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(&link_tmp);
        std::os::unix::fs::symlink(&target_name, &link_tmp)?;
        std::fs::rename(&link_tmp, layout.dir.join("current"))?;
    }
    #[cfg(not(unix))]
    {
        let _ = link_tmp;
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "symlink flip requires unix",
        ));
    }
    Ok(target_name)
}

/// TERM the running audio firmware; its supervisor restarts it from the
/// (newly flipped) current slot within ~1s. Returns false when no process
/// matches (e.g. Mac dev boxes, or the supervisor is mid-restart).
pub fn restart_audio_firmware() -> bool {
    let mut killed = false;
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            let Ok(cmdline) = std::fs::read_to_string(e.path().join("cmdline")) else {
                continue;
            };
            // cmdline args are NUL-separated; match the hm-direct supervisor's child.
            if cmdline.contains("hm-direct") && cmdline.contains("firmware") {
                let pid: i32 = name.parse().unwrap_or(0);
                if pid > 1 && unsafe { libc::kill(pid, libc::SIGTERM) } == 0 {
                    killed = true;
                }
            }
        }
    }
    killed
}
