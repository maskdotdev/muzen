pub const RUNNER_PROTOCOL_VERSION: &str = "muzen.runner.v1";
pub const RUNNER_NAME: &str = "muzen-runner";

mod callback_model;
mod callback_tools;
mod callback_types;
mod cli;
mod context_session;
mod event_stream;
mod execution;
mod planning;
mod protocol;
mod schema;
mod session;
mod stored;
mod transport;
mod types;
mod wiring;

pub use cli::main_entry;
#[cfg(test)]
pub(crate) use protocol::JsonRpcResponse;
#[cfg(test)]
pub(crate) use schema::{protocol_schema, runner_handshake};
#[cfg(test)]
pub(crate) use session::{handle_jsonrpc_line, run_stdio, run_stdio_interactive};
pub(crate) use transport::RunnerCallbackTransport;

#[cfg(test)]
pub(crate) use session::RunnerStdioSession;

#[cfg(test)]
mod tests;
