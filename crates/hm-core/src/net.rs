//! Speaker-side network bootstrap pieces (stage C1).
//!
//! Two problems specific to running hm-core on the dot2-d speaker:
//!
//! 1. **DNS**: this network's router does not answer the speaker's UDP/TCP 53
//!    (verified 2026-11-08: ICMP + TCP 80/443 to the internet work, all DNS
//!    fails). HTTPS 443 is open, so resolution goes over DoH to a fixed IP —
//!    no bootstrap name lookup needed. Selected with `HM_RESOLVER=doh`;
//!    default stays `system` (Mac/NAS deployments keep the OS resolver).
//! 2. **Clock**: a cold-booted speaker has no time source until a core sends
//!    `time_set` — but the on-speaker core IS that core, and cloud TLS needs
//!    a sane clock first. `HM_TIME_BOOTSTRAP=1` fetches a `Date` header over
//!    plain HTTP from a fixed IP before anything else starts.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// DoH base URL (IP literal on purpose: no recursive lookup, works when the
/// host resolver is broken). Overridable for tests via `HM_DOH_URL`.
pub const DEFAULT_DOH_URL: &str = "https://223.5.5.5/resolve";

const CACHE_TTL: Duration = Duration::from_secs(60);

/// Attach the configured resolver to a client builder (no-op unless
/// `HM_RESOLVER=doh`). All outbound service clients go through this so the
/// speaker deployment gets DoH everywhere with one env var.
pub fn apply_resolver(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    match std::env::var("HM_RESOLVER").as_deref() {
        Ok("doh") => builder.dns_resolver(std::sync::Arc::new(DohResolver::from_env())),
        _ => builder,
    }
}

/// Resolve hostnames over the AliDNS DoH JSON API, with a small TTL cache.
pub struct DohResolver {
    http: reqwest::Client,
    base_url: String,
    cache: std::sync::Arc<Mutex<HashMap<String, (Vec<SocketAddr>, Instant)>>>,
}

#[derive(serde::Deserialize)]
struct DohAnswer {
    #[serde(default, rename = "type")]
    r#type: u32,
    #[serde(default)]
    data: String,
}

// AliDNS /resolve uses capitalized keys (Status/Answer) — verified against
// the live endpoint 2026-11-08; the mock in tests pins the same shape.
#[derive(serde::Deserialize)]
struct DohResponse {
    #[serde(default, rename = "Status")]
    status: u32,
    #[serde(default, rename = "Answer")]
    answer: Vec<DohAnswer>,
}

impl DohResolver {
    pub fn from_env() -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("reqwest client"),
            base_url: std::env::var("HM_DOH_URL").unwrap_or_else(|_| DEFAULT_DOH_URL.into()),
            cache: std::sync::Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn lookup(
        http: reqwest::Client,
        base_url: String,
        host: String,
    ) -> Result<Vec<SocketAddr>, String> {
        let resp: DohResponse = http
            .get(&base_url)
            .query(&[("name", host.as_str()), ("type", "A")])
            .send()
            .await
            .map_err(|e| format!("doh request: {e}"))?
            .error_for_status()
            .map_err(|e| format!("doh status: {e}"))?
            .json()
            .await
            .map_err(|e| format!("doh body: {e}"))?;
        if resp.status != 0 {
            return Err(format!("doh status={}", resp.status));
        }
        let addrs: Vec<SocketAddr> = resp
            .answer
            .into_iter()
            .filter(|a| a.r#type == 1) // A records; the speaker is IPv4-only today
            .filter_map(|a| {
                let ip = a.data.parse().ok()?;
                Some(SocketAddr::new(ip, 0))
            })
            .collect();
        if addrs.is_empty() {
            return Err(format!("doh: no A records for {host}"));
        }
        Ok(addrs)
    }
}

impl reqwest::dns::Resolve for DohResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        // The returned future is 'static: clone the pieces it needs.
        let http = self.http.clone();
        let base_url = self.base_url.clone();
        let cache = self.cache.clone();
        let host = name.as_str().to_string();
        Box::pin(async move {
            if let Ok(cache) = cache.lock() {
                if let Some((addrs, at)) = cache.get(&host) {
                    if at.elapsed() < CACHE_TTL {
                        let addrs = addrs.clone();
                        return Ok(Box::new(addrs.into_iter())
                            as Box<dyn Iterator<Item = SocketAddr> + Send>);
                    }
                }
            }
            let addrs = Self::lookup(http, base_url, host.clone())
                .await
                .map_err(|e| {
                    tracing::warn!("doh resolve {host} failed: {e}");
                    e
                })?;
            if let Ok(mut cache) = cache.lock() {
                cache.insert(host, (addrs.clone(), Instant::now()));
                // Keep the cache bounded: it only ever holds service hostnames.
                if cache.len() > 32 {
                    cache.retain(|_, (_, at)| at.elapsed() < CACHE_TTL);
                }
            }
            Ok(Box::new(addrs.into_iter()) as Box<dyn Iterator<Item = SocketAddr> + Send>)
        })
    }
}

/// Parse an RFC 7231 HTTP-date (`Sun, 06 Nov 1994 08:49:37 GMT`) to unix secs.
pub fn parse_http_date(s: &str) -> Option<i64> {
    // Monotonic month table keeps this dependency-free.
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let s = s.trim();
    // "Sun, 06 Nov 1994 08:49:37 GMT" -> drop weekday.
    let rest = s.split_once(", ").map(|(_, r)| r).unwrap_or(s);
    let mut parts = rest.split_whitespace();
    let day: i64 = parts.next()?.parse().ok()?;
    let mon_str = parts.next()?;
    let mon = MONTHS.iter().position(|m| *m == mon_str)? as i64;
    let year: i64 = parts.next()?.parse().ok()?;
    let mut hms = parts.next()?.split(':');
    let hour: i64 = hms.next()?.parse().ok()?;
    let min: i64 = hms.next()?.parse().ok()?;
    let sec: i64 = hms.next()?.parse().ok()?;
    // Days since epoch (civil-from-days algorithm, Howard Hinnant).
    let y = if mon < 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (mon + 10) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + hour * 3600 + min * 60 + sec)
}

/// Lower bound for clock bootstrap sanity (`2025-01-01T00:00:00Z`).
pub const MIN_SANE_UNIX_SECS: i64 = 1_735_689_600;
/// Upper bound for clock bootstrap sanity (`2036-01-01T00:00:00Z`).
pub const MAX_SANE_UNIX_SECS: i64 = 2_082_758_400;

/// True when `unix_secs` falls inside the trusted clock-bootstrap window.
pub fn is_sane_bootstrap_time(unix_secs: i64) -> bool {
    (MIN_SANE_UNIX_SECS..=MAX_SANE_UNIX_SECS).contains(&unix_secs)
}

/// True when the system clock is too old to trust for TLS (a cold-booted
/// speaker sits in 2010 until something sets the clock).
pub fn clock_needs_bootstrap(now: SystemTime) -> bool {
    now.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() < MIN_SANE_UNIX_SECS as u64)
        .unwrap_or(true)
}

/// Fetch a `Date` header over plain HTTP from a fixed IP and return unix secs.
/// Plain HTTP on purpose: no TLS (clock not trusted yet), no DNS (may be
/// broken), only a TCP dial + one response header.
pub async fn fetch_http_date(url: &str) -> Result<i64, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("http date request: {e}"))?;
    let date = resp
        .headers()
        .get(reqwest::header::DATE)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| "no Date header".to_string())?;
    parse_http_date(date).ok_or_else(|| format!("unparseable Date: {date}"))
}

/// Set the system clock (root only). Rejects timestamps outside
/// `[MIN_SANE_UNIX_SECS, MAX_SANE_UNIX_SECS]` before invoking `settimeofday`.
pub fn set_system_clock(unix_secs: i64) -> Result<(), String> {
    if !is_sane_bootstrap_time(unix_secs) {
        return Err(format!(
            "timestamp {unix_secs} outside sane window [{MIN_SANE_UNIX_SECS}..={MAX_SANE_UNIX_SECS}]"
        ));
    }
    let tv = libc::timeval {
        tv_sec: unix_secs as libc::time_t,
        tv_usec: 0,
    };
    // Safety: plain syscall wrapper with a valid timeval.
    let rc = unsafe { libc::settimeofday(&tv, std::ptr::null()) };
    if rc == 0 {
        Ok(())
    } else {
        Err(format!(
            "settimeofday rc={rc} errno={}",
            std::io::Error::last_os_error()
        ))
    }
}

/// Startup time bootstrap: when `HM_TIME_BOOTSTRAP=1` and the clock is bad,
/// pull a Date header and settimeofday. Bounded and best-effort — failures
/// are logged, never fatal.
pub async fn time_bootstrap_if_needed() -> Option<i64> {
    if std::env::var("HM_TIME_BOOTSTRAP").ok().as_deref() != Some("1") {
        return None;
    }
    let now = SystemTime::now();
    if !clock_needs_bootstrap(now) {
        return None;
    }
    let url = std::env::var("HM_TIME_DATE_URL").unwrap_or_else(|_| "http://223.5.5.5/".into());
    match fetch_http_date(&url).await {
        Ok(secs) if !is_sane_bootstrap_time(secs) => {
            tracing::warn!(
                "time bootstrap: rejecting out-of-window timestamp {secs} from {url} (expected [{MIN_SANE_UNIX_SECS}..={MAX_SANE_UNIX_SECS}])"
            );
            None
        }
        Ok(secs) => match set_system_clock(secs) {
            Ok(()) => {
                tracing::info!("time bootstrap: clock set to {secs} via {url}");
                Some(secs)
            }
            Err(e) => {
                tracing::warn!("time bootstrap: got {secs} but settimeofday failed: {e}");
                None
            }
        },
        Err(e) => {
            tracing::warn!("time bootstrap: {e}");
            None
        }
    }
}

/// RFC1918 + Tailscale CGNAT private CIDRs permitted when opening speaker
/// firewall ports (`tcp/8932`, `udp/8933`). Public internet (`0.0.0.0/0`) is
/// never opened.
pub const PRIVATE_LAN_CIDRS: &[&str] = &[
    "192.168.0.0/16",
    "10.0.0.0/8",
    "172.16.0.0/12",
    "100.64.0.0/10",
];

/// True iff `ip` falls within RFC1918 (`10.0.0.0/8`, `172.16.0.0/12`,
/// `192.168.0.0/16`) or Tailscale CGNAT (`100.64.0.0/10`). Handles
/// IPv4-mapped IPv6 (`::ffff:x.x.x.x`). Public IPs, loopback, link-local,
/// multicast, and global IPv6 always return `false`.
pub fn is_private_lan_ip(ip: std::net::IpAddr) -> bool {
    let v4 = match ip {
        std::net::IpAddr::V4(v4) => v4,
        std::net::IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4,
            None => return false,
        },
    };
    let [a, b, _, _] = v4.octets();
    a == 10
        || (a == 192 && b == 168)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 100 && (64..=127).contains(&b))
}

/// True iff `ip` is an IPv6 Unique Local Address (`fc00::/7`).
pub fn is_ipv6_ula(ip: std::net::IpAddr) -> bool {
    matches!(ip, std::net::IpAddr::V6(v6) if (v6.segments()[0] & 0xfe00) == 0xfc00)
}

/// True iff `ip` is a Carrier-Grade NAT / Tailscale / WireGuard overlay address
/// (`100.64.0.0/10` or IPv4-mapped `::ffff:100.64.0.0/10`). RFC1918 home LAN
/// addresses (`192.168.0.0/16`, `10.0.0.0/8`, `172.16.0.0/12`) and loopback
/// return `false` so local LAN Wi-Fi clients keep native lossless sample rates.
pub fn is_cgnat_vpn_ip(ip: std::net::IpAddr) -> bool {
    let v4 = match ip {
        std::net::IpAddr::V4(v4) => v4,
        std::net::IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4,
            None => return false,
        },
    };
    let [a, b, _, _] = v4.octets();
    a == 100 && (64..=127).contains(&b)
}

/// Query the Linux kernel's smoothed TCP round-trip time (`tcpi_rtt`, in
/// microseconds) for a live gateway socket. Returns `None` on non-Linux hosts
/// or before the kernel has recorded a non-zero RTT sample.
pub fn tcp_smoothed_rtt_us(stream: &tokio::net::TcpStream) -> Option<u32> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = stream;
        None
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let mut info: libc::tcp_info = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::tcp_info>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::IPPROTO_TCP,
                libc::TCP_INFO,
                (&mut info as *mut libc::tcp_info).cast::<libc::c_void>(),
                &mut len,
            )
        };
        let min_bytes = std::mem::offset_of!(libc::tcp_info, tcpi_rtt) + std::mem::size_of::<u32>();
        if rc == 0 && (len as usize) >= min_bytes && info.tcpi_rtt > 0 {
            Some(info.tcpi_rtt)
        } else {
            None
        }
    }
}

/// Cap kernel-queued unsent TCP bytes (`TCP_NOTSENT_LOWAT`) on non-loopback
/// gateway sockets so bulk media chunks stay in user-space priority queues
/// (where `Control`/`Voice` frames preempt them) without shrinking `SO_SNDBUF`
/// or throttling LAN Hi-Fi bandwidth-delay product (`#297`).
/// Best-effort: returns `false` on unsupported OSes/kernels without failing.
pub fn set_tcp_notsent_lowat(stream: &tokio::net::TcpStream, bytes: u32) -> bool {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::os::fd::AsRawFd;
        #[cfg(target_os = "linux")]
        const OPT_TCP_NOTSENT_LOWAT: libc::c_int = 25;
        #[cfg(target_os = "macos")]
        const OPT_TCP_NOTSENT_LOWAT: libc::c_int = 0x201;

        let val: libc::c_uint = bytes;
        let rc = unsafe {
            libc::setsockopt(
                stream.as_raw_fd(),
                libc::IPPROTO_TCP,
                OPT_TCP_NOTSENT_LOWAT,
                (&val as *const libc::c_uint).cast::<libc::c_void>(),
                std::mem::size_of::<libc::c_uint>() as libc::socklen_t,
            )
        };
        rc == 0
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (stream, bytes);
        false
    }
}

/// Test-only readback of `TCP_NOTSENT_LOWAT` via `getsockopt` so socket
/// configuration tests assert actual kernel state rather than a proxy call.
#[cfg(test)]
pub fn tcp_notsent_lowat(stream: &tokio::net::TcpStream) -> Option<u32> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::os::fd::AsRawFd;
        #[cfg(target_os = "linux")]
        const OPT_TCP_NOTSENT_LOWAT: libc::c_int = 25;
        #[cfg(target_os = "macos")]
        const OPT_TCP_NOTSENT_LOWAT: libc::c_int = 0x201;

        let mut val: libc::c_uint = 0;
        let mut len = std::mem::size_of::<libc::c_uint>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::IPPROTO_TCP,
                OPT_TCP_NOTSENT_LOWAT,
                (&mut val as *mut libc::c_uint).cast::<libc::c_void>(),
                &mut len,
            )
        };
        if rc == 0 && (len as usize) >= std::mem::size_of::<libc::c_uint>() {
            Some(val)
        } else {
            None
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = stream;
        None
    }
}

/// True iff `ip` is a loopback address (`127.0.0.0/8`, `::1`, or IPv4-mapped
/// `::ffff:127.x.x.x`).
pub fn is_loopback_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_loopback(),
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

/// Detect whether `iptables -S INPUT` or `iptables -L INPUT -n` output
/// indicates a default `DROP` policy on the `INPUT` chain (e.g. Dot2 emOS).
pub fn input_policy_is_drop(iptables_output: &str) -> bool {
    iptables_output.lines().any(|line| {
        let trimmed = line.trim();
        trimmed == "-P INPUT DROP"
            || (trimmed.starts_with("Chain INPUT") && trimmed.contains("policy DROP"))
    })
}

#[cfg(target_os = "linux")]
fn resolve_iptables_bin() -> &'static str {
    for candidate in [
        "/system/bin/iptables",
        "/sbin/iptables",
        "/usr/sbin/iptables",
        "/bin/iptables",
    ] {
        if std::path::Path::new(candidate).is_file() {
            return candidate;
        }
    }
    "iptables"
}

#[cfg(target_os = "linux")]
fn read_iptables_input_policy(iptables: &str) -> Option<String> {
    let probes: [&[&str]; 2] = [&["-L", "INPUT", "-n"], &["-S", "INPUT"]];
    for args in probes {
        if let Ok(out) = std::process::Command::new(iptables).args(args).output() {
            if out.status.success() {
                return Some(String::from_utf8_lossy(&out.stdout).into_owned());
            }
        }
    }
    None
}

/// On Linux drop-by-default speaker firewalls (Dot2 emOS, running as root),
/// idempotently ensure `iptables` accepts private-LAN packets for the given
/// `(proto, port)` pairs (e.g. `("tcp", 8932)` and `("udp", 8933)`).
///
/// Safe no-op on non-Linux hosts, non-root processes (NAS Docker / dev), and
/// hosts whose `INPUT` chain policy is not `DROP`. Best-effort — never fatal.
pub fn ensure_speaker_lan_firewall_ports(ports: &[(&str, u16)]) {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = ports;
    }
    #[cfg(target_os = "linux")]
    {
        if ports.is_empty() {
            return;
        }
        // Only root on embedded speakers can/should touch iptables.
        if unsafe { libc::geteuid() } != 0 {
            return;
        }
        let iptables = resolve_iptables_bin();
        let Some(stdout) = read_iptables_input_policy(iptables) else {
            return;
        };
        if !input_policy_is_drop(&stdout) {
            return;
        }
        let mut opened = 0usize;
        for &(proto, port) in ports {
            if !matches!(proto, "tcp" | "udp") || port == 0 {
                continue;
            }
            let port_s = port.to_string();
            for &cidr in PRIVATE_LAN_CIDRS {
                let exists = std::process::Command::new(iptables)
                    .args([
                        "-C", "INPUT", "-p", proto, "--dport", &port_s, "-s", cidr, "-j", "ACCEPT",
                    ])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success());
                if exists {
                    continue;
                }
                match std::process::Command::new(iptables)
                    .args([
                        "-I", "INPUT", "-p", proto, "--dport", &port_s, "-s", cidr, "-j", "ACCEPT",
                    ])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                {
                    Ok(s) if s.success() => opened += 1,
                    Ok(s) => tracing::warn!(
                        proto,
                        port,
                        cidr,
                        code = ?s.code(),
                        "speaker firewall: iptables insert failed"
                    ),
                    Err(e) => tracing::warn!(
                        proto,
                        port,
                        cidr,
                        error = %e,
                        "speaker firewall: iptables exec failed"
                    ),
                }
            }
        }
        if opened > 0 {
            tracing::info!(
                opened,
                "speaker firewall: opened private-LAN iptables rules for multi-gateway/discovery"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_policy_is_drop_recognizes_both_iptables_formats() {
        assert!(input_policy_is_drop(
            "Chain INPUT (policy DROP)\ntarget prot opt source destination\n"
        ));
        assert!(!input_policy_is_drop(
            "Chain INPUT (policy ACCEPT)\ntarget prot opt source destination\n"
        ));
        assert!(input_policy_is_drop("-P INPUT DROP\n-A INPUT -j ACCEPT\n"));
        assert!(!input_policy_is_drop("-P INPUT ACCEPT\n"));
        assert!(!PRIVATE_LAN_CIDRS.contains(&"0.0.0.0/0"));
    }

    #[test]
    fn is_private_lan_ip_matches_rfc1918_and_tailscale_only() {
        for ok in [
            "10.0.0.1",
            "10.255.255.254",
            "172.16.0.1",
            "172.31.255.254",
            "192.168.0.1",
            "192.168.1.100",
            "100.64.0.1",
            "100.127.255.254",
            "::ffff:192.168.1.50",
            "::ffff:100.64.10.20",
        ] {
            let ip: std::net::IpAddr = ok.parse().unwrap();
            assert!(is_private_lan_ip(ip), "{ok} should be private LAN");
        }
        for vpn in [
            "100.64.0.1",
            "100.100.100.100",
            "100.127.255.254",
            "::ffff:100.64.10.20",
        ] {
            assert!(
                is_cgnat_vpn_ip(vpn.parse().unwrap()),
                "{vpn} should be CGNAT VPN"
            );
        }
        for non_vpn in [
            "127.0.0.1",
            "192.168.1.100",
            "10.0.0.1",
            "172.16.0.1",
            "100.63.255.255",
            "100.128.0.1",
        ] {
            assert!(
                !is_cgnat_vpn_ip(non_vpn.parse().unwrap()),
                "{non_vpn} must NOT be CGNAT VPN"
            );
        }
        for bad in [
            "127.0.0.1",
            "::1",
            "0.0.0.0",
            "8.8.8.8",
            "1.1.1.1",
            "203.0.113.5",
            "172.15.255.255",
            "172.32.0.1",
            "100.63.255.255",
            "100.128.0.1",
            "169.254.1.1",
            "224.0.0.1",
            "ff02::1",
            "::ffff:8.8.8.8",
            "2001:db8::1",
        ] {
            let ip: std::net::IpAddr = bad.parse().unwrap();
            assert!(!is_private_lan_ip(ip), "{bad} must NOT be private LAN");
        }
    }

    #[test]
    fn is_ipv6_ula_and_loopback_ip_classify_addresses_accurately() {
        for ula in ["fc00::1", "fd12:3456:789a::1", "fdff:ffff::ffff"] {
            assert!(is_ipv6_ula(ula.parse().unwrap()), "{ula} should be ULA");
        }
        for non_ula in [
            "::1",
            "fe80::1",
            "2001:db8::1",
            "fbff::1",
            "fe00::1",
            "192.168.1.1",
        ] {
            assert!(
                !is_ipv6_ula(non_ula.parse().unwrap()),
                "{non_ula} must NOT be ULA"
            );
        }
        for lb in ["127.0.0.1", "127.0.1.1", "::1", "::ffff:127.0.0.1"] {
            assert!(
                is_loopback_ip(lb.parse().unwrap()),
                "{lb} should be loopback"
            );
        }
        for non_lb in [
            "192.168.1.1",
            "203.0.113.9",
            "::ffff:192.168.1.1",
            "fc00::1",
        ] {
            assert!(
                !is_loopback_ip(non_lb.parse().unwrap()),
                "{non_lb} must NOT be loopback"
            );
        }
    }

    #[test]
    fn parses_rfc7231_dates() {
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784111777)
        );
        assert_eq!(
            parse_http_date("Sat, 08 Nov 2026 01:27:26 GMT"),
            Some(1_794_101_246)
        );
        assert_eq!(parse_http_date("garbage"), None);
    }

    #[test]
    fn bootstrap_threshold_matches_2025() {
        assert!(clock_needs_bootstrap(
            UNIX_EPOCH + Duration::from_secs(1_262_304_400)
        )); // 2010-01-01
        let sane = SystemTime::now();
        // Only a genuinely-past clock triggers; "now" on any build machine is 2026+.
        assert!(!clock_needs_bootstrap(sane));
    }

    #[test]
    fn clock_bootstrap_rejects_out_of_window_timestamps() {
        assert!(is_sane_bootstrap_time(MIN_SANE_UNIX_SECS));
        assert!(is_sane_bootstrap_time(1_794_101_246));
        assert!(is_sane_bootstrap_time(MAX_SANE_UNIX_SECS));
        assert!(!is_sane_bootstrap_time(MIN_SANE_UNIX_SECS - 1));
        assert!(!is_sane_bootstrap_time(0));
        assert!(!is_sane_bootstrap_time(-1));
        assert!(!is_sane_bootstrap_time(MAX_SANE_UNIX_SECS + 1));

        let err_low = set_system_clock(MIN_SANE_UNIX_SECS - 1).unwrap_err();
        assert!(err_low.contains("outside sane window"), "{err_low}");
        let err_high = set_system_clock(MAX_SANE_UNIX_SECS + 1).unwrap_err();
        assert!(err_high.contains("outside sane window"), "{err_high}");
    }

    // Minimal one-shot HTTP server speaking the AliDNS /resolve JSON shape,
    // so the resolver's wire format is pinned by a test (not just by prod).
    async fn mock_doh(payload: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            let body = payload.as_bytes();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.write_all(body).await.unwrap();
        });
        format!("http://{addr}/resolve")
    }

    #[tokio::test]
    async fn doh_resolver_parses_a_records() {
        let url = mock_doh(
            r#"{"Status":0,"Answer":[{"name":"x.test.","type":1,"TTL":60,"data":"192.0.2.10"},{"name":"x.test.","type":5,"data":"skip-cname"}]}"#,
        )
        .await;
        let resolver = DohResolver {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            base_url: url,
            cache: std::sync::Arc::new(Mutex::new(HashMap::new())),
        };
        use reqwest::dns::Resolve as _;
        let name: reqwest::dns::Name = "x.test".parse().unwrap();
        let addrs: Vec<SocketAddr> = resolver.resolve(name).await.unwrap().collect();
        assert_eq!(addrs, vec!["192.0.2.10:0".parse().unwrap()]);
    }
}
