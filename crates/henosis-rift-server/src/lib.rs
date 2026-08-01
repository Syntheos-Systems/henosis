/// Managed execution capability and reconciliation boundary.
pub mod agent_control;
pub mod auth;
/// Transactional creation of the managed Henosis room.
pub mod bootstrap;
pub mod config;
pub mod db;
pub mod error;
pub mod models;
pub mod routes;
pub mod runtime;
pub mod ws;

/// Database-free gate over the embedded migration set.
#[cfg(test)]
mod migrate_tests {
    /// The embedded migrator resolves `./migrations` at compile time and all
    /// six SQL files parse. This database-free gate proves that the
    /// migration set embedded by the binary is well-formed without
    /// standing up Postgres.
    #[test]
    fn migrations_embed_and_parse() {
        let migrator = sqlx::migrate!("./migrations");
        assert_eq!(
            migrator.migrations.len(),
            6,
            "expected the embedded rift migrations (001_initial, 002_bridge, \
             003_agent_support, 004_message_type_backfill, 005_server_bridge_state, \
             006_room_agent_configuration)"
        );
    }
}
