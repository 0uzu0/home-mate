//! M2 integration: a fake gateway (raw TCP speaking protocol v0) drives the
//! REAL core — hub freshness, voice pipeline turns, honest receipts, mode
//! negotiation and stop semantics, over the actual management API where the
//! console would talk.

use futures::StreamExt;
use hm_core::api::AppState;
use hm_core::gateway::{CoreFrame, GatewayFrame, HelloDevice};
use hm_core::llm::fake::{FakeLlm, Scripted};
use hm_core::speech::MockTts;
use hm_core::tool::mocks::MockInject;
use hm_core::tool::{ToolCtx, ToolStatus};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};

#[path = "../common/mod.rs"]
mod common;

mod helpers;
use helpers::*;

mod barge_in_and_cues;
mod d0_d1;
mod d1_5;
mod d2;
mod d3;
mod delivery_and_reflex;
mod execution_scope;
mod follow_window;
mod mode_and_diag;
mod northbound_transport;
mod playback_and_delivery;
mod reflex_and_media;
mod reviewed_voice_policy;
mod turn_and_stream;

mod context_ttl;

mod media_lifecycle;

mod ack_followup;
