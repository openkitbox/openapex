//! Pure permission rule evaluation and tool-call extraction.
//!
//! No I/O, database access, or client interaction lives here. The gate and
//! saved-permission storage are owned by later tasks.

pub use engine::evaluate;
pub use extract::{Extraction, extract, extract_mcp};
pub use shell::{ShellCommands, analyze};

pub mod engine;
pub mod extract;
pub mod gate;
pub mod shell;
pub mod store;

pub use gate::{Decision, PermissionService, ReplyError, SourceRef};
