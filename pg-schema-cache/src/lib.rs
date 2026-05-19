mod error;
#[cfg(any(feature = "resolute", feature = "tokio-postgres"))]
mod introspection_shared;

pub use error::SchemaCacheError;
pub use pg_schema_cache_types::*;

#[cfg(feature = "resolute")]
mod introspection_resolute;
#[cfg(feature = "resolute")]
mod listener_resolute;

#[cfg(feature = "tokio-postgres")]
mod introspection_tp;
#[cfg(feature = "tokio-postgres")]
mod listener_tp;

#[cfg(feature = "resolute")]
pub mod resolute {
    pub use crate::introspection_resolute::build_schema_cache;
    pub use crate::listener_resolute::start_schema_listener;
}

#[cfg(feature = "tokio-postgres")]
pub mod tokio_postgres {
    pub use crate::introspection_tp::build_schema_cache;
    pub use crate::listener_tp::start_schema_listener;
}
