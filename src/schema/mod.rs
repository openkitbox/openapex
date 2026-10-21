//! Compatibility shim: the wire contracts now live in the `ox-schema` crate.
//! Existing `crate::schema::event::…` paths keep working through this re-export.

pub use ox_schema::{event, inbox, message, support};
