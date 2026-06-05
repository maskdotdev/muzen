use std::env;
use std::net::SocketAddr;

use anyhow::Result;
use clap::Parser;
use muzen::review_session::ReviewHttpRouterOptions;
use muzen::service::{serve, MuzenHttpService};

#[derive(Debug, Parser)]
#[command(name = "muzen-service")]
#[command(about = "Muzen RFC 0001 HTTP service host")]
struct ServiceCli {
    /// Address to bind, for example 127.0.0.1:7341.
    #[arg(long, default_value = "127.0.0.1:7341")]
    bind: SocketAddr,

    /// Environment variable containing the GitHub webhook HMAC secret.
    #[arg(long, default_value = "GITHUB_WEBHOOK_SECRET")]
    github_webhook_secret_env: String,

    /// Environment variable containing the GitLab webhook token.
    #[arg(long, default_value = "GITLAB_WEBHOOK_TOKEN")]
    gitlab_webhook_token_env: String,
}

#[tokio::main]
async fn main() {
    let code = match run().await {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("{error:#}");
            4
        }
    };
    std::process::exit(code);
}

async fn run() -> Result<()> {
    let cli = ServiceCli::parse();
    let service = MuzenHttpService::in_memory(ReviewHttpRouterOptions {
        github_webhook_secret: env::var(cli.github_webhook_secret_env).ok(),
        gitlab_webhook_secret: env::var(cli.gitlab_webhook_token_env).ok(),
    });
    serve(cli.bind, service).await
}
