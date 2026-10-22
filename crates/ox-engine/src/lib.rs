//! The engine: per-session actor runner, magic-context (mctx), the system
//! prompt and the cardinal chat transport.
//!
//! Kept as one crate on purpose: runner ↔ mctx share the session/transcript
//! model so tightly that splitting them would buy a crate boundary and pay
//! for it with a cycle.

pub mod cardinal;
pub mod mctx;
pub mod prompt;
pub mod runner;
