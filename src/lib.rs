pub mod config;
pub mod context;
pub mod crypto;
pub mod envelope;
pub mod hash;
pub mod heklang_host;
pub mod http;
pub mod invariant;
pub mod lock;
pub mod opdb;
pub mod read_api;
pub mod read_model;
pub mod schema;
pub mod tags;
pub mod ui;

pub mod cli;
pub mod dispatch;
pub mod effect;
pub mod introspect;
pub mod loader;
pub mod openapi;
pub mod plan;
pub mod projector;
pub mod runtime;
pub mod server;
pub mod testing;
pub mod validate;
pub mod verify;

/// Generators for the conversion-table properties. Not compiled into the library: the
/// tables they exercise are private to their own modules, so the properties live beside
/// them rather than reaching in from `tests/`.
#[cfg(test)]
mod propgen;
