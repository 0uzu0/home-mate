//! Where a track comes from: the household library (Subsonic/OpenSubsonic) and
//! the online service (Personal Services).
//!
//! Search and fetch only — what to do with a stream (decode, push, confirm)
//! belongs to `decode.rs` and `session.rs`. The `binding` on every `Track` ties
//! an id to the connection that returned it, so an id from another server or
//! account is never fetched from this one.

use crate::config::{MusicConfig, MusicConnection};
use md5::{Digest, Md5};
use serde_json::Value;

use super::{MusicError, Secrets, Source, Track, MAX_BODY};

// --- Subsonic (library) ----------------------------------------------------

pub(super) fn subsonic_auth(conn: &MusicConnection, secret: &str) -> Result<String, MusicError> {
    let salt = format!(
        "{:x}",
        Md5::digest(format!("{}-{}", crate::id::now_ms(), crate::id::next("S")))
    );
    let token = format!("{:x}", Md5::digest(format!("{secret}{salt}")));
    let mut url = String::new();
    let base = conn.endpoint.trim_end_matches('/');
    url.push_str(base);
    url.push_str("/rest/");
    Ok(format!(
        "u={}&t={}&s={}&v=1.16.1&c=home-mate&f=json",
        urlencoding(&conn.user),
        token,
        salt
    ))
}

/// Minimal percent-encoding for query VALUES (the Subsonic query carries
/// user text: spaces, 中文, & and friends).
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn subsonic_url(
    conn: &MusicConnection,
    secret: &str,
    endpoint: &str,
    params: &[(&str, String)],
) -> Result<String, MusicError> {
    let auth = subsonic_auth(conn, secret)?;
    let base = conn.endpoint.trim_end_matches('/');
    let extra: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{}={}", k, urlencoding(v)))
        .collect();
    let mut url = format!("{base}/rest/{endpoint}.view?{auth}");
    for e in extra {
        url.push('&');
        url.push_str(&e);
    }
    Ok(url)
}

fn binding(conn: &MusicConnection) -> String {
    format!("{}|{}", conn.endpoint.trim_end_matches('/'), conn.user)
}

pub(super) fn valid_library_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 160
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

pub(super) fn valid_online_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 160 && id.bytes().all(|b| b.is_ascii_digit())
}

async fn body_limited(resp: reqwest::Response) -> Result<Value, MusicError> {
    let bytes = resp.bytes().await.map_err(|_| MusicError::Http)?;
    if bytes.len() > MAX_BODY {
        return Err(MusicError::Response);
    }
    serde_json::from_slice(&bytes).map_err(|_| MusicError::Response)
}

pub(super) async fn library_search(
    client: &reqwest::Client,
    conn: &MusicConnection,
    secret: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<Track>, MusicError> {
    let q = query.trim();
    if q.chars().count() > 80 || q.chars().any(char::is_control) {
        return Err(MusicError::Response);
    }
    let resp = if q.is_empty() {
        let url = subsonic_url(
            conn,
            secret,
            "getRandomSongs",
            &[("size", limit.to_string())],
        )?;
        client
            .get(url)
            .send()
            .await
            .map_err(|_| MusicError::Connection)?
    } else {
        let url = subsonic_url(
            conn,
            secret,
            "search3",
            &[
                ("query", q.to_string()),
                ("songCount", limit.to_string()),
                ("artistCount", "0".into()),
                ("albumCount", "0".into()),
            ],
        )?;
        client
            .get(url)
            .send()
            .await
            .map_err(|_| MusicError::Connection)?
    };
    let resp = resp.error_for_status().map_err(|_| MusicError::Http)?;
    let body = body_limited(resp).await?;
    let root = &body["subsonic-response"];
    if root["status"] != "ok" {
        // Auth/permission errors land here (error code in the body, never the
        // secret) — the caller reports "library rejected the request".
        return Err(MusicError::Rejected);
    }
    let rows = &root[if q.is_empty() {
        "randomSongs"
    } else {
        "searchResult3"
    }]["song"];
    let bind = binding(conn);
    let mut out = Vec::new();
    if let Some(items) = rows.as_array() {
        for row in items.iter().take(limit) {
            let id = row["id"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| row["id"].as_u64().map(|v| v.to_string()))
                .unwrap_or_default();
            if !valid_library_id(&id) {
                continue;
            }
            let title: String = row["title"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(256)
                .collect();
            if title.trim().is_empty() {
                continue;
            }
            let artist: String = row["artist"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(256)
                .collect();
            out.push(Track {
                source: Source::Library,
                id,
                title,
                artist,
                binding: bind.clone(),
            });
        }
    }
    Ok(out)
}

// --- Personal Services (online) -------------------------------------------

pub(super) async fn online_search(
    client: &reqwest::Client,
    conn: &MusicConnection,
    token: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<Track>, MusicError> {
    let q = query.trim();
    if q.is_empty() {
        // The online service is query-driven: there is nothing to browse.
        return Err(MusicError::Response);
    }
    let base = conn.endpoint.trim_end_matches('/');
    let url = format!("{base}/v1/search?q={}&limit={}", urlencoding(q), limit);
    let resp = client
        .get(url)
        .header("X-Internal-Token", token)
        .send()
        .await
        .map_err(|_| MusicError::Connection)?
        .error_for_status()
        .map_err(|_| MusicError::Http)?;
    let body = body_limited(resp).await?;
    let bind = binding(conn);
    let mut out = Vec::new();
    if let Some(items) = body["results"].as_array() {
        for row in items.iter().take(limit) {
            let id = row["id"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| row["id"].as_u64().map(|v| v.to_string()))
                .unwrap_or_default();
            if !valid_online_id(&id) {
                continue;
            }
            let title: String = row["title"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(256)
                .collect();
            if title.trim().is_empty() {
                continue;
            }
            let artist: String = row["artist"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(256)
                .collect();
            out.push(Track {
                source: Source::Online,
                id,
                title,
                artist,
                binding: bind.clone(),
            });
        }
    }
    Ok(out)
}

pub(super) async fn open_stream(
    client: &reqwest::Client,
    cfg: &MusicConfig,
    secrets: &Secrets,
    track: &Track,
) -> Result<reqwest::Response, MusicError> {
    match track.source {
        Source::Library => {
            let conn = cfg.library.as_ref().ok_or(MusicError::SourceDisabled)?;
            let secret = secrets.library.as_ref().ok_or(MusicError::Credential)?;
            if track.binding != binding(conn) {
                return Err(MusicError::CandidateRequired);
            }
            let url = subsonic_url(
                conn,
                secret,
                "stream",
                &[("id", track.id.clone()), ("format", "raw".into())],
            )?;
            client
                .get(url)
                .send()
                .await
                .map_err(|_| MusicError::Connection)?
                .error_for_status()
                .map_err(|_| MusicError::Http)
        }
        Source::Online => {
            let conn = cfg.online.as_ref().ok_or(MusicError::SourceDisabled)?;
            let token = secrets.online.as_ref().ok_or(MusicError::Credential)?;
            if track.binding != binding(conn) {
                return Err(MusicError::CandidateRequired);
            }
            let base = conn.endpoint.trim_end_matches('/');
            // 192 kbps: the device plays mono PCM, so a bigger bitrate buys
            // nothing a listener can hear.
            let url = format!("{base}/v1/stream/{}?max_br=192", track.id);
            client
                .get(url)
                .header("X-Internal-Token", token)
                .send()
                .await
                .map_err(|_| MusicError::Connection)?
                .error_for_status()
                .map_err(|_| MusicError::Http)
        }
        Source::Story | Source::Radio => Err(MusicError::SourceDisabled),
    }
}
