use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use muzen::agent_runtime::{
    http, HttpServiceConfig, LocalRuntime, LocalRuntimeConfig, RuntimeTransport,
};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() {
    let result = async {
        let (listen, runtime_config, bearer_token) = parse_config()?;
        let listener = TcpListener::bind(listen)
            .await
            .map_err(|error| format!("failed to bind {listen}: {error}"))?;
        let runtime = LocalRuntime::connect(runtime_config)
            .await
            .map_err(|error| error.to_string())?;
        let inner: Arc<dyn RuntimeTransport> = Arc::new(runtime);
        http::serve(
            listener,
            inner,
            HttpServiceConfig {
                bearer_token,
                ..HttpServiceConfig::default()
            },
            ctrl_c(),
        )
        .await
        .map_err(|error| error.to_string())
    }
    .await;
    if let Err(error) = result {
        eprintln!("muzen-agent-service: {error}");
        eprintln!(
            "usage: muzen-agent-service --listen <addr:port> --store memory | --store sqlite --db <path> [--allow-loopback-http] [--bearer-token <token>]"
        );
        std::process::exit(2);
    }
}

async fn ctrl_c() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    static INTERRUPTED: AtomicBool = AtomicBool::new(false);
    extern "C" fn mark_interrupted(_: libc::c_int) {
        INTERRUPTED.store(true, Ordering::SeqCst);
    }
    // Tokio's signal feature is not enabled in this workspace. libc is an
    // existing dependency, so the binary installs only the one handler it
    // needs and lets the async runtime perform the graceful close.
    unsafe {
        libc::signal(libc::SIGINT, mark_interrupted as libc::sighandler_t);
    }
    while !INTERRUPTED.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn parse_config() -> Result<(SocketAddr, LocalRuntimeConfig, Option<String>), String> {
    let mut arguments = std::env::args().skip(1);
    let mut listen = None;
    let mut store = None;
    let mut database = None;
    let mut allow_loopback_http = false;
    let mut bearer_token = None;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--listen" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "--listen requires an address".to_owned())?;
                listen = Some(
                    value
                        .parse()
                        .map_err(|_| format!("invalid listen address: {value}"))?,
                );
            }
            "--store" => {
                store = Some(
                    arguments
                        .next()
                        .ok_or_else(|| "--store requires a value".to_owned())?,
                );
            }
            "--db" => {
                database = Some(PathBuf::from(
                    arguments
                        .next()
                        .ok_or_else(|| "--db requires a path".to_owned())?,
                ));
            }
            "--allow-loopback-http" => allow_loopback_http = true,
            "--bearer-token" => {
                bearer_token = Some(
                    arguments
                        .next()
                        .ok_or_else(|| "--bearer-token requires a token".to_owned())?,
                );
            }
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    let listen = listen.ok_or_else(|| "--listen is required".to_owned())?;
    let runtime = match store.as_deref() {
        Some("memory") => {
            if database.is_some() {
                return Err("--db is only valid with --store sqlite".to_owned());
            }
            LocalRuntimeConfig::memory_with_model_router()
        }
        Some("sqlite") => LocalRuntimeConfig::sqlite_with_model_router(
            database.ok_or_else(|| "--store sqlite requires --db <path>".to_owned())?,
        ),
        Some(value) => return Err(format!("unsupported store: {value}")),
        None => return Err("--store is required".to_owned()),
    }
    .with_loopback_http(allow_loopback_http);
    let bearer_token = bearer_token.or_else(|| std::env::var("MUZEN_BEARER_TOKEN").ok());
    Ok((listen, runtime, bearer_token))
}
