//! One import path for the whole workspace: `ox::runner`, `ox::api`, …
//!
//! The implementation lives in the member crates (`ox-core`, `ox-tools`,
//! `ox-engine`, `ox-server`, …); this façade exists so integration tests and
//! the binary can keep naming the assembled system the way they always have.

pub use ox_acp as acp;
pub use ox_cbm as cbm;
pub use ox_core::{USER_AGENT, config, event_bus, model_catalog, projector};
pub use ox_db as db;
pub use ox_engine::{cardinal, mctx, prompt, runner};
pub use ox_mcp as mcp;
pub use ox_schema as schema;
pub use ox_schema::{id, slug};
pub use ox_server::{
    api, build_router, build_state, build_state_on_with_bus, build_state_with,
    build_state_with_home, compose, run, serve_on,
};
pub use ox_tools as tool;
