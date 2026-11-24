//! Automation domain model and durable storage.
//!
//! The model is the wire contract shared by tools, HTTP and the scheduler; the
//! store owns its SQLite mapping and compare-and-swap guards.

pub mod action;
pub mod cron;
pub mod decision;
pub mod lease;
pub mod scheduler;
pub mod store;
pub mod types;
