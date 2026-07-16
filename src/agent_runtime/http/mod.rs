//! HTTP/SSE adapter for the Agent Runtime Interface.
//!
//! The local v1 has one global optional bearer token. Tenant identity and
//! multi-tenant resource scoping are deliberately outside this adapter.

mod client;
mod server;

pub use client::{HttpTransport, HttpTransportOptions};
pub use server::{router, serve, HttpServiceConfig};

#[cfg(test)]
mod tests;
