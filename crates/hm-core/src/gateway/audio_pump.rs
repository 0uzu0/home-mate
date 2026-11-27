//! Per-device bounded audio pump queues (`MG-P1-03`).
//!
//! Each device receives its own bounded FIFO `mpsc` channel (capacity 256) and
//! dedicated consumer task in `VoicePipeline`, so a high-rate burst or slow
//! Silero ONNX inference on Device B can never lag-drop or head-of-line-block
//! mic frames belonging to sibling Device A.

use super::QueuedDeviceEvent;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, Notify};

/// Bounded per-device audio frame buffer capacity (FIFO).
pub const DEVICE_AUDIO_QUEUE_CAP: usize = 256;

/// Upper bound on lazily created per-device audio queues.
const MAX_TRACKED_AUDIO_DEVICES: usize = 64;

/// Per-device barrier tracking `AudioChunk` enqueue/VAD completion progress so
/// `DeviceEvent::Keyword` can await draining all preceding preroll `AudioChunk`s
/// before `on_keyword_at` samples `current_block_cursor()` / wake capture.
pub struct DeviceAudioSync {
    enqueued_seq: AtomicU64,
    processed_seq: AtomicU64,
    closed: AtomicBool,
    notify: Notify,
}

impl DeviceAudioSync {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            enqueued_seq: AtomicU64::new(0),
            processed_seq: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            notify: Notify::new(),
        })
    }

    pub fn mark_enqueued(&self, seq: u64) {
        self.enqueued_seq.fetch_max(seq, Ordering::Release);
    }

    pub fn mark_processed(&self, seq: u64) {
        self.processed_seq.fetch_max(seq, Ordering::Release);
        self.notify.notify_waiters();
    }

    pub fn mark_closed(&self) {
        self.closed.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    pub fn enqueued_seq(&self) -> u64 {
        self.enqueued_seq.load(Ordering::Acquire)
    }

    pub fn processed_seq(&self) -> u64 {
        self.processed_seq.load(Ordering::Acquire)
    }

    pub async fn wait_processed(&self, target_seq: u64) {
        if target_seq == 0 {
            return;
        }
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            // Register the waiter BEFORE checking the atomics so a concurrent
            // `mark_processed` / `mark_closed` (`notify_waiters`) can never be
            // lost between the atomic load and the first poll of `notified`.
            notified.as_mut().enable();
            if self.processed_seq.load(Ordering::Acquire) >= target_seq
                || self.closed.load(Ordering::Acquire)
            {
                return;
            }
            notified.await;
        }
    }
}

pub struct DeviceAudioQueueHandle {
    pub device: String,
    pub rx: mpsc::Receiver<QueuedDeviceEvent>,
    pub sync: Arc<DeviceAudioSync>,
}

struct DeviceQueueSlot {
    tx: mpsc::Sender<QueuedDeviceEvent>,
    dropped: Arc<AtomicU64>,
    sync: Arc<DeviceAudioSync>,
    protected: bool,
}

pub(super) struct AudioPumpRouter {
    queues: Mutex<HashMap<String, DeviceQueueSlot>>,
    dropped_by_device: Mutex<BTreeMap<String, Arc<AtomicU64>>>,
    dropped_total: AtomicU64,
    new_queues_tx: mpsc::UnboundedSender<DeviceAudioQueueHandle>,
    new_queues_rx: Mutex<Option<mpsc::UnboundedReceiver<DeviceAudioQueueHandle>>>,
}

impl AudioPumpRouter {
    pub(super) fn new() -> Self {
        let (new_queues_tx, new_queues_rx) = mpsc::unbounded_channel();
        Self {
            queues: Mutex::new(HashMap::new()),
            dropped_by_device: Mutex::new(BTreeMap::new()),
            dropped_total: AtomicU64::new(0),
            new_queues_tx,
            new_queues_rx: Mutex::new(Some(new_queues_rx)),
        }
    }

    pub(super) fn take_queue_stream(
        &self,
    ) -> Option<mpsc::UnboundedReceiver<DeviceAudioQueueHandle>> {
        crate::lock_or_recover(&self.new_queues_rx, "hub.audio_pump_rx").take()
    }

    fn dropped_counter(&self, device: &str) -> Arc<AtomicU64> {
        let mut map = crate::lock_or_recover(&self.dropped_by_device, "hub.audio_dropped_by_dev");
        if map.len() >= MAX_TRACKED_AUDIO_DEVICES && !map.contains_key(device) {
            // Keep bounded without clearing active entries.
            if let Some(first_key) = map.keys().next().cloned() {
                map.remove(&first_key);
            }
        }
        map.entry(device.to_string())
            .or_insert_with(|| Arc::new(AtomicU64::new(0)))
            .clone()
    }

    /// Ensure a bounded queue exists for `device` (idempotent).
    /// `protected = true` (loopback speakers) pins the queue so unauthenticated
    /// LAN peers can never evict the local speaker's live audio pump.
    pub(super) fn ensure_device(&self, device: &str, protected: bool) {
        let mut queues = crate::lock_or_recover(&self.queues, "hub.audio_queues");
        if let Some(slot) = queues.get_mut(device) {
            if !slot.tx.is_closed() {
                slot.protected |= protected;
                return;
            }
        }
        self.create_queue_locked(&mut queues, device, protected);
    }

    fn create_queue_locked(
        &self,
        queues: &mut HashMap<String, DeviceQueueSlot>,
        device: &str,
        protected: bool,
    ) {
        if queues.len() >= MAX_TRACKED_AUDIO_DEVICES && !queues.contains_key(device) {
            queues.retain(|_, slot| {
                if slot.tx.is_closed() {
                    slot.sync.mark_closed();
                    false
                } else {
                    true
                }
            });
            if queues.len() >= MAX_TRACKED_AUDIO_DEVICES {
                // Never evict a protected (loopback) audio queue.
                let evict_key = queues
                    .iter()
                    .find(|(_, slot)| !slot.protected)
                    .map(|(k, _)| k.clone());
                if let Some(evict) = evict_key {
                    if let Some(old) = queues.remove(&evict) {
                        old.sync.mark_closed();
                    }
                } else if !protected {
                    return;
                }
            }
        }
        let (tx, rx) = mpsc::channel(DEVICE_AUDIO_QUEUE_CAP);
        let dropped = self.dropped_counter(device);
        let sync = DeviceAudioSync::new();
        if let Some(old) = queues.insert(
            device.to_string(),
            DeviceQueueSlot {
                tx,
                dropped,
                sync: sync.clone(),
                protected,
            },
        ) {
            old.sync.mark_closed();
        }
        let _ = self.new_queues_tx.send(DeviceAudioQueueHandle {
            device: device.to_string(),
            rx,
            sync,
        });
    }

    /// Enqueue one `AudioChunk` strictly onto `device`'s dedicated FIFO queue.
    pub(super) fn push(&self, device: &str, queued: QueuedDeviceEvent) {
        let seq = queued.seq;
        let mut queues = crate::lock_or_recover(&self.queues, "hub.audio_queues");
        if !queues.get(device).is_some_and(|s| !s.tx.is_closed()) {
            self.create_queue_locked(&mut queues, device, false);
        }
        if let Some(slot) = queues.get(device) {
            match slot.tx.try_send(queued) {
                Ok(()) => {
                    slot.sync.mark_enqueued(seq);
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    // Dropped frame never entered `rx`; leave `enqueued_seq` at the
                    // last frame actually queued so waiters never block on a drop.
                    let dev_drops = slot.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                    self.dropped_total.fetch_add(1, Ordering::Relaxed);
                    slot.sync.notify.notify_waiters();
                    if dev_drops == 1 || dev_drops % 32 == 0 {
                        tracing::warn!(
                            device = %device,
                            dropped = dev_drops,
                            cap = DEVICE_AUDIO_QUEUE_CAP,
                            "per-device audio queue full; dropped audio chunk"
                        );
                    }
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    slot.sync.mark_closed();
                    queues.remove(device);
                }
            }
        }
    }

    /// Close and remove `device`'s queue on disconnect so its worker task exits cleanly.
    pub(super) fn remove_device(&self, device: &str) {
        if let Some(slot) = crate::lock_or_recover(&self.queues, "hub.audio_queues").remove(device)
        {
            slot.sync.mark_closed();
        }
    }

    pub(super) fn latest_enqueued_seq(&self, device: &str) -> u64 {
        crate::lock_or_recover(&self.queues, "hub.audio_queues")
            .get(device)
            .map(|slot| slot.sync.enqueued_seq())
            .unwrap_or(0)
    }

    pub(super) fn sync_for(&self, device: &str) -> Option<Arc<DeviceAudioSync>> {
        crate::lock_or_recover(&self.queues, "hub.audio_queues")
            .get(device)
            .map(|slot| slot.sync.clone())
    }

    pub(super) fn dropped_total(&self) -> u64 {
        self.dropped_total.load(Ordering::Relaxed)
    }

    pub(super) fn dropped_by_device(&self) -> BTreeMap<String, u64> {
        crate::lock_or_recover(&self.dropped_by_device, "hub.audio_dropped_by_dev")
            .iter()
            .map(|(k, v)| (k.clone(), v.load(Ordering::Relaxed)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_loopback_queue_is_never_evicted_at_cap() {
        let router = AudioPumpRouter::new();
        let mut handles = router.take_queue_stream().unwrap();
        router.ensure_device("dot2-local", true);
        // Saturate beyond MAX_TRACKED_AUDIO_DEVICES (64) with unprotected LAN devices.
        for i in 0..(MAX_TRACKED_AUDIO_DEVICES + 16) {
            router.ensure_device(&format!("lan-spam-{i}"), false);
        }
        let mut kept = Vec::new();
        while let Ok(h) = handles.try_recv() {
            kept.push(h);
        }
        let queues = crate::lock_or_recover(&router.queues, "test.queues");
        assert!(
            queues.contains_key("dot2-local"),
            "protected loopback queue must never be evicted under LAN saturation"
        );
        assert!(
            queues.len() <= MAX_TRACKED_AUDIO_DEVICES,
            "queue map must stay bounded at MAX_TRACKED_AUDIO_DEVICES ({MAX_TRACKED_AUDIO_DEVICES}), got {}",
            queues.len()
        );
        drop(kept);
    }
}
