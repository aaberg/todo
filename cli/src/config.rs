use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// Base URL of the sync relay, e.g. "https://relay.aaberg.cc"
    #[serde(default)]
    pub sync_url: Option<String>,
    /// Session token from the relay (Bearer token)
    #[serde(default)]
    pub session_token: Option<String>,
    /// This device's UUID — generated on first use
    #[serde(default)]
    pub device_id: Option<String>,
}

impl Config {
    pub fn path() -> PathBuf {
        let home = std::env::var_os("HOME").unwrap_or_else(|| std::ffi::OsString::from("."));
        PathBuf::from(home).join(".todo").join("config.toml")
    }

    pub fn load() -> Self {
        let path = Self::path();
        let Ok(content) = fs::read_to_string(&path) else {
            return Self::default();
        };
        toml::from_str(&content).unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), ConfigError> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(ConfigError::Io)?;
        }
        let content = toml::to_string_pretty(self).map_err(ConfigError::Serialize)?;
        fs::write(&path, content).map_err(ConfigError::Io)?;
        // Restrict permissions — contains a session token
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
                .map_err(ConfigError::Io)?;
        }
        Ok(())
    }

    pub fn is_logged_in(&self) -> bool {
        self.session_token.is_some() && self.sync_url.is_some()
    }

    pub fn require_auth(&self) -> Result<(&str, &str), ConfigError> {
        match (&self.sync_url, &self.session_token) {
            (Some(url), Some(token)) => Ok((url, token)),
            _ => Err(ConfigError::NotLoggedIn),
        }
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Serialize(toml::ser::Error),
    NotLoggedIn,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "config I/O error: {e}"),
            ConfigError::Serialize(e) => write!(f, "config serialize error: {e}"),
            ConfigError::NotLoggedIn => write!(
                f,
                "not logged in — run `todo login` first"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Ensure a device_id exists in the config. Returns the device_id.
pub fn ensure_device_id(config: &mut Config) -> String {
    if config.device_id.is_none() {
        config.device_id = Some(uuid::Uuid::new_v4().to_string());
    }
    config.device_id.clone().unwrap()
}

#[allow(dead_code)]
fn config_path_for_test() -> PathBuf {
    Path::new("/tmp/todo-test-config.toml").to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrip() {
        let config = Config {
            sync_url: Some("https://relay.example.com".to_string()),
            session_token: Some("test-token".to_string()),
            device_id: Some("device-uuid".to_string()),
        };
        let serialized = toml::to_string_pretty(&config).unwrap();
        let deserialized: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(deserialized.sync_url, config.sync_url);
        assert_eq!(deserialized.session_token, config.session_token);
        assert_eq!(deserialized.device_id, config.device_id);
    }

    #[test]
    fn default_config_is_not_logged_in() {
        let config = Config::default();
        assert!(!config.is_logged_in());
        assert!(config.require_auth().is_err());
    }

    #[test]
    fn logged_in_config_has_auth() {
        let config = Config {
            sync_url: Some("https://relay.example.com".to_string()),
            session_token: Some("token".to_string()),
            device_id: None,
        };
        assert!(config.is_logged_in());
        assert!(config.require_auth().is_ok());
    }

    #[test]
    fn ensure_device_id_generates_and_persists() {
        let mut config = Config::default();
        let id1 = ensure_device_id(&mut config);
        let id2 = ensure_device_id(&mut config);
        assert_eq!(id1, id2);
        assert!(!id1.is_empty());
    }
}
