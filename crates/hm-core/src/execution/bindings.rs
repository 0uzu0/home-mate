use super::*;
use crate::{auth::AuthStore, session::SessionStore};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Mutex, time::Duration};

struct Binding {
    authority: Authority,
    session: String,
    expires_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (ScopeBindings, String) {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let auth = AuthStore::new(store.clone());
        let token = auth.provision("synthetic").unwrap();
        let principal = auth.verify(&token).unwrap();
        let sessions = Arc::new(SessionStore::new(store.clone()));
        (ScopeBindings::new(store, auth, sessions), principal)
    }

    #[test]
    fn expiry_revoke_rotation_cancel_and_epoch_fence_actual_mutations() {
        let (mut bindings, principal) = fixture();
        let now = Arc::new(Mutex::new(Instant::now()));
        let clock = now.clone();
        bindings.clock = Arc::new(move || *clock.lock().unwrap());
        let binding = bindings
            .issue(&principal, Some("小明".into()), Some("B".into()), false)
            .unwrap();
        let id = binding["binding_id"].as_str().unwrap();
        let (authority, _) = bindings.resolve(id, &principal, None).unwrap();
        let memory = crate::memory::MemoryService::new(bindings.store.clone());
        memory
            .add_authorized(&authority, "valid scoped note", "fact", "voice")
            .unwrap();
        *now.lock().unwrap() += Duration::from_millis(BINDING_TTL_MS);
        assert!(bindings.read(id, &principal).is_err());
        assert!(memory
            .add_authorized(&authority, "expired note", "fact", "voice")
            .is_err());
        let fresh = bindings
            .issue(&principal, None, Some("B".into()), false)
            .unwrap();
        let (authority, _) = bindings
            .resolve(fresh["binding_id"].as_str().unwrap(), &principal, None)
            .unwrap();
        let cancel = Arc::new(AtomicBool::new(true));
        assert!(memory
            .add_authorized(
                &authority.clone().with_cancel(cancel),
                "cancelled note",
                "fact",
                "voice"
            )
            .is_err());
        bindings
            .revoke(fresh["binding_id"].as_str().unwrap(), &principal)
            .unwrap();
        assert!(memory
            .add_authorized(&authority, "revoked note", "fact", "voice")
            .is_err());
        let fresh = bindings
            .issue(&principal, None, Some("B".into()), false)
            .unwrap();
        let (authority, _) = bindings
            .resolve(fresh["binding_id"].as_str().unwrap(), &principal, None)
            .unwrap();
        bindings.auth.rotate("replacement").unwrap();
        assert!(memory
            .add_authorized(&authority, "rotated note", "fact", "voice")
            .is_err());
        let device = Authority::internal(
            bindings.store.clone(),
            ScopeContext {
                device_id: Some("B".into()),
                room: None,
                person_id: None,
            },
        )
        .unwrap();
        let candidate = memory
            .add_candidate_authorized(
                &device,
                "device candidate",
                "fact",
                "device-session",
                "test",
                "fact",
            )
            .unwrap();
        assert_eq!(
            (&*candidate.scope_person, &*candidate.scope_device),
            ("", "B")
        );
        let item = memory.confirm_candidate(&candidate.id).unwrap();
        assert_eq!((&*item.scope_person, &*item.scope_device), ("", "B"));
        let stale_candidate = memory
            .add_candidate_authorized(
                &device,
                "late candidate",
                "fact",
                "another-session",
                "test",
                "fact",
            )
            .unwrap();
        bindings
            .store
            .with(|c| {
                c.execute(
                    "UPDATE config_head SET data_epoch=data_epoch+1 WHERE id=1",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        assert!(memory.confirm_candidate(&stale_candidate.id).is_err());
        assert!(memory
            .add_authorized(&device, "stale note", "fact", "voice")
            .is_err());
        assert_eq!(memory.list().unwrap().len(), 2);
    }

    #[test]
    fn capacity_rejection_does_not_create_durable_rows_and_other_actor_cannot_read() {
        let (bindings, principal) = fixture();
        let binding = bindings
            .issue(&principal, None, Some("B".into()), false)
            .unwrap();
        assert!(bindings
            .read(binding["binding_id"].as_str().unwrap(), "wrong-principal")
            .is_err());
        for _ in 1..128 {
            bindings
                .issue(&principal, None, Some("B".into()), false)
                .unwrap();
        }
        let counts = || {
            bindings
                .store
                .with(|c| {
                    Ok((
                        c.query_row("SELECT count(*) FROM sessions", [], |r| r.get::<_, i64>(0))?,
                        c.query_row("SELECT count(*) FROM execution_sessions", [], |r| {
                            r.get::<_, i64>(0)
                        })?,
                    ))
                })
                .unwrap()
        };
        let before = counts();
        for _ in 0..3 {
            assert_eq!(
                bindings
                    .issue(&principal, None, Some("B".into()), false)
                    .unwrap_err(),
                "SCOPE_BINDING_CAPACITY"
            );
        }
        assert_eq!(counts(), before);
    }
}

pub struct ScopeBindings {
    store: Arc<Store>,
    auth: Arc<AuthStore>,
    sessions: Arc<SessionStore>,
    bindings: Mutex<HashMap<String, Binding>>,
    clock: Clock,
}

impl ScopeBindings {
    pub fn new(store: Arc<Store>, auth: Arc<AuthStore>, sessions: Arc<SessionStore>) -> Self {
        Self {
            store,
            auth,
            sessions,
            bindings: Mutex::new(HashMap::new()),
            clock: Arc::new(Instant::now),
        }
    }
    pub fn principal(&self, token: &str) -> Result<String, String> {
        self.auth
            .verify(token)
            .ok_or_else(|| "PAIRING_REQUIRED".into())
    }
    pub fn issue(
        &self,
        principal: &str,
        person: Option<String>,
        device: Option<String>,
        household_write: bool,
    ) -> Result<Value, String> {
        let scope = ScopeContext {
            person_id: person,
            device_id: device,
            room: None,
        }
        .normalized();
        if scope
            .person_id
            .as_ref()
            .is_some_and(|p| p.chars().count() > 120 || p.chars().any(char::is_control))
            || scope
                .device_id
                .as_ref()
                .is_some_and(|d| !crate::fleet::valid_peer_id(d))
        {
            return Err("INVALID_SCOPE".into());
        }
        if household_write && (scope.person_id.is_some() || scope.device_id.is_some()) {
            return Err("HOUSEHOLD_GRANT_REQUIRES_HOUSEHOLD_SCOPE".into());
        }
        let mut bindings = crate::lock_or_recover(&self.bindings, "execution.bindings");
        bindings.retain(|_, b| b.authority.check().is_ok());
        if bindings.len() >= 128 {
            return Err("SCOPE_BINDING_CAPACITY".into());
        }
        let id = crate::config_delivery::uuid().map_err(|e| format!("{e:?}"))?;
        let session = format!(
            "S{}",
            crate::config_delivery::uuid().map_err(|e| format!("{e:?}"))?
        );
        self.sessions
            .ensure(&session)
            .map_err(|e| format!("{e:?}"))?;
        let mut authority = Authority::internal(self.store.clone(), scope)?;
        authority.physical_device = None; // A text scope selection cannot mint a physical origin.
        if household_write {
            authority.write_scope = Some((String::new(), String::new()));
        }
        authority.principal = format!("binding:{principal}:{id}");
        authority.lease = Some(Arc::new(Lease {
            active: AtomicBool::new(true),
            deadline: (self.clock)() + Duration::from_millis(BINDING_TTL_MS),
            principal: principal.into(),
            clock: self.clock.clone(),
        }));
        authority.bind_session(&session)?;
        let binding = Binding {
            authority,
            session,
            expires_ms: crate::id::now_ms() + BINDING_TTL_MS,
        };
        let snapshot = Self::snapshot(&id, &binding);
        bindings.insert(id, binding);
        Ok(snapshot)
    }
    fn snapshot(id: &str, binding: &Binding) -> Value {
        json!({"binding_id":id,"session_id":binding.session,"scope":binding.authority.scope,
            "household_write":binding.authority.default_write_scope()==Some(("","")),"expires_ms":binding.expires_ms,
            "ttl_ms":BINDING_TTL_MS,"data_epoch":binding.authority.epoch})
    }
    fn matches(binding: &Binding, principal: &str) -> bool {
        binding
            .authority
            .lease
            .as_ref()
            .is_some_and(|l| l.principal == principal)
    }
    pub fn read(&self, id: &str, principal: &str) -> Result<Value, String> {
        let bindings = crate::lock_or_recover(&self.bindings, "execution.bindings");
        let b = bindings
            .get(id)
            .filter(|b| Self::matches(b, principal))
            .ok_or("SCOPE_BINDING_UNAVAILABLE")?;
        b.authority.check()?;
        Ok(Self::snapshot(id, b))
    }
    pub fn resolve(
        &self,
        id: &str,
        principal: &str,
        session: Option<&str>,
    ) -> Result<(Authority, String), String> {
        let bindings = crate::lock_or_recover(&self.bindings, "execution.bindings");
        let b = bindings
            .get(id)
            .filter(|b| Self::matches(b, principal))
            .ok_or("SCOPE_BINDING_UNAVAILABLE")?;
        b.authority.check()?;
        if session.is_some_and(|s| s != b.session) {
            return Err("SESSION_SCOPE_MISMATCH".into());
        }
        Ok((b.authority.clone(), b.session.clone()))
    }
    pub fn revoke(&self, id: &str, principal: &str) -> Result<(), String> {
        let bindings = crate::lock_or_recover(&self.bindings, "execution.bindings");
        let b = bindings
            .get(id)
            .filter(|b| Self::matches(b, principal))
            .ok_or("SCOPE_BINDING_UNAVAILABLE")?;
        self.store
            .with(|_| {
                b.authority
                    .lease
                    .as_ref()
                    .unwrap()
                    .active
                    .store(false, Ordering::SeqCst);
                Ok(())
            })
            .map_err(|e| e.to_string())
    }
}
