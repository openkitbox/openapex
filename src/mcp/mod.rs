//! MCP client integration.

// The wire types are consumed by transport/client/stdio in later tasks of the
// ox-mcp plan; until then they are only exercised by their own tests.
#[allow(dead_code)]
pub(crate) mod client;
#[allow(dead_code)]
pub(crate) mod jsonrpc;
// stdio is wired into the tool adapter in later tasks of the ox-mcp plan;
// until then it is only exercised by its own tests.
#[allow(dead_code)]
pub(crate) mod stdio;
#[allow(dead_code)]
pub(crate) mod tool;
#[allow(dead_code)]
pub(crate) mod transport;
