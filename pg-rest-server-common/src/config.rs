use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct AppConfig {
    pub database: DatabaseConfig,
    #[serde(default)]
    pub server: ServerConfig,
    pub jwt: JwtConfig,
}

#[derive(Debug, Deserialize)]
pub struct DatabaseConfig {
    pub uri: String,
    pub schemas: Vec<String>,
    pub anon_role: String,
    #[serde(default = "default_pool_size")]
    pub pool_size: usize,
    /// Set to false for PgBouncer transaction-mode compatibility.
    #[serde(default = "default_true")]
    pub prepared_statements: bool,
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// "text" (default) or "json" for structured JSON logging.
    #[serde(default = "default_log_format")]
    pub log_format: String,
    /// CORS allowed origins. Empty or ["*"] = permissive. Otherwise, list of origins.
    #[serde(default)]
    pub cors_origins: Vec<String>,
    /// Maximum request body size in bytes. Default 1 MiB.
    #[serde(default = "default_body_limit")]
    pub body_limit: usize,
    /// Requests per second per IP (0 = unlimited). Default 0.
    #[serde(default)]
    pub rate_limit: u64,
}

#[derive(Debug, Deserialize)]
pub struct JwtConfig {
    pub secret: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            log_format: default_log_format(),
            cors_origins: Vec::new(),
            body_limit: default_body_limit(),
            rate_limit: 0,
        }
    }
}

fn default_host() -> String {
    "0.0.0.0".to_string()
}
fn default_port() -> u16 {
    3000
}
fn default_pool_size() -> usize {
    10
}
fn default_true() -> bool {
    true
}
fn default_log_format() -> String {
    "text".to_string()
}
fn default_body_limit() -> usize {
    1024 * 1024 // 1 MiB
}

impl AppConfig {
    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let content = std::fs::read_to_string(path)?;
        let config: AppConfig = toml::from_str(&content)?;
        Ok(config)
    }
}
