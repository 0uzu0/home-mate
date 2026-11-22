//! Reset linearizes under Store's existing connection lock and data_epoch.
use super::*;
use rusqlite::Connection;

impl SharedMemoryService {
    pub(super) fn epoch_from(c: &Connection) -> crate::store::Result<i64> {
        c.query_row("SELECT data_epoch FROM config_head WHERE id=1", [], |row| {
            row.get(0)
        })
        .map_err(Into::into)
    }
    pub(super) fn epoch(&self) -> Result<i64, String> {
        self.store
            .with(Self::epoch_from)
            .map_err(|error| error.to_string())
    }
    /// Compare and mutate while holding the very same Store lock as reset.
    pub(super) fn at_epoch<T>(
        &self,
        epoch: i64,
        apply: impl FnOnce(&Connection) -> crate::store::Result<T>,
    ) -> Result<Option<T>, String> {
        self.store
            .with(|c| {
                if Self::epoch_from(c)? != epoch {
                    return Ok(None);
                }
                apply(c).map(Some)
            })
            .map_err(|error| error.to_string())
    }
    pub(super) fn admit(&self, epoch: i64) -> Result<(), SyncOutcome> {
        match self.at_epoch(epoch, |_| Ok(())) {
            Ok(Some(())) => Ok(()),
            Ok(None) => Err(SyncOutcome::DiscardedReset),
            Err(error) => Err(SyncOutcome::Failed(error)),
        }
    }
    pub(super) fn bind_circuit(st: &mut CircuitState, epoch: i64) {
        if st.epoch != epoch {
            *st = CircuitState {
                epoch,
                ..Default::default()
            };
        }
    }
    /// Always Store -> circuit, including status and configuration updates.
    pub(super) fn current_circuit<T>(
        &self,
        apply: impl FnOnce(&mut CircuitState) -> T,
    ) -> Result<T, String> {
        self.store
            .with(|c| {
                let epoch = Self::epoch_from(c)?;
                let mut st = crate::lock_or_recover(&self.circuit, "shared_memory.circuit");
                Self::bind_circuit(&mut st, epoch);
                Ok(apply(&mut st))
            })
            .map_err(|error| error.to_string())
    }
    pub(super) fn acknowledge(&self, epoch: i64, ids: &[String]) -> Result<Option<usize>, String> {
        self.at_epoch(epoch, |c| {
            let tx = c.unchecked_transaction()?;
            let mut count = 0;
            for id in ids {
                count += tx.execute("DELETE FROM shared_memory_outbox WHERE id=?1", [id])?;
            }
            tx.commit()?;
            Ok(count)
        })
    }
    pub(super) fn node_id(&self) -> Result<String, String> {
        self.store
            .with(|c| {
                let id: String = c.query_row(
                    "SELECT COALESCE(node_id, '') FROM config_head WHERE id=1",
                    [],
                    |r| r.get(0),
                )?;
                Ok(id)
            })
            .map_err(|error| error.to_string())
    }
    pub(super) fn publish(
        &self,
        epoch: i64,
        profiles: &[ProfileWire],
    ) -> Result<Option<usize>, String> {
        self.at_epoch(epoch, |c| {
            let tx = c.unchecked_transaction()?;
            let node_id: String = tx.query_row(
                "SELECT COALESCE(node_id, '') FROM config_head WHERE id=1",
                [],
                |r| r.get(0),
            )?;
            let mut count = 0;
            let fetched_ms = crate::id::now_ms() as i64;
            let mut keep_ids = std::collections::HashSet::new();
            for profile in profiles {
                let id = profile.id.trim();
                let summary = profile.summary.trim();
                let scope_device = profile.scope_device.trim();
                let scope_person = profile.scope_person.trim();
                if id.is_empty() || summary.is_empty() {
                    continue;
                }
                if !node_id.trim().is_empty()
                    && !scope_device.is_empty()
                    && scope_device != node_id.trim()
                {
                    continue;
                }
                let dimension = v2::normalize_dimension(&profile.dimension);
                let version = profile.version.max(1) as i64;
                tx.execute(
                    "INSERT INTO shared_profile_cache
                     (id, scope_person, scope_device, source_device, version, row_revision, dimension, summary, updated_ms, fetched_ms)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
                     ON CONFLICT(id) DO UPDATE SET
                        scope_person=excluded.scope_person,
                        scope_device=excluded.scope_device,
                        source_device=excluded.source_device,
                        version=excluded.version,
                        row_revision=excluded.row_revision,
                        dimension=excluded.dimension,
                        summary=excluded.summary,
                        updated_ms=excluded.updated_ms,
                        fetched_ms=excluded.fetched_ms",
                    rusqlite::params![
                        id,
                        scope_person,
                        scope_device,
                        profile.source_device.trim(),
                        version,
                        profile.row_revision as i64,
                        dimension,
                        summary,
                        profile.updated_ms as i64,
                        fetched_ms
                    ],
                )?;
                keep_ids.insert(id.to_string());
                count += 1;
            }
            // Reconcile deletions & scope-narrowing: remove any cached profile
            // not present in the authoritative snapshot or with empty summary.
            let existing_ids: Vec<String> = {
                let mut stmt = tx.prepare("SELECT id FROM shared_profile_cache")?;
                let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            for old_id in existing_ids {
                if !keep_ids.contains(&old_id) {
                    tx.execute("DELETE FROM shared_profile_cache WHERE id=?1", [&old_id])?;
                }
            }
            tx.execute("DELETE FROM shared_profile_cache WHERE TRIM(summary) = ''", [])?;
            tx.commit()?;
            Ok(count)
        })
    }
    pub(super) fn failure(&self, epoch: i64, error: &str) -> SyncOutcome {
        match self.at_epoch(epoch, |_| {
            let mut st = crate::lock_or_recover(&self.circuit, "shared_memory.circuit");
            Self::bind_circuit(&mut st, epoch);
            st.consecutive_failures = st.consecutive_failures.saturating_add(1);
            let exp = st.consecutive_failures.saturating_sub(1).min(4);
            let cooldown = (CIRCUIT_BASE_COOLDOWN_MS << exp).min(CIRCUIT_MAX_COOLDOWN_MS);
            let now = crate::id::now_ms();
            st.open_until_ms = now.saturating_add(cooldown);
            st.last_error = Some(error.to_owned());
            st.last_sync_ms = Some(now);
            Ok(())
        }) {
            Ok(Some(())) => SyncOutcome::Failed(error.to_owned()),
            Ok(None) => SyncOutcome::DiscardedReset,
            Err(error) => SyncOutcome::Failed(error),
        }
    }
    pub(super) fn success(&self, epoch: i64) -> Result<Option<()>, String> {
        self.at_epoch(epoch, |_| {
            let mut st = crate::lock_or_recover(&self.circuit, "shared_memory.circuit");
            Self::bind_circuit(&mut st, epoch);
            st.consecutive_failures = 0;
            st.open_until_ms = 0;
            st.last_error = None;
            st.last_sync_ms = Some(crate::id::now_ms());
            Ok(())
        })
    }
}
