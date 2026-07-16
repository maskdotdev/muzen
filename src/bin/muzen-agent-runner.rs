use std::path::PathBuf;

use muzen::agent_runtime::{runner, LocalRuntimeConfig};

#[tokio::main]
async fn main() {
    match parse_config() {
        Ok(config) => {
            if let Err(error) = runner::serve_stdio(config).await {
                eprintln!("muzen-agent-runner: {error}");
                std::process::exit(1);
            }
        }
        Err(error) => {
            eprintln!("muzen-agent-runner: {error}");
            eprintln!(
                "usage: muzen-agent-runner --store memory | --store sqlite --db <path> [--allow-loopback-http]"
            );
            std::process::exit(2);
        }
    }
}

fn parse_config() -> Result<LocalRuntimeConfig, String> {
    let mut arguments = std::env::args().skip(1);
    let mut store = None;
    let mut database = None;
    let mut allow_loopback_http = false;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
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
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    let config = match store.as_deref() {
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
    };
    Ok(config.with_loopback_http(allow_loopback_http))
}
