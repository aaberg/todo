mod api;
mod auth;
mod config;
mod db;
mod oidc;

use std::sync::Arc;

use axum::{
    routing::{get, post},
    Router,
};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use auth::AppState;
use config::Config;
use db::RelayDb;
use oidc::OidcClient;


/// Parse CLI args. Currently supports `--debug` to enable verbose debug logging.
fn parse_debug_flag() -> bool {
    std::env::args().any(|arg| arg == "--debug")
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "todo_relay=debug,tower_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let config = Config::from_env(parse_debug_flag())?;
    tracing::info!(
        bind = %config.bind,
        public_url = %config.public_url,
        oidc_issuer = %config.oidc_issuer,
        debug_mode = config.debug,
        "starting todo-relay"
    );
    // OIDC discovery — fails fast if the provider is unreachable
    let oidc = OidcClient::discover(
        &config.oidc_issuer,
        &config.oidc_client_id,
        &config.oidc_client_secret,
        &config.redirect_uri(),
        config.debug,
    )
    .await?;
    tracing::info!("OIDC discovery complete");

    let relay_db = RelayDb::open(&config.database_path)?;

    let state = AppState {
        db: Arc::new(std::sync::Mutex::new(relay_db)),
        oidc: Arc::new(oidc),
        config: Arc::new(config.clone()),
        pending_logins: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
    };

    // Public routes — no auth required
    let public = Router::new()
        .route("/health", get(api::health))
        .route("/auth/login", get(api::auth_login))
        .route("/auth/callback", get(api::auth_callback))
        .route("/auth/logout", post(api::logout))
        .with_state(state.clone());

    // Protected routes — Bearer token required
    let protected = Router::new()
        .route("/push", post(api::push))
        .route("/pull", get(api::pull))
        .route("/me", get(api::me))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ))
        .with_state(state.clone());

    let app = public.merge(protected);

    let listener = tokio::net::TcpListener::bind(&config.bind).await?;
    tracing::info!(bind = %config.bind, "listening");
    axum::serve(listener, app).await?;

    Ok(())
}
