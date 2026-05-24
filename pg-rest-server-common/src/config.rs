use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::Path,
};

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct AppConfig {
    pub database: DatabaseConfig,
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(alias = "jwt")]
    pub auth: AuthConfig,
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
    pub host: IpAddr,
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

/// Authentication configuration — mutually exclusive variants enforced at parse time.
///
/// Both `[jwt]` and `[auth]` are valid TOML section names regardless of variant.
///
/// ## HMAC secret (or inline JWKS / URL-in-secret auto-detection)
/// ```toml
/// [jwt]
/// secret = "my-hmac-secret"
/// ```
///
/// ## Explicit JWKS URL
/// ```toml
/// [auth]
/// jwks_url = "https://example.com/.well-known/jwks.json"
/// ```
///
/// ## OIDC Token Introspection (RFC 7662)
/// ```toml
/// [auth]
/// introspection = { url = "...", client_id = "...", client_secret = "..." }
/// ```
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum AuthConfig {
    /// OIDC Token Introspection (RFC 7662).
    Introspection(IntrospectionAuth),
    /// Explicit JWKS URL.
    JwksUrl(JwksUrlAuth),
    /// HMAC secret (or inline JWKS / URL auto-detected from value).
    Secret(SecretAuth),
}

/// `introspection` field only — rejects any unknown fields.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntrospectionAuth {
    pub introspection: IntrospectionConfig,
}

/// `jwks_url` field only — rejects any unknown fields.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JwksUrlAuth {
    pub jwks_url: String,
}

/// `secret` field only — rejects any unknown fields.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretAuth {
    pub secret: String,
}

#[derive(Debug, Deserialize)]
pub struct IntrospectionConfig {
    pub url: String,
    pub client_id: String,
    pub client_secret: String,
    pub cache_ttl: Option<u64>,
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

fn default_host() -> IpAddr {
    Ipv4Addr::new(0, 0, 0, 0).into()
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

impl ServerConfig {
    pub fn bind_addr(&self) -> SocketAddr {
        (self.host, self.port).into()
    }
}
