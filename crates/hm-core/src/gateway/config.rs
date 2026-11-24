//! Gateway v2 configuration protocol (`config_apply_v2`) waiters, capability
//! gating, fencing state, and frame dispatch on `DeviceHub`.

use super::{
    ConfigBlock, ConfigBlockState, ConfigReceiptStatus, CoreFrame, DeviceEvent, DeviceHub,
    GatewayFrame, HubState, MAX_CONFIG_FRAME_BYTES,
};
use crate::id;
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

pub(super) struct ConfigOneshotWaiter {
    pub(super) sender: oneshot::Sender<GatewayFrame>,
    pub(super) generation: u64,
}

pub(super) struct ConfigApplyWaiter {
    pub(super) sender: mpsc::UnboundedSender<GatewayFrame>,
    pub(super) generation: u64,
}

#[derive(Debug, Clone)]
pub struct ConfigValidateRequest {
    pub device: String,
    pub expected_gateway_boot_id: Option<String>,
    pub expected_state_revision: Option<u64>,
    pub block: ConfigBlock,
    pub schema_version: u32,
    pub block_hash: String,
    pub values: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct ConfigApplyRequest {
    pub device: String,
    pub core_node_id: String,
    pub data_epoch: u64,
    pub application_id: String,
    pub operation_id: String,
    pub desired_revision: u64,
    pub block_generation: u64,
    pub block: ConfigBlock,
    pub schema_version: u32,
    pub block_hash: String,
    pub expected_gateway_boot_id: Option<String>,
    pub expected_state_revision: Option<u64>,
    pub capability_fingerprint: Option<String>,
    pub values: serde_json::Value,
}

fn check_frame_size(frame: &CoreFrame) -> Result<(), String> {
    let encoded = serde_json::to_vec(frame).map_err(|e| format!("encode config frame: {e}"))?;
    if encoded.len() > MAX_CONFIG_FRAME_BYTES {
        return Err(format!(
            "config frame exceeds MAX_CONFIG_FRAME_BYTES ({} > {MAX_CONFIG_FRAME_BYTES})",
            encoded.len()
        ));
    }
    Ok(())
}

impl DeviceHub {
    /// Verify that the device's live gateway negotiated protocol >= 2, the
    /// target device is online, and its descriptor advertises both
    /// `config_apply_v2` and `config_block_<block>`.
    pub fn supports_config_apply_v2(&self, device: &str, block: ConfigBlock) -> Result<(), String> {
        let st = self.lock_state();
        let proto = st
            .device_to_conn
            .get(device)
            .and_then(|cid| st.connections.get(cid))
            .map(|c| c.protocol_operating)
            .or(st.protocol_operating)
            .unwrap_or(0);
        if proto < 2 {
            return Err(format!(
                "gateway negotiated protocol {proto} < 2; config_apply_v2 unavailable"
            ));
        }
        let slot = st
            .devices
            .get(device)
            .ok_or_else(|| format!("device '{device}' is not registered"))?;
        if !slot.online {
            return Err(format!("device '{device}' is offline"));
        }
        if !slot
            .descriptor
            .capabilities
            .iter()
            .any(|c| c == "config_apply_v2")
        {
            return Err(format!(
                "device '{device}' lacks 'config_apply_v2' capability"
            ));
        }
        let block_cap = block.capability_name();
        if !slot.descriptor.capabilities.iter().any(|c| c == block_cap) {
            return Err(format!("device '{device}' lacks '{block_cap}' capability"));
        }
        Ok(())
    }

    /// Verify protocol >= 2 and `config_apply_v2` capability for `config_state_get`.
    pub fn supports_config_state_get(&self, device: &str) -> Result<(), String> {
        let st = self.lock_state();
        let proto = st
            .device_to_conn
            .get(device)
            .and_then(|cid| st.connections.get(cid))
            .map(|c| c.protocol_operating)
            .or(st.protocol_operating)
            .unwrap_or(0);
        if proto < 2 {
            return Err(format!(
                "gateway negotiated protocol {proto} < 2; config_apply_v2 unavailable"
            ));
        }
        let slot = st
            .devices
            .get(device)
            .ok_or_else(|| format!("device '{device}' is not registered"))?;
        if !slot.online {
            return Err(format!("device '{device}' is offline"));
        }
        if !slot
            .descriptor
            .capabilities
            .iter()
            .any(|c| c == "config_apply_v2")
        {
            return Err(format!(
                "device '{device}' lacks 'config_apply_v2' capability"
            ));
        }
        Ok(())
    }

    /// Return `(gateway_boot_id, state_revision, capability_fingerprint)` last
    /// observed for `device`.
    pub fn device_config_fencing(
        &self,
        device: &str,
    ) -> Option<(Option<String>, Option<u64>, Option<String>)> {
        let st = self.lock_state();
        st.devices.get(device).map(|d| {
            (
                d.gateway_boot_id.clone(),
                d.state_revision,
                d.capability_fingerprint.clone(),
            )
        })
    }

    /// Return a snapshot of the last reported `ConfigBlockState`s for `device`.
    pub fn device_config_blocks(&self, device: &str) -> HashMap<ConfigBlock, ConfigBlockState> {
        let st = self.lock_state();
        st.devices
            .get(device)
            .map(|d| d.config_blocks.clone())
            .unwrap_or_default()
    }

    pub async fn config_state_get(
        &self,
        device: &str,
        timeout: Duration,
    ) -> Result<GatewayFrame, String> {
        self.supports_config_state_get(device)?;
        let request_id = id::next("CFGSTATE");
        let (tx, rx) = oneshot::channel();
        {
            let mut st = self.lock_state();
            let gen = Self::device_generation(&st, device);
            st.config_state_waiters.insert(
                request_id.clone(),
                ConfigOneshotWaiter {
                    sender: tx,
                    generation: gen,
                },
            );
        }
        let frame = CoreFrame::ConfigStateGet {
            request_id: request_id.clone(),
            device: device.to_string(),
        };
        if let Err(e) = self.send_to_device(device, frame) {
            self.lock_state().config_state_waiters.remove(&request_id);
            return Err(e);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(frame)) => Ok(frame),
            Ok(Err(_)) => Err("config_state_get waiter retired on link drop".into()),
            Err(_) => {
                self.lock_state().config_state_waiters.remove(&request_id);
                Err(format!(
                    "config_state_get timed out after {}ms",
                    timeout.as_millis()
                ))
            }
        }
    }

    pub async fn config_validate(
        &self,
        req: ConfigValidateRequest,
        timeout: Duration,
    ) -> Result<GatewayFrame, String> {
        self.supports_config_apply_v2(&req.device, req.block)?;
        let device = req.device.clone();
        let request_id = id::next("CFGVAL");
        let frame = CoreFrame::ConfigValidate {
            request_id: request_id.clone(),
            device: req.device,
            expected_gateway_boot_id: req.expected_gateway_boot_id,
            expected_state_revision: req.expected_state_revision,
            block: req.block,
            schema_version: req.schema_version,
            block_hash: req.block_hash,
            values: req.values,
        };
        check_frame_size(&frame)?;
        let (tx, rx) = oneshot::channel();
        {
            let mut st = self.lock_state();
            let gen = Self::device_generation(&st, &device);
            st.config_validate_waiters.insert(
                request_id.clone(),
                ConfigOneshotWaiter {
                    sender: tx,
                    generation: gen,
                },
            );
        }
        if let Err(e) = self.send_to_device(&device, frame) {
            self.lock_state()
                .config_validate_waiters
                .remove(&request_id);
            return Err(e);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(frame)) => Ok(frame),
            Ok(Err(_)) => Err("config_validate waiter retired on link drop".into()),
            Err(_) => {
                self.lock_state()
                    .config_validate_waiters
                    .remove(&request_id);
                Err(format!(
                    "config_validate timed out after {}ms",
                    timeout.as_millis()
                ))
            }
        }
    }

    pub fn config_apply(
        &self,
        req: ConfigApplyRequest,
    ) -> Result<mpsc::UnboundedReceiver<GatewayFrame>, String> {
        self.supports_config_apply_v2(&req.device, req.block)?;
        let device = req.device.clone();
        let operation_id = req.operation_id.clone();
        let frame = CoreFrame::ConfigApply {
            device: req.device,
            core_node_id: req.core_node_id,
            data_epoch: req.data_epoch,
            application_id: req.application_id,
            operation_id: operation_id.clone(),
            desired_revision: req.desired_revision,
            block_generation: req.block_generation,
            block: req.block,
            schema_version: req.schema_version,
            block_hash: req.block_hash,
            expected_gateway_boot_id: req.expected_gateway_boot_id,
            expected_state_revision: req.expected_state_revision,
            capability_fingerprint: req.capability_fingerprint,
            values: req.values,
        };
        check_frame_size(&frame)?;
        let (tx, rx) = mpsc::unbounded_channel();
        {
            let mut st = self.lock_state();
            let gen = Self::device_generation(&st, &device);
            st.config_apply_waiters.insert(
                operation_id.clone(),
                ConfigApplyWaiter {
                    sender: tx,
                    generation: gen,
                },
            );
        }
        if let Err(e) = self.send_to_device(&device, frame) {
            self.lock_state().config_apply_waiters.remove(&operation_id);
            return Err(e);
        }
        Ok(rx)
    }

    pub fn remove_config_apply_waiter(&self, operation_id: &str) {
        self.lock_state().config_apply_waiters.remove(operation_id);
    }

    pub(super) fn handle_config_state(
        &self,
        st: &mut HubState,
        generation: u64,
        frame: GatewayFrame,
    ) {
        let GatewayFrame::ConfigState {
            ref request_id,
            ref device,
            ref gateway_boot_id,
            state_revision,
            ref capability_fingerprint,
            ref blocks,
            ts,
        } = frame
        else {
            return;
        };
        if st.device_frame_stale(generation, device) {
            st.record(
                "stale_event",
                json!({ "device": device, "kind": "config_state" }),
            );
            return;
        }
        if let Some(slot) = st.devices.get_mut(device) {
            if gateway_boot_id.is_some() {
                slot.gateway_boot_id = gateway_boot_id.clone();
            }
            if state_revision.is_some() {
                slot.state_revision = state_revision;
            }
            if capability_fingerprint.is_some() {
                slot.capability_fingerprint = capability_fingerprint.clone();
            }
            for b in blocks {
                slot.config_blocks.insert(b.block, b.clone());
            }
        }
        st.record(
            "config_state",
            json!({
                "device": device,
                "gateway_boot_id": gateway_boot_id,
                "state_revision": state_revision,
                "blocks": blocks.len(),
            }),
        );
        if let Some(req_id) = request_id {
            if st
                .config_state_waiters
                .get(req_id)
                .is_some_and(|w| w.generation == generation)
            {
                if let Some(waiter) = st.config_state_waiters.remove(req_id) {
                    let _ = waiter.sender.send(frame.clone());
                }
            }
        }
        self.emit_control(DeviceEvent::ConfigState {
            device: device.clone(),
            gateway_boot_id: gateway_boot_id.clone(),
            state_revision,
            blocks: blocks.clone(),
            ts,
        });
    }

    pub(super) fn handle_config_validated(
        &self,
        st: &mut HubState,
        generation: u64,
        frame: GatewayFrame,
    ) {
        let GatewayFrame::ConfigValidated {
            ref request_id,
            ref device,
            block,
            ok,
            supported,
            ref blockers,
            state,
            ref gateway_boot_id,
            state_revision,
            ref capability_fingerprint,
            ..
        } = frame
        else {
            return;
        };
        if st.device_frame_stale(generation, device) {
            st.record(
                "stale_event",
                json!({ "device": device, "kind": "config_validated" }),
            );
            return;
        }
        if let Some(slot) = st.devices.get_mut(device) {
            if gateway_boot_id.is_some() {
                slot.gateway_boot_id = gateway_boot_id.clone();
            }
            if state_revision.is_some() {
                slot.state_revision = state_revision;
            }
            if capability_fingerprint.is_some() {
                slot.capability_fingerprint = capability_fingerprint.clone();
            }
        }
        let effective_ok = (ok || supported)
            && blockers.is_empty()
            && !matches!(
                state,
                ConfigReceiptStatus::Failed
                    | ConfigReceiptStatus::Unsupported
                    | ConfigReceiptStatus::Superseded
            );
        st.record(
            "config_validated",
            json!({
                "device": device,
                "request_id": request_id,
                "block": block,
                "ok": effective_ok,
                "supported": supported,
                "blockers": blockers,
                "state": state,
            }),
        );
        if st
            .config_validate_waiters
            .get(request_id)
            .is_some_and(|w| w.generation == generation)
        {
            if let Some(waiter) = st.config_validate_waiters.remove(request_id) {
                let _ = waiter.sender.send(frame);
            }
        }
    }

    pub(super) fn handle_config_receipt(
        &self,
        st: &mut HubState,
        generation: u64,
        frame: GatewayFrame,
    ) {
        let GatewayFrame::ConfigReceipt {
            ref device,
            data_epoch,
            ref application_id,
            ref operation_id,
            block_generation,
            block,
            state,
            ref reason,
            retryable,
            ref gateway_boot_id,
            state_revision,
            ref persisted_hash,
            ref active_hash,
            ref active_values,
            ref evidence,
            ts,
            ..
        } = frame
        else {
            return;
        };
        if st.device_frame_stale(generation, device) {
            st.record(
                "stale_event",
                json!({ "device": device, "kind": "config_receipt" }),
            );
            return;
        }
        if let Some(slot) = st.devices.get_mut(device) {
            if gateway_boot_id.is_some() {
                slot.gateway_boot_id = gateway_boot_id.clone();
            }
            if state_revision.is_some() {
                slot.state_revision = state_revision;
            }
            slot.config_blocks.insert(
                block,
                ConfigBlockState {
                    block,
                    block_generation,
                    operation_id: operation_id.clone(),
                    data_epoch,
                    persisted_hash: persisted_hash.clone(),
                    active_hash: active_hash.clone(),
                    active_values: active_values.clone(),
                    evidence: evidence.clone(),
                    state,
                    reason: reason.clone(),
                },
            );
        }
        st.record(
            "config_receipt",
            json!({
                "device": device,
                "application_id": application_id,
                "operation_id": operation_id,
                "block": block,
                "state": state,
                "reason": reason,
            }),
        );
        if let Some(waiter) = st
            .config_apply_waiters
            .get(operation_id)
            .filter(|w| w.generation == generation)
        {
            let _ = waiter.sender.send(frame.clone());
            if state.is_terminal() {
                st.config_apply_waiters.remove(operation_id);
            }
        }
        self.emit_control(DeviceEvent::ConfigReceipt {
            device: device.clone(),
            application_id: application_id.clone(),
            operation_id: operation_id.clone(),
            block,
            block_generation,
            state,
            reason: reason.clone(),
            retryable,
            persisted_hash: persisted_hash.clone(),
            active_hash: active_hash.clone(),
            active_values: active_values.clone(),
            evidence: evidence.clone(),
            ts,
        });
    }
}
