use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use muzen::remote_http::{
    serve, ContextHttpRouterOptions, MuzenHttpService, ReviewHttpRouter, ReviewHttpRouterOptions,
};
use muzen::review_sessions::{Muzen, DEFAULT_MUZEN_STORE_URL, MUZEN_STORE_URL_ENV};

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

    /// Store URL. Supported schemes: sqlite:// and memory://.
    #[arg(long)]
    store_url: Option<String>,

    /// Environment variable containing the Muzen store URL.
    #[arg(long, default_value = MUZEN_STORE_URL_ENV)]
    store_url_env: String,

    /// Environment variable containing the context learning store root directory.
    #[arg(long, default_value = "MUZEN_CONTEXT_LEARNING_STORE_ROOT")]
    context_learning_store_root_env: String,

    /// Environment variable containing the context derived-data cache root directory.
    #[arg(long, default_value = "MUZEN_CONTEXT_DERIVED_CACHE_ROOT")]
    context_derived_cache_root_env: String,
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
        context: ContextHttpRouterOptions {
            learning_store_root: env::var(cli.context_learning_store_root_env)
                .ok()
                .map(PathBuf::from),
            derived_cache_root: env::var(cli.context_derived_cache_root_env)
                .ok()
                .map(PathBuf::from),
        },
    };
    let store_url = cli
        .store_url
        .or_else(|| env::var(cli.store_url_env).ok())
        .unwrap_or_else(|| DEFAULT_MUZEN_STORE_URL.to_string());
    let muzen = Muzen::from_store_url(&store_url).await?;
    let service = MuzenHttpService::new(ReviewHttpRouter::with_options(muzen, router_options));
    serve(cli.bind, service).await
}
