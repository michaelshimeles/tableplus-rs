//! Core of tableplus-rs: database drivers, SQL generation, parsing helpers and persistence.
//! Everything here is UI-agnostic and covered by unit and integration tests.

pub mod alter;
pub mod completion;
pub mod config;
pub mod dialect;
pub mod driver;
pub mod export;
pub mod fuzzy;
pub mod model;
pub mod plan;
pub mod secrets;
pub mod server;
pub mod sqlgen;
pub mod sqltext;
pub mod store;
pub mod tunnel;
pub mod value;

pub use config::{ConnectionConfig, Credentials, DriverKind};
pub use dialect::Dialect;
pub use driver::{Connection, DynConnection};
pub use model::*;
pub use server::{Flavor, ServerInfo, Version};
pub use value::Value;
