pub mod auth;
pub mod config;
pub mod db;
pub mod error;
pub mod models;
pub mod routes;
pub mod ws;

#[cfg(test)]
mod migrate_tests {
    /// The embedded migrator resolves `./migrations` at compile time and all
    /// three SQL files parse. This is a database-free gate: it proves the
    /// migration set the binary self-applies on boot is well-formed, without
    /// standing up Postgres. Replaces the vacuous zero-test acceptance the
    /// standalone crate shipped (rift-absorption playbook, step 1).
    #[test]
    fn migrations_embed_and_parse() {
        let migrator = sqlx::migrate!("./migrations");
        assert_eq!(
            migrator.migrations.len(),
            3,
            "expected the three absorbed rift migrations (001_initial, 002_bridge, 003_agent_support)"
        );
    }
}
