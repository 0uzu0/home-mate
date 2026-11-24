//! Versioned reviewed configuration delivery and supervised persona publication.
pub mod api;
mod discovery;
pub mod fleet;
mod gateway_dispatch;
mod home;
pub mod inventory;
mod local_services;
pub mod migration;
mod mode;
mod model;
mod music;
mod open_sockets;
mod openapi;
mod profile;
mod readback;
mod service;
pub mod v1;
mod weather;
mod worker;

pub(crate) use gateway_dispatch::{
    dispatch_audio_eq_v2, dispatch_duplex_v2, dispatch_reflex_v2, dispatch_unit_acoustics,
    dispatch_unit_acoustics_parts,
};
pub(crate) use home::HomeSnapshot;
pub use model::*;
pub(crate) use music::MusicSnapshot;
pub(crate) use openapi::OpenApiSnapshot;
pub(crate) use service::portable_service_url;
pub use service::ConfigDelivery;
pub(crate) use weather::WeatherSnapshot;
pub use worker::ConfigApplyCoordinator;
pub use worker::ConfigApplyCoordinator as ConfigWorker;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod v1_export_tests;
