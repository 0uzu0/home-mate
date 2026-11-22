//! The music block (R1e, [music-service-20261112]): a household library
//! (Subsonic/OpenSubsonic, e.g. Navidrome) and an online service (Personal
//! Services).
//!
//! Both optional, both user-configured. `prefer_local` is the RUI-local
//! priority: a library hit is not followed by an online request. `secret` never
//! lives in this block — it is in the secrets table (write-only).

use serde::{Deserialize, Serialize};

use super::default_true;

/// Music is its own config block (R1e): its shape is not a ServiceConfig
/// (two connections, two secrets, a local-first preference), and a household
/// with no music service must still have a working assistant.
pub const MUSIC_KEY: &str = "music";
pub(crate) const MAX_MUSIC_CONNECTION_FIELD: usize = 4096;
/// Where the two connection secrets live (write-only, like every other key).
pub const MUSIC_LIBRARY_SECRET: &str = "music.library.secret";
pub const MUSIC_ONLINE_SECRET: &str = "music.online.secret";

/// One music connection. `secret` never lives here — it is in the secrets
/// table (write-only), exactly like every service key.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MusicConnection {
    #[serde(default)]
    pub endpoint: String,
    /// Subsonic user for the library. For the token-authenticated online
    /// service this is an optional namespace for cached track identities.
    #[serde(default)]
    pub user: String,
}

/// The music block (R1e, [music-service-20261112]): a household library
/// (Subsonic/OpenSubsonic, e.g. Navidrome) and an online service (Personal
/// Services). Both optional, both user-configured. `prefer_local` is the
/// RUI-local priority: a library hit is not followed by an online request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MusicConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub prefer_local: bool,
    #[serde(default)]
    pub library: Option<MusicConnection>,
    #[serde(default)]
    pub online: Option<MusicConnection>,
}

impl Default for MusicConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            prefer_local: true,
            library: None,
            online: None,
        }
    }
}

/// Read-back shape: endpoints and users are visible, secrets are not.
#[derive(Debug, Clone, Serialize)]
pub struct MusicStatus {
    pub enabled: bool,
    pub prefer_local: bool,
    pub library: Option<MusicConnectionStatus>,
    pub online: Option<MusicConnectionStatus>,
    /// Opaque revision of this block and its credential bindings. The
    /// console sends it back on save; a mismatch is refused instead of
    /// silently overwriting another page's edit (the 2026-11-12 12:39 loss
    /// of a just-saved Navidrome config).
    pub revision: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MusicConnectionStatus {
    pub endpoint: String,
    pub user: String,
    pub secret_set: bool,
}

impl MusicConfig {
    pub(crate) fn endpoint_is_portable(endpoint: &str) -> bool {
        endpoint.is_empty()
            || reqwest::Url::parse(endpoint).is_ok_and(|url| {
                matches!(url.scheme(), "http" | "https")
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none()
            })
    }

    pub(crate) fn normalized_with(
        mut self,
        allow_loopback: bool,
    ) -> Result<Self, super::ConfigError> {
        for (which, connection) in [("library", &mut self.library), ("online", &mut self.online)] {
            if let Some(connection) = connection {
                connection.normalize_with(which, allow_loopback)?;
            }
        }
        Ok(self)
    }
}

impl MusicConnection {
    pub(crate) fn normalize_with(
        &mut self,
        which: &'static str,
        allow_loopback: bool,
    ) -> Result<(), super::ConfigError> {
        let endpoint = self.endpoint.trim();
        if !MusicConfig::endpoint_is_portable(endpoint)
            || (!endpoint.is_empty() && !super::valid_music_endpoint_with(endpoint, allow_loopback))
        {
            return Err(super::ConfigError::BadMusicEndpoint { which });
        }
        self.endpoint = endpoint.trim_end_matches('/').to_owned();
        self.user = self.user.trim().to_owned();
        if [&self.endpoint, &self.user]
            .into_iter()
            .any(|value| value.contains('\0') || value.chars().count() > MAX_MUSIC_CONNECTION_FIELD)
        {
            return Err(super::ConfigError::BadMusicConnection { which });
        }
        Ok(())
    }
}
