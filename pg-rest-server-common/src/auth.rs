use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Debug, Clone)]
pub struct JwtClaims {
    pub role: String,
    /// Raw JSON string of all claims, forwarded to PostgreSQL as a GUC.
    pub raw: String,
}

/// LRU-style JWT cache: token string → validated claims.
/// Avoids redundant HMAC-SHA256 for repeated tokens.
pub struct JwtCache {
    entries: Mutex<HashMap<u64, JwtClaims>>,
}

impl Default for JwtCache {
    fn default() -> Self {
        Self::new()
    }
}

impl JwtCache {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::with_capacity(256)),
        }
    }

    fn hash_token(token: &str) -> u64 {
        let mut hash: u64 = 0xcbf29ce484222325;
        for byte in token.bytes() {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash
    }

    pub fn get(&self, token: &str) -> Option<JwtClaims> {
        let key = Self::hash_token(token);
        let cache = self.entries.lock().unwrap();
        cache.get(&key).cloned()
    }

    pub fn insert(&self, token: &str, claims: JwtClaims) {
        let key = Self::hash_token(token);
        let mut cache = self.entries.lock().unwrap();
        if cache.len() >= 1024 {
            cache.clear();
        }
        cache.insert(key, claims);
    }
}
