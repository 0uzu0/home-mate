//! CPU affinity for the on-speaker deployment (D/CPU task, 2026-11-10).
//!
//! dot2-d's MTK HPS keeps exactly one core online at steady state; the
//! supervisor stops HPS, onlines cpu1–3, and hands this process cpu2 so the
//! audio firmware owns cpu1 and cpu0 keeps kernel/network work. Pinning is
//! opt-in through `HM_CORE_CPU` — Mac/NAS development keeps the scheduler's
//! choice. Any failure degrades to "not pinned" with a log line; it is never
//! fatal (a scheduler quirk must not take the assistant down).

/// Pin every thread of this process to `cpu`. Returns how many tasks were
/// pinned. The tokio runtime's worker threads already exist when `main()`
/// runs (the `#[tokio::main]` attribute builds the runtime first), so this
/// walks `/proc/self/task`; threads created later inherit the creator mask.
#[cfg(target_os = "linux")]
pub fn pin_process_to_cpu(cpu: usize) -> Result<usize, String> {
    let mut set = [0u8; 128]; // cpu_set_t: 1024 bits, same layout on musl
    if cpu >= set.len() * 8 {
        return Err(format!("cpu{cpu} out of range"));
    }
    set[cpu / 8] |= 1 << (cpu % 8);

    let mut tids: Vec<u32> = std::fs::read_dir("/proc/self/task")
        .map_err(|e| format!("read /proc/self/task: {e}"))?
        .filter_map(|e| e.ok()?.file_name().to_string_lossy().parse().ok())
        .collect();
    if tids.is_empty() {
        tids.push(std::process::id());
    }
    let mut pinned = 0usize;
    let mut last_err: Option<String> = None;
    for tid in tids {
        let rc = unsafe {
            libc::sched_setaffinity(
                tid as libc::pid_t,
                set.len(),
                set.as_ptr() as *const libc::cpu_set_t,
            )
        };
        if rc == 0 {
            pinned += 1;
        } else {
            last_err = Some(format!("tid {tid}: {}", std::io::Error::last_os_error()));
        }
    }
    if pinned == 0 {
        return Err(last_err.unwrap_or_else(|| "no threads pinned".into()));
    }
    Ok(pinned)
}

#[cfg(not(target_os = "linux"))]
pub fn pin_process_to_cpu(_cpu: usize) -> Result<usize, String> {
    Err("cpu affinity is a Linux feature".into())
}

/// Read `var` and pin when set. `None` = unset (no pinning requested);
/// `Some(Err)` = requested but failed (caller logs and continues).
pub fn pin_from_env(var: &str) -> Option<Result<usize, String>> {
    let spec = std::env::var(var).ok()?;
    let spec = spec.trim();
    if spec.is_empty() || spec.eq_ignore_ascii_case("off") || spec == "0s" {
        return None;
    }
    let cpu: usize = match spec.parse() {
        Ok(c) => c,
        Err(_) => return Some(Err(format!("{var}={spec:?} is not a CPU index"))),
    };
    Some(
        pin_process_to_cpu(cpu)
            .map(|n| n)
            .map_err(|e| format!("{var}={cpu}: {e}")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_env_means_no_pinning() {
        std::env::remove_var("HM_TEST_CPU_UNSET");
        assert!(pin_from_env("HM_TEST_CPU_UNSET").is_none());
    }

    #[test]
    fn off_means_no_pinning() {
        std::env::set_var("HM_TEST_CPU_OFF", "off");
        assert!(pin_from_env("HM_TEST_CPU_OFF").is_none());
    }

    #[test]
    fn bad_spec_degrades_to_error_not_panic() {
        std::env::set_var("HM_TEST_CPU_BAD", "cpu-two");
        let got = pin_from_env("HM_TEST_CPU_BAD").expect("set");
        assert!(got.is_err());
    }
}
