//! The engine: per-session actor runner, magic-context (mctx), the system
//! prompt. The chat transport lives in `ox-llm`.
//!
//! Kept as one crate on purpose: runner ↔ mctx share the session/transcript
//! model so tightly that splitting them would buy a crate boundary and pay
//! for it with a cycle.

pub mod mctx;
pub mod prompt;
pub mod runner;
