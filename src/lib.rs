// V1 contracts intentionally include protocol states not all exercised by the MVP benchmark.
pub mod cli;
pub mod context_engine;
pub mod review_session;
pub mod reviewer;
pub mod runner;
pub mod service;

pub(crate) mod bench;
pub(crate) mod contracts;
pub(crate) mod job;
pub(crate) mod repo;
pub(crate) mod runtime;
pub(crate) mod util;

#[cfg(test)]
mod tests;
