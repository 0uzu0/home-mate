//! O(1) lifetime gauges for actual transport tasks/futures and bounded outbound queues.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

pub const OUTBOUND_CONTROL_CAP: usize = 256;
pub const OUTBOUND_VOICE_CAP: usize = 256;
pub const OUTBOUND_PLAY_CAP: usize = 640;

#[derive(Default)]
pub(super) struct OutboundMetrics {
    pub control_high_water: AtomicUsize,
    pub voice_high_water: AtomicUsize,
    pub play_high_water: AtomicUsize,
    pub control_dropped: AtomicUsize,
    pub voice_dropped: AtomicUsize,
    pub play_dropped: AtomicUsize,
}

impl OutboundMetrics {
    pub fn observe_control_depth(&self, depth: usize) {
        self.control_high_water.fetch_max(depth, Ordering::SeqCst);
    }

    pub fn observe_voice_depth(&self, depth: usize) {
        self.voice_high_water.fetch_max(depth, Ordering::SeqCst);
    }

    pub fn observe_play_depth(&self, depth: usize) {
        self.play_high_water.fetch_max(depth, Ordering::SeqCst);
    }
}

#[derive(Default)]
pub(super) struct TransportTasks {
    pub connections: Arc<AtomicUsize>,
    pub readers: Arc<AtomicUsize>,
    pub writers: Arc<AtomicUsize>,
    pub outbound: Arc<OutboundMetrics>,
}

pub(super) struct LiveTask(Arc<AtomicUsize>);
impl LiveTask {
    pub fn enter(counter: &Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(counter.clone())
    }
}
impl Drop for LiveTask {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl TransportTasks {
    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "connection_tasks": self.connections.load(Ordering::SeqCst),
            "reader_futures": self.readers.load(Ordering::SeqCst),
            "writer_tasks": self.writers.load(Ordering::SeqCst),
            "outbound_control_high_water": self.outbound.control_high_water.load(Ordering::SeqCst),
            "outbound_voice_high_water": self.outbound.voice_high_water.load(Ordering::SeqCst),
            "outbound_play_high_water": self.outbound.play_high_water.load(Ordering::SeqCst),
            "outbound_control_dropped": self.outbound.control_dropped.load(Ordering::SeqCst),
            "outbound_voice_dropped": self.outbound.voice_dropped.load(Ordering::SeqCst),
            "outbound_play_dropped": self.outbound.play_dropped.load(Ordering::SeqCst),
        })
    }
}
