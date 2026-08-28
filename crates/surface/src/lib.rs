//! The agent-facing surfaces: thin shells over the atelier core, never a
//! second behavior (ADR-0006). One dispatch serves every transport: MCP
//! over stdio, MCP over streamable HTTP, and plain REST.

mod http;
mod mcp;

pub use http::{serve_http, serve_http_until};
pub use mcp::serve_stdio;
