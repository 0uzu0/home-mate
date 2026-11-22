//! Bounded server-owned destructive OpenAPI authority; never model-deserializable.
pub mod api;
pub(crate) mod migration;
mod service;
pub use service::ConfirmationService;
pub(crate) mod store;
/// Unique single-use dispatch ownership. Only this service can mint a permit.
pub(crate) struct DispatchPermit {
    _sealed: (),
}
