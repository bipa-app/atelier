//! The agent-facing surfaces: thin shells over the atelier core, never a
//! second behavior (ADR-0006). v1 serves MCP over stdio; the HTTP
//! transports are a later slice.

mod mcp;

pub use mcp::serve_stdio;
