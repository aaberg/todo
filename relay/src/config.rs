use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: String,
    pub public_url: String,
    pub database_path: String,
    pub oidc_issuer: String,
    pub oidc_client_id: String,
    pub oidc_client_secret: String,
    pub allowed_groups: Vec<String>,
    pub session_ttl_secs: i64,
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        Ok(Self {
            bind: env::var("TODO_RELAY_BIND")
                .unwrap_or_else(|_| "127.0.0.1:3000".to_string()),
            public_url: required("TODO_RELAY_PUBLIC_URL")?,
            database_path: env::var("TODO_RELAY_DATABASE")
                .unwrap_or_else(|_| "relay.db".to_string()),
            oidc_issuer: required("TODO_OIDC_ISSUER")?,
            oidc_client_id: required("TODO_OIDC_CLIENT_ID")?,
            oidc_client_secret: required("TODO_OIDC_CLIENT_SECRET")?,
            allowed_groups: env::var("TODO_ALLOWED_GROUPS")
                .unwrap_or_else(|_| "todo-users".to_string())
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            session_ttl_secs: env::var("TODO_SESSION_TTL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(30 * 24 * 3600),
        })
    }

    pub fn redirect_uri(&self) -> String {
        format!("{}/auth/callback", self.public_url)
    }
}

fn required(key: &str) -> Result<String, ConfigError> {
    env::var(key).map_err(|_| ConfigError::Missing(key.to_string()))
}

#[derive(Debug)]
pub enum ConfigError {
    Missing(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Missing(key) => write!(f, "missing required env var: {key}"),
        }
    }
}

impl std::error::Error for ConfigError {}
