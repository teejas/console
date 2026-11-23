//! Wire types shared by every console component.
//!
//! - [`ids`]: typed, prefixed ULID identifiers.
//! - [`domain`]: task states, queue bands, disclosure levels, and other shared enums.
//! - [`event`]: the durable event log's record type.
//! - [`api`]: the Orchestrator's client API (JSON-RPC methods).
//! - [`link`]: the executor link messages.
//!
//! Wire shapes are pinned by snapshot tests in `tests/wire.rs`. A snapshot change is a
//! protocol change: bump [`api::API_VERSION`] or [`link::LINK_PROTOCOL_VERSION`] when
//! it is incompatible.

pub mod api;
pub mod domain;
pub mod event;
pub mod ids;
pub mod link;

pub use ids::*;
