use crate::db::schema_sql::{AUXILIARY_SCHEMA_STATEMENTS, CORE_SCHEMA_SQL, SYNTHEOS_SERVICES_SQL};
use crate::Result;

pub fn create_tables(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute_batch(CORE_SCHEMA_SQL)?;

    for statement in AUXILIARY_SCHEMA_STATEMENTS {
        conn.execute(statement, [])?;
    }

    conn.execute_batch(SYNTHEOS_SERVICES_SQL)?;

    // NOTE: LIBSQL_VECTOR_INDEX_STATEMENTS intentionally skipped.
    // libsql_vector_idx() is a libsql-only function that does not exist in
    // rusqlite/SQLCipher. Vector indexing now goes through LanceDB.

    Ok(())
}
