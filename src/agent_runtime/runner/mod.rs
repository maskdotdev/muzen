mod client;
mod server;
mod wire;

pub use client::{spawn_local_runner, RunnerChild};
pub use server::serve_stdio;

pub(crate) use client::RunnerTransport;

#[cfg(test)]
mod tests;
