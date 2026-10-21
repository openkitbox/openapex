//! MCP client integration.

// The wire types are consumed by transport/client/stdio in later tasks of the
// ox-mcp plan; until then they are only exercised by their own tests.
#[allow(dead_code)]
pub(crate) mod jsonrpc;
