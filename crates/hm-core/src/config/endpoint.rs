//! Address validation, shared by every block that stores a URL.
//!
//! All URL policy checks delegate to [`crate::tool::http`] so LAN services
//! (`home`, `music`) and public services (`weather`, `news`, `search`,
//! `openapi`) share one implementation and neither performs a DNS lookup at
//! save time.

/// `http(s)://host[:port][/path]` on the LAN, rejecting loopback (unless
/// `allow_loopback` is set for tests), link-local, cloud metadata, and public
/// IP literals ([music-service-20261112] §2).
pub fn valid_music_endpoint(raw: &str) -> bool {
    valid_music_endpoint_with(raw, false)
}

pub fn valid_music_endpoint_with(raw: &str, allow_loopback: bool) -> bool {
    crate::tool::http::validate_music_url_with(raw, allow_loopback).is_ok()
}

/// `http(s)://host[:port][/path]` for Home Assistant: allows LAN/private IPv4
/// and IPv6, `.local` mDNS names, and loopback only when `allow_loopback` is
/// enabled for tests; rejects link-local and cloud metadata. Empty string means
/// unconfigured.
pub fn valid_home_endpoint(raw: &str) -> bool {
    valid_home_endpoint_with(raw, false)
}

pub fn valid_home_endpoint_with(raw: &str, allow_loopback: bool) -> bool {
    let s = raw.trim();
    s.is_empty() || crate::tool::http::validate_home_url_with(s, allow_loopback).is_ok()
}

/// `http(s)://host[:port][/path]` for a public outbound service (`weather`,
/// `news`, `search`, `openapi`). Empty string means provider default.
pub fn valid_weather_endpoint(raw: &str) -> bool {
    valid_public_endpoint(raw, false)
}

/// Public outbound URL check with optional loopback opt-in for local test
/// servers (`Store::allow_loopback_public_for_test`).
pub fn valid_public_endpoint(raw: &str, allow_loopback: bool) -> bool {
    let s = raw.trim();
    if s.is_empty() {
        return true; // "" = provider default
    }
    crate::tool::http::validate_public_url(s, allow_loopback).is_ok()
}

/// News feed URL check: allows public HTTP(S) and RFC1918/LAN addresses
/// (e.g. `http://192.168.1.10:1200` NAS RSSHub) while rejecting link-local
/// (`169.254.0.0/16`), cloud metadata, and loopback (unless `allow_loopback`).
pub fn valid_news_endpoint(raw: &str, allow_loopback: bool) -> bool {
    let s = raw.trim();
    if s.is_empty() {
        return false;
    }
    crate::tool::http::validate_news_url(s, allow_loopback).is_ok()
}

pub(super) fn valid_place_alias(a: &str) -> bool {
    let n = a.chars().count();
    (1..=20).contains(&n)
        && !a
            .chars()
            .any(|c| c.is_control() || matches!(c, '"' | '\'' | '/' | '\\' | '|'))
}

/// `Area/Location` (Asia/Shanghai) or "auto"/"" — anything else is a config
/// error rather than a silently-wrong clock in the answer.
pub(super) fn valid_timezone(tz: &str) -> bool {
    if tz.is_empty() || tz == "auto" {
        return true;
    }
    let parts: Vec<&str> = tz.split('/').collect();
    if parts.len() != 2 {
        return false;
    }
    parts.iter().all(|p| {
        !p.is_empty()
            && p.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+'))
    })
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use std::sync::Arc;

    #[test]
    fn config_service_enforces_outbound_url_policy() {
        let store = Arc::new(crate::store::Store::open_in_memory().unwrap());
        let svc = ConfigService::new(store);

        // Home Assistant accepts LAN IPv4/IPv6 and .local.
        for ok in [
            "http://192.168.1.10:8123",
            "http://homeassistant.local:8123",
            "http://[fd00::1]:8123",
        ] {
            assert!(
                svc.save_home(
                    HomeConfig {
                        backend: HomeBackend::HomeAssistant,
                        base_url: ok.into(),
                        ..Default::default()
                    },
                    "test"
                )
                .is_ok(),
                "home should accept {ok}"
            );
        }

        // Link-local & cloud metadata rejected everywhere.
        for bad in [
            "http://169.254.169.254/latest/meta-data/",
            "http://[fe80::1]/",
            "http://metadata.google.internal/",
        ] {
            assert!(
                svc.save_home(
                    HomeConfig {
                        backend: HomeBackend::HomeAssistant,
                        base_url: bad.into(),
                        ..Default::default()
                    },
                    "test"
                )
                .is_err(),
                "home should reject {bad}"
            );
            assert!(
                svc.save_music(
                    MusicConfig {
                        library: Some(MusicConnection {
                            endpoint: bad.into(),
                            user: "u".into(),
                        }),
                        ..Default::default()
                    },
                    None,
                    None,
                    "test"
                )
                .is_err(),
                "music should reject {bad}"
            );
            assert!(
                svc.save_weather(
                    WeatherConfig {
                        endpoint: bad.into(),
                        ..Default::default()
                    },
                    None,
                    "test"
                )
                .is_err(),
                "weather should reject {bad}"
            );
            assert!(
                svc.save_news(
                    NewsConfig {
                        sources: vec![NewsSource {
                            alias: "feed".into(),
                            url: bad.into(),
                            kind: NewsKind::Rss,
                            max_items: 3,
                        }],
                        ..Default::default()
                    },
                    "test"
                )
                .is_err(),
                "news should reject {bad}"
            );
            assert!(
                svc.save_search(
                    SearchConfig {
                        base_url: bad.into(),
                        ..Default::default()
                    },
                    None,
                    "test"
                )
                .is_err(),
                "search should reject {bad}"
            );
            assert!(
                validate_openapi_config(&OpenApiConfig {
                    spec_url: bad.into(),
                    ..Default::default()
                })
                .is_err(),
                "openapi should reject {bad}"
            );
        }

        // Public services reject loopback and private LAN by default.
        for lan in [
            "http://127.0.0.1:8080",
            "http://192.168.1.10/feed",
            "http://nas.local/",
        ] {
            assert!(
                svc.save_weather(
                    WeatherConfig {
                        endpoint: lan.into(),
                        ..Default::default()
                    },
                    None,
                    "test"
                )
                .is_err(),
                "weather should reject {lan}"
            );
        }

        // News accepts RFC1918 LAN feeds (e.g. NAS RSSHub) while rejecting loopback by default.
        for lan_ok in [
            "http://192.168.1.10:1200/36kr/newsflashes",
            "http://10.0.0.5:1200/rss",
            "http://nas.local:1200/feed",
        ] {
            assert!(
                svc.save_news(
                    NewsConfig {
                        sources: vec![NewsSource {
                            alias: "科技".into(),
                            url: lan_ok.into(),
                            kind: NewsKind::Rss,
                            max_items: 5,
                        }],
                        ..Default::default()
                    },
                    "test"
                )
                .is_ok(),
                "news should accept LAN URL {lan_ok}"
            );
        }
        assert!(
            svc.save_news(
                NewsConfig {
                    sources: vec![NewsSource {
                        alias: "loopback".into(),
                        url: "http://127.0.0.1:1200/rss".into(),
                        kind: NewsKind::Rss,
                        max_items: 5,
                    }],
                    ..Default::default()
                },
                "test"
            )
            .is_err(),
            "news should reject loopback when allow_loopback is false"
        );
    }
}
