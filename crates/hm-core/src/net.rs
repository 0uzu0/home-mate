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

#[cfg(test)]
mod tests {
    use super::*;

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
