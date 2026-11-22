//! Per-unit acoustic profile and night LED / orientation configuration,
//! keyed by `stable_id`.
//!
//! Per `AGENTS.md`: hardware is one Dot2 model and firmware family, not
//! interchangeable acoustics. Volume cap, mix levels, and VAD thresholds are
//! measured per unit and are NEVER copied across units during fleet push or
//! config import.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::{append_history, ConfigError, ConfigService};
use crate::store::Result;

pub const UNIT_ACOUSTIC_PROFILES_KEY: &str = "unit_acoustic_profiles";
pub const DEVICE_LED_CONFIGS_KEY: &str = "device_led_configs";

fn default_volume_cap() -> f64 {
    1.0
}

fn default_mix_gain() -> f32 {
    1.0
}

fn default_vad_threshold() -> f32 {
    0.5
}

fn default_night_start_minute() -> u16 {
    1320 // 22:00
}

fn default_night_end_minute() -> u16 {
    420 // 07:00
}

fn default_night_brightness_pct() -> u8 {
    10
}

/// Per-unit acoustic calibration parameters. Measured per physical unit and
/// never copied across units by fleet push or household backup import.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UnitAcousticProfile {
    #[serde(default = "default_volume_cap")]
    pub volume_cap: f64,
    #[serde(default = "default_mix_gain")]
    pub mix_gain: f32,
    #[serde(default = "default_vad_threshold")]
    pub vad_threshold: f32,
    /// Canonical Gateway capture onset threshold in dBFS (`-120.0..=0.0`).
    /// When absent, runtime falls back to the node's calibrated `vad.start_db`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub onset_threshold_dbfs: Option<f32>,
    /// Per-unit microphone gain calibration offset (dB) applied to `rms_dbfs`
    /// during multi-speaker wake arbitration. Defaults to `0.0`.
    #[serde(default)]
    pub mic_gain_offset_db: f32,
    /// Whether `mic_gain_offset_db` has been field-calibrated for this unit.
    /// Defaults to `false` until measured and explicitly marked calibrated.
    #[serde(default)]
    pub mic_calibrated: bool,
}

impl Default for UnitAcousticProfile {
    fn default() -> Self {
        Self {
            volume_cap: default_volume_cap(),
            mix_gain: default_mix_gain(),
            vad_threshold: default_vad_threshold(),
            onset_threshold_dbfs: None,
            mic_gain_offset_db: 0.0,
            mic_calibrated: false,
        }
    }
}

impl UnitAcousticProfile {
    pub fn validate(&self) -> std::result::Result<(), ConfigError> {
        if !self.volume_cap.is_finite() || !(0.0..=1.0).contains(&self.volume_cap) {
            return Err(ConfigError::BadDeviceProfile(format!(
                "volume_cap must be in 0.0..=1.0, got {}",
                self.volume_cap
            )));
        }
        if !self.mix_gain.is_finite() || !(0.05..=2.0).contains(&self.mix_gain) {
            return Err(ConfigError::BadDeviceProfile(format!(
                "mix_gain must be in 0.05..=2.0, got {}",
                self.mix_gain
            )));
        }
        if !self.vad_threshold.is_finite() || !(0.05..=0.99).contains(&self.vad_threshold) {
            return Err(ConfigError::BadDeviceProfile(format!(
                "vad_threshold must be in 0.05..=0.99, got {}",
                self.vad_threshold
            )));
        }
        if let Some(onset) = self.onset_threshold_dbfs {
            if !onset.is_finite() || !(-120.0..=0.0).contains(&onset) {
                return Err(ConfigError::BadDeviceProfile(format!(
                    "onset_threshold_dbfs must be in -120.0..=0.0, got {onset}"
                )));
            }
        }
        if !self.mic_gain_offset_db.is_finite()
            || !(-24.0..=24.0).contains(&self.mic_gain_offset_db)
        {
            return Err(ConfigError::BadDeviceProfile(format!(
                "mic_gain_offset_db must be in -24.0..=24.0, got {}",
                self.mic_gain_offset_db
            )));
        }
        Ok(())
    }
}

/// Per-unit night-mode LED dimming schedule and physical ring orientation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeviceLedConfig {
    #[serde(default)]
    pub night_enabled: bool,
    #[serde(default = "default_night_start_minute")]
    pub night_start_minute: u16,
    #[serde(default = "default_night_end_minute")]
    pub night_end_minute: u16,
    #[serde(default = "default_night_brightness_pct")]
    pub night_brightness_pct: u8,
    #[serde(default)]
    pub orientation_rotation: u8,
    #[serde(default)]
    pub orientation_reverse: bool,
}

impl Default for DeviceLedConfig {
    fn default() -> Self {
        Self {
            night_enabled: false,
            night_start_minute: default_night_start_minute(),
            night_end_minute: default_night_end_minute(),
            night_brightness_pct: default_night_brightness_pct(),
            orientation_rotation: 0,
            orientation_reverse: false,
        }
    }
}

impl DeviceLedConfig {
    pub fn validate(&self) -> std::result::Result<(), ConfigError> {
        if self.night_start_minute >= 1440 {
            return Err(ConfigError::BadDeviceProfile(format!(
                "night_start_minute must be 0..1440, got {}",
                self.night_start_minute
            )));
        }
        if self.night_end_minute >= 1440 {
            return Err(ConfigError::BadDeviceProfile(format!(
                "night_end_minute must be 0..1440, got {}",
                self.night_end_minute
            )));
        }
        if self.night_brightness_pct > 100 {
            return Err(ConfigError::BadDeviceProfile(format!(
                "night_brightness_pct must be 0..=100, got {}",
                self.night_brightness_pct
            )));
        }
        if self.orientation_rotation >= 12 {
            return Err(ConfigError::BadDeviceProfile(format!(
                "orientation_rotation must be 0..12 LED steps, got {}",
                self.orientation_rotation
            )));
        }
        Ok(())
    }
}

fn validate_device_id(device: &str) -> std::result::Result<&str, ConfigError> {
    let device = device.trim();
    if device.is_empty() || device.chars().count() > 64 || device.chars().any(char::is_control) {
        return Err(ConfigError::BadDeviceProfile(
            "device id must be 1–64 visible characters".into(),
        ));
    }
    Ok(device)
}

impl ConfigService {
    /// All per-unit acoustic profiles keyed by `stable_id`.
    pub fn unit_acoustic_profiles(&self) -> Result<BTreeMap<String, UnitAcousticProfile>> {
        match self.store.config_get(UNIT_ACOUSTIC_PROFILES_KEY)? {
            Some(raw) => Ok(serde_json::from_str(&raw)?),
            None => Ok(Default::default()),
        }
    }

    /// Acoustic profile for a single `stable_id` (or defaults if not yet customized).
    pub fn unit_acoustic_profile(&self, device: &str) -> Result<UnitAcousticProfile> {
        let all = self.unit_acoustic_profiles()?;
        Ok(all.get(device.trim()).cloned().unwrap_or_default())
    }

    /// Save one unit's acoustic profile.
    pub fn save_unit_acoustic_profile(
        &self,
        device: &str,
        profile: UnitAcousticProfile,
        actor: &str,
    ) -> std::result::Result<UnitAcousticProfile, ConfigError> {
        let device = validate_device_id(device)?;
        profile.validate()?;
        let mut all = self.unit_acoustic_profiles()?;
        all.insert(device.to_string(), profile.clone());
        self.store
            .config_set(UNIT_ACOUSTIC_PROFILES_KEY, &serde_json::to_string(&all)?)?;
        append_history(
            &self.store,
            actor,
            "device_profile",
            &format!(
                "{device}: volume_cap={:.2} mix_gain={:.2} vad_threshold={:.2}",
                profile.volume_cap, profile.mix_gain, profile.vad_threshold
            ),
            &serde_json::json!({
                "device": device,
                "acoustic": profile,
            }),
        )?;
        Ok(profile)
    }

    /// All per-unit LED configs keyed by `stable_id`.
    pub fn device_led_configs(&self) -> Result<BTreeMap<String, DeviceLedConfig>> {
        match self.store.config_get(DEVICE_LED_CONFIGS_KEY)? {
            Some(raw) => Ok(serde_json::from_str(&raw)?),
            None => Ok(Default::default()),
        }
    }

    /// LED config for a single `stable_id` (or defaults if not yet customized).
    pub fn device_led_config(&self, device: &str) -> Result<DeviceLedConfig> {
        let all = self.device_led_configs()?;
        Ok(all.get(device.trim()).cloned().unwrap_or_default())
    }

    /// Save one unit's LED config.
    pub fn save_device_led_config(
        &self,
        device: &str,
        cfg: DeviceLedConfig,
        actor: &str,
    ) -> std::result::Result<DeviceLedConfig, ConfigError> {
        let device = validate_device_id(device)?;
        cfg.validate()?;
        let mut all = self.device_led_configs()?;
        all.insert(device.to_string(), cfg.clone());
        self.store
            .config_set(DEVICE_LED_CONFIGS_KEY, &serde_json::to_string(&all)?)?;
        append_history(
            &self.store,
            actor,
            "device_led",
            &format!(
                "{device}: night={} ({}..{}, {}%) rot={} rev={}",
                cfg.night_enabled,
                cfg.night_start_minute,
                cfg.night_end_minute,
                cfg.night_brightness_pct,
                cfg.orientation_rotation,
                cfg.orientation_reverse
            ),
            &serde_json::json!({
                "device": device,
                "led": cfg,
            }),
        )?;
        Ok(cfg)
    }
}
