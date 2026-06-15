pub const RUNNER_PROTOCOL_VERSION: &str = "muzen.runner.v1";
pub const RUNNER_NAME: &str = "muzen-runner";

mod adapters;
mod cli;
mod execution;
mod planning;
mod protocol;
mod schema;
mod session;
mod stored;
mod transport;
mod types;
mod wiring;

pub use cli::{main_entry, run_main, RunnerCli, RunnerCommand, RunnerSchemaCommand};
pub(crate) use execution::execute_run_start;
pub use protocol::{
    JsonRpcError, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, RunnerErrorData,
};
pub use schema::{protocol_schema, runner_check, runner_handshake};
pub use session::{handle_jsonrpc_line, run_stdio, run_stdio_interactive};
pub(crate) use transport::RunnerCallbackTransport;
pub use types::*;

#[cfg(test)]
pub(crate) use session::RunnerStdioSession;

#[cfg(test)]
mod tests;
