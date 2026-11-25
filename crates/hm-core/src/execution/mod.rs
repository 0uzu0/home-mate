//! Server-owned memory authority. Neither this type nor a live grant is deserializable.
pub mod api;
mod bindings;
pub(crate) mod migration;
pub use bindings::ScopeBindings;

use crate::{store::Store, working_memory::ScopeContext};
use rusqlite::{Connection, OptionalExtension};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Instant;

pub const BINDING_TTL_MS: u64 = 15 * 60 * 1000;

pub(crate) type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

pub(crate) struct Lease {
    pub active: AtomicBool,
    pub deadline: Instant,
    pub principal: String,
    pub clock: Clock,
}

#[derive(Clone)]
pub struct Authority {
    scope: ScopeContext,
    physical_device: Option<String>,
    write_scope: Option<(String, String)>,
    principal: String,
    lan_exempt: bool,
    epoch: u64,
    lease: Option<Arc<Lease>>,
    store: Arc<Store>,
    cancel: Option<Arc<AtomicBool>>,
    cancel_scope: Option<Arc<crate::task::CancellationScope>>,
}

impl Authority {
    pub(crate) fn internal(store: Arc<Store>, scope: ScopeContext) -> Result<Self, String> {
        let scope = scope.normalized();
        let write_scope = if scope.person_id.is_some() || scope.device_id.is_some() {
            Some((
                scope.person_id.clone().unwrap_or_default(),
                scope.device_id.clone().unwrap_or_default(),
            ))
        } else {
            None
        };
        let epoch = store
            .with(|c| {
                Ok(
                    c.query_row("SELECT data_epoch FROM config_head WHERE id=1", [], |r| {
                        r.get::<_, i64>(0)
                    })?,
                )
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            physical_device: scope.device_id.clone(),
            scope,
            write_scope,
            principal: "internal".into(),
            lan_exempt: false,
            epoch: epoch as u64,
            lease: None,
            store,
            cancel: None,
            cancel_scope: None,
        })
    }

    pub(crate) fn internal_with_lan_exempt(
        store: Arc<Store>,
        scope: ScopeContext,
        lan_exempt: bool,
    ) -> Result<Self, String> {
        Ok(Self::internal(store, scope)?.with_lan_exempt(lan_exempt))
    }

    pub fn anonymous(store: Arc<Store>, principal: Option<String>) -> Result<Self, String> {
        let mut authority = Self::internal(store, ScopeContext::default())?;
        authority.principal = principal
            .map(|p| format!("management:{p}"))
            .unwrap_or_else(|| "anonymous".into());
        Ok(authority)
    }

    pub fn with_lan_exempt(mut self, lan_exempt: bool) -> Self {
        self.lan_exempt = lan_exempt;
        self
    }

    pub fn is_lan_exempt(&self) -> bool {
        self.lan_exempt
    }

    pub(crate) fn with_cancel(mut self, cancel: Arc<AtomicBool>) -> Self {
        self.cancel = Some(cancel);
        self
    }
    /// Capture provenance from trusted physical ingress, separate from selected visibility.
    pub fn physical_device_id(&self) -> Option<&str> {
        self.physical_device.as_deref()
    }
    pub(crate) fn with_cancel_scope(mut self, scope: Arc<crate::task::CancellationScope>) -> Self {
        self.cancel = Some(scope.flag());
        self.cancel_scope = Some(scope);
        self
    }
    /// Same cancellation gate and lock order as ordinary task commits.
    pub(crate) fn mutate<T>(
        &self,
        store: &Store,
        action: impl FnOnce(&Connection) -> crate::store::Result<T>,
    ) -> crate::store::Result<T> {
        if !std::ptr::eq(store, self.store.as_ref()) {
            return Err(crate::store::StoreError::Authority(
                "execution store mismatch".into(),
            ));
        }
        let apply = || store.with(action);
        match &self.cancel_scope {
            Some(scope) => scope
                .commit(apply)
                .ok_or_else(|| crate::store::StoreError::Authority("task cancelled".into()))?,
            None => apply(),
        }
    }
    pub(crate) fn confirmation_requester(&self) -> Option<Option<String>> {
        if let Some(lease) = &self.lease {
            Some(Some(lease.principal.clone()))
        } else if let Some(principal) = self.principal.strip_prefix("management:") {
            Some(Some(principal.into()))
        } else if self.physical_device.is_some() {
            Some(None)
        } else {
            None
        }
    }
    pub(crate) fn admit<T>(
        &self,
        f: impl FnOnce() -> crate::store::Result<T>,
    ) -> crate::store::Result<T> {
        match &self.cancel_scope {
            Some(scope) => scope
                .commit(f)
                .ok_or_else(|| crate::store::StoreError::Authority("task cancelled".into()))?,
            None => f(),
        }
    }
    /// Consumed-outcome bookkeeping may survive cancellation, but history
    /// publication must linearize through the same original cancellation gate.
    pub(crate) fn bookkeep<T>(&self, f: impl FnOnce(bool) -> T) -> T {
        match &self.cancel_scope {
            Some(scope) => scope.inspect(f),
            None => f(!self
                .cancel
                .as_ref()
                .is_some_and(|c| c.load(Ordering::SeqCst))),
        }
    }
    pub fn scope(&self) -> &ScopeContext {
        &self.scope
    }
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
    pub fn default_write_scope(&self) -> Option<(&str, &str)> {
        self.write_scope
            .as_ref()
            .map(|(p, d)| (p.as_str(), d.as_str()))
    }
    pub fn can_read(&self, person: &str, device: &str) -> bool {
        self.scope.allows(person, device)
    }
    pub fn can_write(&self, person: &str, device: &str) -> bool {
        self.default_write_scope() == Some((person, device))
    }
    /// Omitted model fields default to the one server-authorized destination.
    /// Explicit empty strings request household scope and cannot widen it.
    pub fn write_target(
        &self,
        person: Option<&str>,
        device: Option<&str>,
    ) -> Option<(String, String)> {
        let (p, d) = self.default_write_scope()?;
        let target = (
            person.map(str::trim).unwrap_or(p),
            device.map(str::trim).unwrap_or(d),
        );
        self.can_write(target.0, target.1)
            .then(|| (target.0.into(), target.1.into()))
    }
    pub fn check(&self) -> Result<(), String> {
        self.store
            .with(|c| self.check_in(c))
            .map_err(|e| e.to_string())
    }
    /// Call inside the same Store transaction/lock as the protected mutation.
    pub(crate) fn check_in(&self, c: &Connection) -> crate::store::Result<()> {
        if self
            .cancel
            .as_ref()
            .is_some_and(|c| c.load(Ordering::SeqCst))
        {
            return Err(crate::store::StoreError::Authority("task cancelled".into()));
        }
        let epoch: i64 = c.query_row("SELECT data_epoch FROM config_head WHERE id=1", [], |r| {
            r.get(0)
        })?;
        if epoch as u64 != self.epoch {
            return Err(crate::store::StoreError::Authority(
                "execution data epoch changed".into(),
            ));
        }
        if let Some(lease) = &self.lease {
            if !lease.active.load(Ordering::SeqCst) || (lease.clock)() >= lease.deadline {
                return Err(crate::store::StoreError::Authority(
                    "scope binding expired or revoked".into(),
                ));
            }
            let raw: Option<String> = c
                .query_row(
                    "SELECT value FROM config_kv WHERE key='api_tokens'",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            let live = raw
                .and_then(|raw| serde_json::from_str::<Vec<crate::auth::TokenEntry>>(&raw).ok())
                .is_some_and(|entries| entries.iter().any(|entry| entry.id == lease.principal));
            if !live {
                return Err(crate::store::StoreError::Authority(
                    "scope principal revoked".into(),
                ));
            }
        }
        Ok(())
    }
    fn session_fingerprint(&self) -> Result<String, String> {
        // Physical device sessions (`dev:*`) are owned by the device identity
        // (`DeviceSlot::can_be_claimed_by` blocks lower-trust hijacking) and are
        // bound/managed by `SessionStore` regardless of whether a LAN gateway
        // currently connects unauthenticated (`lan_exempt = true`) or paired
        // (`lan_exempt = false`). Anonymous HTTP chat sessions (`physical_device
        // == None`) keep `lan_exempt` in the fingerprint so unauthenticated LAN
        // chat cannot share a session with loopback anonymous chat.
        let fingerprint_lan_exempt = if self.physical_device.is_some() {
            false
        } else {
            self.lan_exempt
        };
        serde_json::to_string(&(
            &self.principal,
            fingerprint_lan_exempt,
            &self.scope.person_id,
            &self.scope.device_id,
            &self.write_scope,
        ))
        .map_err(|e| e.to_string())
    }
    pub(crate) fn session_is_bound(&self, session: &str) -> Result<bool, String> {
        self.store
            .with(|c| {
                self.check_in(c)?;
                self.session_is_bound_in(c, session)
            })
            .map_err(|e| e.to_string())
    }
    pub(crate) fn session_is_bound_in(
        &self,
        c: &Connection,
        session: &str,
    ) -> crate::store::Result<bool> {
        let fingerprint = self
            .session_fingerprint()
            .map_err(crate::store::StoreError::Authority)?;
        let existing: Option<(String, i64)> = c
            .query_row(
                "SELECT authority,data_epoch FROM execution_sessions WHERE session_id=?1",
                [session],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(existing.is_some_and(|(key, epoch)| key == fingerprint && epoch as u64 == self.epoch))
    }
    pub(crate) fn bind_session_in(
        &self,
        c: &Connection,
        session: &str,
    ) -> crate::store::Result<()> {
        let fingerprint = self
            .session_fingerprint()
            .map_err(crate::store::StoreError::Authority)?;
        let existing: Option<(String, i64)> = c
            .query_row(
                "SELECT authority,data_epoch FROM execution_sessions WHERE session_id=?1",
                [session],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match existing {
            Some((key, epoch)) if key == fingerprint && epoch as u64 == self.epoch => {}
            Some(_) => {
                return Err(crate::store::StoreError::Authority(
                    "session belongs to another execution scope".into(),
                ))
            }
            None => {
                let messages: i64 = c.query_row(
                    "SELECT count(*) FROM messages WHERE session_id=?1",
                    [session],
                    |r| r.get(0),
                )?;
                if messages != 0 {
                    return Err(crate::store::StoreError::Authority(
                        "SESSION_SCOPE_UNVERIFIED".into(),
                    ));
                }
                c.execute(
                    "INSERT INTO execution_sessions(session_id,authority,data_epoch) VALUES (?1,?2,?3)",
                    rusqlite::params![session, fingerprint, self.epoch as i64],
                )?;
            }
        }
        Ok(())
    }
    pub(crate) fn bind_session(&self, session: &str) -> Result<(), String> {
        self.store
            .with(|c| {
                let tx = c.unchecked_transaction()?;
                self.check_in(&tx)?;
                self.bind_session_in(&tx, session)?;
                tx.commit()?;
                Ok(())
            })
            .map_err(|e| e.to_string())
    }
}
