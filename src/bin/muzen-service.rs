use std::env;
use std::net::SocketAddr;

use anyhow::Result;
use clap::Parser;
use muzen::review_session::{
    Muzen, PostgresReviewSessionStore, PostgresWorkspaceProfileStore, ReviewHttpRouter,
    ReviewHttpRouterOptions,
};
use muzen::service::{serve, MuzenHttpService};
use std::sync::Arc;

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

    /// Environment variable containing the Postgres database URL.
    #[arg(long, default_value = "DATABASE_URL")]
    database_url_env: String,
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
    let router_options = ReviewHttpRouterOptions {
        github_webhook_secret: env::var(cli.github_webhook_secret_env).ok(),
        gitlab_webhook_secret: env::var(cli.gitlab_webhook_token_env).ok(),
    };
    let service = if let Ok(database_url) = env::var(cli.database_url_env) {
        let muzen = Muzen::with_stores(
            Arc::new(PostgresReviewSessionStore::connect(&database_url)?),
            Arc::new(PostgresWorkspaceProfileStore::connect(&database_url)?),
        );
        MuzenHttpService::new(ReviewHttpRouter::with_options(muzen, router_options))
    } else {
        MuzenHttpService::in_memory(router_options)
    };
    serve(cli.bind, service).await
}
