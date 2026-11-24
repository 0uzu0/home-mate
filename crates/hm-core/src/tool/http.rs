//! Shared outbound HTTP helpers: bounded body reading and URL policy.
//!
//! All outbound tool clients use [`read_bounded`] so response-size caps and
//! body error classification (timeout vs premature close vs protocol failure)
//! are consistent across services.
//!
//! Outbound URL validation distinguishes two trust zones:
//! - [`OutboundPolicy::Lan`]: Home Assistant and Music live on the household
//!   LAN, so private IPv4/IPv6, `.local` mDNS names, and loopback are allowed,
//!   while link-local (`169.254.0.0/16`, `fe80::/10`), cloud metadata hosts,
//!   and non-http(s) schemes are rejected.
//! - [`OutboundPolicy::Public`]: Weather, News, Search, and OpenAPI talk to
//!   public services by default, so loopback, private LAN, `.local`,
//!   link-local, cloud metadata, and non-http(s) schemes are rejected.

use std::error::Error as StdError;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

/// Structured error returned by [`read_bounded`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BodyReadError {
    #[error("response body exceeded {limit} bytes")]
    TooLarge { limit: usize },
    #[error("timed out while reading response body")]
    Timeout,
    #[error("peer closed connection prematurely while reading response body: {0}")]
    PrematureClose(String),
    #[error("protocol/transport error while reading response body: {0}")]
    Protocol(String),
}

/// Read at most `limit` bytes from `resp`. Drops the stream immediately if a
/// chunk would exceed `limit` so an oversized body is never buffered in full.
pub async fn read_bounded(
    mut resp: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, BodyReadError> {
    let mut out: Vec<u8> = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                if out.len().saturating_add(chunk.len()) > limit {
                    return Err(BodyReadError::TooLarge { limit });
                }
                out.extend_from_slice(&chunk);
            }
            Ok(None) => return Ok(out),
            Err(e) => return Err(classify_body_error(&e)),
        }
    }
}

fn classify_body_error(err: &reqwest::Error) -> BodyReadError {
    if err.is_timeout() {
        return BodyReadError::Timeout;
    }

    let mut cur: Option<&(dyn StdError + 'static)> = err.source();
    while let Some(src) = cur {
        if let Some(io_err) = src.downcast_ref::<std::io::Error>() {
            match io_err.kind() {
                std::io::ErrorKind::TimedOut => return BodyReadError::Timeout,
                std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::BrokenPipe => {
                    return BodyReadError::PrematureClose(err.to_string());
                }
                _ => {}
            }
        }
        cur = src.source();
    }

    let msg = err.to_string().to_ascii_lowercase();
    if msg.contains("timed out") || msg.contains("timeout") {
        return BodyReadError::Timeout;
    }
    if msg.contains("incomplete message")
        || msg.contains("unexpected eof")
        || msg.contains("unexpected end of file")
        || msg.contains("early eof")
        || msg.contains("connection closed before message completed")
    {
        return BodyReadError::PrematureClose(err.to_string());
    }

    BodyReadError::Protocol(err.to_string())
}

/// Outbound URL validation policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboundPolicy {
    /// Home Assistant (`home`) and Music (`music`): allows LAN/private
    /// IPv4/IPv6, `.local` / LAN hostnames, and loopback only when
    /// `allow_loopback` is explicitly enabled (tests); rejects link-local,
    /// cloud metadata, unspecified/multicast, and non-http(s) schemes.
    Lan {
        reject_public_ip_literals: bool,
        allow_loopback: bool,
    },
    /// News (`news`): allows public `http(s)` endpoints AND RFC1918 / ULA /
    /// LAN hostnames (e.g. NAS RSSHub `http://192.168.1.168:1200`); rejects
    /// link-local (`169.254.0.0/16`, `fe80::/10`), cloud metadata, CGNAT,
    /// userinfo, and loopback (unless `allow_loopback` is set for tests).
    PublicOrLan { allow_loopback: bool },
    /// Weather, Search, OpenAPI: public `http(s)` endpoints only.
    /// Rejects link-local, loopback (unless `allow_loopback` is set for
    /// local test servers), private LAN, `.local`, and cloud metadata.
    Public { allow_loopback: bool },
}

/// Reason an outbound URL was rejected by [`validate_outbound_url`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UrlPolicyError {
    #[error("URL must not be empty")]
    Empty,
    #[error("URL is too long or contains control/whitespace characters")]
    Malformed,
    #[error("URL scheme must be http or https")]
    BadScheme,
    #[error("URL must not contain userinfo")]
    UserInfoForbidden,
    #[error("URL is missing a host")]
    MissingHost,
    #[error("link-local and cloud metadata addresses are forbidden")]
    LinkLocalOrMetadata,
    #[error("unspecified or multicast addresses are forbidden")]
    SpecialAddress,
    #[error("loopback addresses are forbidden for this service")]
    LoopbackForbidden,
    #[error("private/LAN addresses are forbidden for this service")]
    PrivateLanForbidden,
    #[error("public IP literals are forbidden for this service")]
    PublicIpForbidden,
}

/// Validate and parse an outbound URL against `policy`.
pub fn validate_outbound_url(
    raw: &str,
    policy: OutboundPolicy,
) -> Result<reqwest::Url, UrlPolicyError> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(UrlPolicyError::Empty);
    }
    if s.len() > 2048 || s.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(UrlPolicyError::Malformed);
    }
    let url = reqwest::Url::parse(s).map_err(|_| UrlPolicyError::Malformed)?;
    match url.scheme() {
        "http" | "https" => {}
        _ => return Err(UrlPolicyError::BadScheme),
    }
    if matches!(
        policy,
        OutboundPolicy::Public { .. } | OutboundPolicy::PublicOrLan { .. }
    ) && (!url.username().is_empty() || url.password().is_some())
    {
        return Err(UrlPolicyError::UserInfoForbidden);
    }
    let host = url.host_str().ok_or(UrlPolicyError::MissingHost)?;
    if let Some(v6_str) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        let v6: Ipv6Addr = v6_str.parse().map_err(|_| UrlPolicyError::Malformed)?;
        validate_ipv6(v6, policy)?;
    } else if let Ok(v4) = host.parse::<Ipv4Addr>() {
        validate_ipv4(v4, policy)?;
    } else if let Ok(v6) = host.parse::<Ipv6Addr>() {
        validate_ipv6(v6, policy)?;
    } else {
        validate_domain(host, policy)?;
    }
    Ok(url)
}

#[cfg(test)]
pub fn validate_home_url(raw: &str) -> Result<(), UrlPolicyError> {
    validate_home_url_with(raw, false)
}

pub fn validate_home_url_with(raw: &str, allow_loopback: bool) -> Result<(), UrlPolicyError> {
    validate_outbound_url(
        raw,
        OutboundPolicy::Lan {
            reject_public_ip_literals: false,
            allow_loopback,
        },
    )
    .map(|_| ())
}

#[cfg(test)]
pub fn validate_music_url(raw: &str) -> Result<(), UrlPolicyError> {
    validate_music_url_with(raw, false)
}

pub fn validate_music_url_with(raw: &str, allow_loopback: bool) -> Result<(), UrlPolicyError> {
    validate_outbound_url(
        raw,
        OutboundPolicy::Lan {
            reject_public_ip_literals: true,
            allow_loopback,
        },
    )
    .map(|_| ())
}

pub fn validate_public_url(raw: &str, allow_loopback: bool) -> Result<(), UrlPolicyError> {
    validate_outbound_url(raw, OutboundPolicy::Public { allow_loopback }).map(|_| ())
}

pub fn validate_news_url(raw: &str, allow_loopback: bool) -> Result<(), UrlPolicyError> {
    validate_outbound_url(raw, OutboundPolicy::PublicOrLan { allow_loopback }).map(|_| ())
}

/// Validate a DNS-resolved IP address against `policy`.
pub fn validate_resolved_ip(ip: IpAddr, policy: OutboundPolicy) -> Result<(), UrlPolicyError> {
    match ip {
        IpAddr::V4(v4) => validate_ipv4(v4, policy),
        IpAddr::V6(v6) => validate_ipv6(v6, policy),
    }
}

/// `reqwest::dns::Resolve` wrapper that enforces [`OutboundPolicy`] on every
/// resolved IP address, preventing DNS-rebinding SSRF attacks.
///
/// When `HM_RESOLVER=doh`, wraps [`crate::net::DohResolver`]; otherwise uses
/// `tokio::net::lookup_host`.
pub struct PolicyDnsResolver {
    policy: OutboundPolicy,
    inner: Option<Arc<dyn reqwest::dns::Resolve>>,
}

impl PolicyDnsResolver {
    pub fn new(policy: OutboundPolicy) -> Self {
        let inner: Option<Arc<dyn reqwest::dns::Resolve>> =
            match std::env::var("HM_RESOLVER").as_deref() {
                Ok("doh") => Some(Arc::new(crate::net::DohResolver::from_env())),
                _ => None,
            };
        Self { policy, inner }
    }

    #[cfg(test)]
    pub fn with_resolver(policy: OutboundPolicy, inner: Arc<dyn reqwest::dns::Resolve>) -> Self {
        Self {
            policy,
            inner: Some(inner),
        }
    }
}

impl reqwest::dns::Resolve for PolicyDnsResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let policy = self.policy;
        let inner = self.inner.clone();
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = if let Some(resolver) = inner {
                resolver.resolve(name).await?.collect()
            } else {
                tokio::net::lookup_host((host.as_str(), 0))
                    .await
                    .map_err(|e| -> Box<dyn StdError + Send + Sync> { Box::new(e) })?
                    .collect()
            };
            if addrs.is_empty() {
                return Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("dns resolution returned no addresses for {host}"),
                )) as Box<dyn StdError + Send + Sync>);
            }
            for addr in &addrs {
                if let Err(err) = validate_resolved_ip(addr.ip(), policy) {
                    return Err(Box::new(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        format!(
                            "resolved IP {} for {host} rejected by outbound policy: {err}",
                            addr.ip()
                        ),
                    )) as Box<dyn StdError + Send + Sync>);
                }
            }
            Ok(Box::new(addrs.into_iter()) as Box<dyn Iterator<Item = SocketAddr> + Send>)
        })
    }
}

/// Attach a [`PolicyDnsResolver`] enforcing `policy` to a `reqwest::ClientBuilder`.
pub fn apply_policy_resolver(
    builder: reqwest::ClientBuilder,
    policy: OutboundPolicy,
) -> reqwest::ClientBuilder {
    builder.dns_resolver(Arc::new(PolicyDnsResolver::new(policy)))
}

fn validate_ipv4(v4: Ipv4Addr, policy: OutboundPolicy) -> Result<(), UrlPolicyError> {
    if v4.is_link_local() {
        return Err(UrlPolicyError::LinkLocalOrMetadata);
    }
    if v4.is_unspecified()
        || v4.octets()[0] == 0
        || v4.is_broadcast()
        || v4.is_multicast()
        || v4.is_documentation()
    {
        return Err(UrlPolicyError::SpecialAddress);
    }
    if v4.is_loopback() {
        return match policy {
            OutboundPolicy::Lan {
                allow_loopback: true,
                ..
            }
            | OutboundPolicy::Public {
                allow_loopback: true,
            }
            | OutboundPolicy::PublicOrLan {
                allow_loopback: true,
            } => Ok(()),
            OutboundPolicy::Lan {
                allow_loopback: false,
                ..
            }
            | OutboundPolicy::Public {
                allow_loopback: false,
            }
            | OutboundPolicy::PublicOrLan {
                allow_loopback: false,
            } => Err(UrlPolicyError::LoopbackForbidden),
        };
    }
    let is_cgnat = v4.octets()[0] == 100 && (64..=127).contains(&v4.octets()[1]);
    if is_cgnat {
        return match policy {
            OutboundPolicy::Lan { .. } => Ok(()),
            OutboundPolicy::Public { .. } | OutboundPolicy::PublicOrLan { .. } => {
                Err(UrlPolicyError::PrivateLanForbidden)
            }
        };
    }
    if v4.is_private() {
        return match policy {
            OutboundPolicy::Lan { .. } | OutboundPolicy::PublicOrLan { .. } => Ok(()),
            OutboundPolicy::Public { .. } => Err(UrlPolicyError::PrivateLanForbidden),
        };
    }
    match policy {
        OutboundPolicy::Lan {
            reject_public_ip_literals: true,
            ..
        } => Err(UrlPolicyError::PublicIpForbidden),
        OutboundPolicy::Lan {
            reject_public_ip_literals: false,
            ..
        }
        | OutboundPolicy::Public { .. }
        | OutboundPolicy::PublicOrLan { .. } => Ok(()),
    }
}

fn validate_ipv6(v6: Ipv6Addr, policy: OutboundPolicy) -> Result<(), UrlPolicyError> {
    if let Some(v4) = v6.to_ipv4_mapped() {
        return validate_ipv4(v4, policy);
    }
    if is_ipv6_link_local(v6) || is_aws_ipv6_metadata(v6) {
        return Err(UrlPolicyError::LinkLocalOrMetadata);
    }
    if v6.is_unspecified() || v6.is_multicast() {
        return Err(UrlPolicyError::SpecialAddress);
    }
    if v6.is_loopback() {
        return match policy {
            OutboundPolicy::Lan {
                allow_loopback: true,
                ..
            }
            | OutboundPolicy::Public {
                allow_loopback: true,
            }
            | OutboundPolicy::PublicOrLan {
                allow_loopback: true,
            } => Ok(()),
            OutboundPolicy::Lan {
                allow_loopback: false,
                ..
            }
            | OutboundPolicy::Public {
                allow_loopback: false,
            }
            | OutboundPolicy::PublicOrLan {
                allow_loopback: false,
            } => Err(UrlPolicyError::LoopbackForbidden),
        };
    }
    if is_ipv6_ula(v6) {
        return match policy {
            OutboundPolicy::Lan { .. } | OutboundPolicy::PublicOrLan { .. } => Ok(()),
            OutboundPolicy::Public { .. } => Err(UrlPolicyError::PrivateLanForbidden),
        };
    }
    match policy {
        OutboundPolicy::Lan {
            reject_public_ip_literals: true,
            ..
        } => Err(UrlPolicyError::PublicIpForbidden),
        OutboundPolicy::Lan {
            reject_public_ip_literals: false,
            ..
        }
        | OutboundPolicy::Public { .. }
        | OutboundPolicy::PublicOrLan { .. } => Ok(()),
    }
}

fn validate_domain(domain: &str, policy: OutboundPolicy) -> Result<(), UrlPolicyError> {
    let host = domain.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return Err(UrlPolicyError::MissingHost);
    }
    if is_metadata_hostname(&host) {
        return Err(UrlPolicyError::LinkLocalOrMetadata);
    }
    if host == "localhost" || host.ends_with(".localhost") {
        return match policy {
            OutboundPolicy::Lan {
                allow_loopback: true,
                ..
            }
            | OutboundPolicy::Public {
                allow_loopback: true,
            }
            | OutboundPolicy::PublicOrLan {
                allow_loopback: true,
            } => Ok(()),
            OutboundPolicy::Lan {
                allow_loopback: false,
                ..
            }
            | OutboundPolicy::Public {
                allow_loopback: false,
            }
            | OutboundPolicy::PublicOrLan {
                allow_loopback: false,
            } => Err(UrlPolicyError::LoopbackForbidden),
        };
    }
    if is_lan_hostname(&host) {
        return match policy {
            OutboundPolicy::Lan { .. } | OutboundPolicy::PublicOrLan { .. } => Ok(()),
            OutboundPolicy::Public { .. } => Err(UrlPolicyError::PrivateLanForbidden),
        };
    }
    Ok(())
}

fn is_ipv6_link_local(v6: Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xffc0) == 0xfe80
}

fn is_ipv6_ula(v6: Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xfe00) == 0xfc00
}

fn is_aws_ipv6_metadata(v6: Ipv6Addr) -> bool {
    v6.segments() == [0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254]
}

fn is_metadata_hostname(host: &str) -> bool {
    host == "metadata.google.internal"
        || host.ends_with(".metadata.google.internal")
        || host == "metadata.goog"
        || host.ends_with(".metadata.goog")
        || host == "metadata"
        || host == "instance-data"
}

fn is_lan_hostname(host: &str) -> bool {
    !host.contains('.')
        || host.ends_with(".local")
        || host.ends_with(".lan")
        || host.ends_with(".internal")
        || host.ends_with(".home.arpa")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn lan_policy_accepts_home_and_music_private_urls_and_rejects_loopback_by_default() {
        assert!(validate_home_url("http://192.168.1.10:8123").is_ok());
        assert!(validate_home_url("http://10.0.0.5:8123").is_ok());
        assert!(validate_home_url("http://172.16.1.2:8123").is_ok());
        assert!(validate_home_url("http://[fd00::1]:8123").is_ok());
        assert!(validate_home_url("http://homeassistant.local:8123").is_ok());
        assert_eq!(
            validate_home_url("http://127.0.0.1:8123"),
            Err(UrlPolicyError::LoopbackForbidden)
        );
        assert_eq!(
            validate_home_url("http://[::1]:8123"),
            Err(UrlPolicyError::LoopbackForbidden)
        );
        assert_eq!(
            validate_home_url("http://localhost:8123"),
            Err(UrlPolicyError::LoopbackForbidden)
        );
        assert!(validate_home_url_with("http://127.0.0.1:8123", true).is_ok());
        assert!(validate_home_url_with("http://[::1]:8123", true).is_ok());

        assert!(validate_music_url("http://192.168.1.27:4533").is_ok());
        assert!(validate_music_url("http://nas.local:4533/music").is_ok());
        assert!(validate_music_url("https://music.lan").is_ok());
        assert_eq!(
            validate_music_url("http://127.0.0.1:9999"),
            Err(UrlPolicyError::LoopbackForbidden)
        );
        assert!(validate_music_url_with("http://127.0.0.1:9999", true).is_ok());
        assert!(validate_music_url("http://8.8.8.8:4533").is_err());
    }

    #[test]
    fn link_local_and_cloud_metadata_rejected_everywhere() {
        let bad = [
            "http://169.254.169.254/latest/meta-data/",
            "http://169.254.1.1:8123",
            "http://[fe80::1]/",
            "http://[fe80::1]:8123/api",
            "http://[::ffff:169.254.169.254]/",
            "http://metadata.google.internal/computeMetadata/v1/",
            "http://metadata.goog/",
            "http://[fd00:ec2::254]/latest/meta-data/",
            "ftp://homeassistant.local",
            "file:///etc/passwd",
        ];
        for url in bad {
            assert!(validate_home_url(url).is_err(), "home accepted {url}");
            assert!(validate_music_url(url).is_err(), "music accepted {url}");
            assert!(
                validate_public_url(url, false).is_err(),
                "public(false) accepted {url}"
            );
            assert!(
                validate_public_url(url, true).is_err(),
                "public(true) accepted {url}"
            );
        }
    }

    #[test]
    fn public_policy_rejects_loopback_and_lan_by_default() {
        assert!(validate_public_url("https://api.open-meteo.com/v1/forecast", false).is_ok());
        assert!(validate_public_url("https://1.1.1.1/dns-query", false).is_ok());
        assert!(validate_public_url("https://[2606:4700:4700::1111]/", false).is_ok());

        for url in [
            "http://127.0.0.1:8080/v1",
            "http://[::1]:8080/v1",
            "http://localhost:8080/v1",
            "http://192.168.1.10/feed",
            "http://10.0.0.1/api",
            "http://172.16.0.1/api",
            "http://[fc00::1]/api",
            "http://nas.local/feed",
            "http://service.internal/api",
            "http://user:pass@example.com/",
        ] {
            assert!(
                validate_public_url(url, false).is_err(),
                "public accepted {url}"
            );
        }

        // Loopback opt-in for local test servers still rejects private LAN on strict public policy.
        assert!(validate_public_url("http://127.0.0.1:8080/v1", true).is_ok());
        assert!(validate_public_url("http://192.168.1.10/feed", true).is_err());

        // News policy allows both public URLs and RFC1918/LAN feeds (e.g. NAS RSSHub),
        // while keeping link-local/metadata and non-test loopback blocked.
        assert!(validate_news_url("https://hnrss.org/frontpage", false).is_ok());
        assert!(validate_news_url("http://192.168.1.168:1200/36kr/newsflashes", false).is_ok());
        assert!(validate_news_url("http://10.0.0.5:1200/rss", false).is_ok());
        assert!(validate_news_url("http://172.16.1.2:1200/rss", false).is_ok());
        assert!(validate_news_url("http://[fd00::1]:1200/rss", false).is_ok());
        assert!(validate_news_url("http://nas.local:1200/rss", false).is_ok());
        assert!(validate_news_url("http://127.0.0.1:1200/rss", false).is_err());
        assert!(validate_news_url("http://127.0.0.1:1200/rss", true).is_ok());
        assert!(validate_news_url("http://169.254.169.254/latest/meta-data/", false).is_err());
        assert!(validate_news_url("http://169.254.169.254/latest/meta-data/", true).is_err());
        assert!(validate_news_url("http://100.100.100.200/latest/meta-data/", false).is_err());
    }

    #[tokio::test]
    async fn read_bounded_distinguishes_timeout_premature_close_and_protocol() {
        // 1. Stalled body -> Timeout
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 512];
            let _ = sock.read(&mut buf).await;
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nhello")
                .await;
            tokio::time::sleep(Duration::from_secs(2)).await;
        });

        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(80))
            .build()
            .unwrap();
        let resp = client
            .get(format!("http://{addr}/stall"))
            .send()
            .await
            .unwrap();
        let err = read_bounded(resp, 1024).await.unwrap_err();
        assert_eq!(err, BodyReadError::Timeout);

        // 2. Premature peer close -> PrematureClose
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 512];
            let _ = sock.read(&mut buf).await;
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nhello")
                .await;
            drop(sock);
        });

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let resp = client
            .get(format!("http://{addr}/eof"))
            .send()
            .await
            .unwrap();
        let err = read_bounded(resp, 1024).await.unwrap_err();
        assert!(
            matches!(err, BodyReadError::PrematureClose(_)),
            "expected PrematureClose, got {err:?}"
        );

        // 3. Invalid chunked framing -> Protocol
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 512];
            let _ = sock.read(&mut buf).await;
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nZZZZ\r\n")
                .await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        });

        let resp = client
            .get(format!("http://{addr}/proto"))
            .send()
            .await
            .unwrap();
        let err = read_bounded(resp, 1024).await.unwrap_err();
        assert!(
            matches!(err, BodyReadError::Protocol(_)),
            "expected Protocol, got {err:?}"
        );
    }

    struct StaticResolver(Vec<SocketAddr>);

    impl reqwest::dns::Resolve for StaticResolver {
        fn resolve(&self, _name: reqwest::dns::Name) -> reqwest::dns::Resolving {
            let addrs = self.0.clone();
            Box::pin(async move {
                Ok(Box::new(addrs.into_iter()) as Box<dyn Iterator<Item = SocketAddr> + Send>)
            })
        }
    }

    #[tokio::test]
    async fn policy_dns_resolver_rejects_metadata_and_disallowed_ips() {
        use reqwest::dns::Resolve;

        let mk_name = || "rebind.example.com".parse::<reqwest::dns::Name>().unwrap();

        // Link-local / cloud metadata IP must be rejected under every policy,
        // even when mixed with a valid public IP.
        let metadata_inner = Arc::new(StaticResolver(vec![
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)), 0),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)), 0),
        ]));
        for policy in [
            OutboundPolicy::Public {
                allow_loopback: false,
            },
            OutboundPolicy::Public {
                allow_loopback: true,
            },
            OutboundPolicy::PublicOrLan {
                allow_loopback: false,
            },
            OutboundPolicy::PublicOrLan {
                allow_loopback: true,
            },
            OutboundPolicy::Lan {
                reject_public_ip_literals: false,
                allow_loopback: false,
            },
            OutboundPolicy::Lan {
                reject_public_ip_literals: false,
                allow_loopback: true,
            },
        ] {
            let resolver = PolicyDnsResolver::with_resolver(policy, metadata_inner.clone());
            let err = resolver
                .resolve(mk_name())
                .await
                .err()
                .expect("metadata must fail");
            assert!(
                err.to_string().contains("169.254.169.254"),
                "unexpected error for {policy:?}: {err}"
            );
        }

        // Private LAN IP is rejected by Public, allowed by PublicOrLan.
        let lan_inner = Arc::new(StaticResolver(vec![SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(192, 168, 5, 10)),
            0,
        )]));
        let pub_res = PolicyDnsResolver::with_resolver(
            OutboundPolicy::Public {
                allow_loopback: false,
            },
            lan_inner.clone(),
        );
        assert!(pub_res.resolve(mk_name()).await.is_err());
        let lan_res = PolicyDnsResolver::with_resolver(
            OutboundPolicy::PublicOrLan {
                allow_loopback: false,
            },
            lan_inner,
        );
        let resolved: Vec<SocketAddr> = lan_res.resolve(mk_name()).await.unwrap().collect();
        assert_eq!(resolved.len(), 1);

        // Loopback is rejected when allow_loopback=false, allowed when true.
        let lb_inner = Arc::new(StaticResolver(vec![SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            0,
        )]));
        assert!(PolicyDnsResolver::with_resolver(
            OutboundPolicy::Public {
                allow_loopback: false
            },
            lb_inner.clone(),
        )
        .resolve(mk_name())
        .await
        .is_err());
        assert!(PolicyDnsResolver::with_resolver(
            OutboundPolicy::Public {
                allow_loopback: true
            },
            lb_inner,
        )
        .resolve(mk_name())
        .await
        .is_ok());
    }
}
