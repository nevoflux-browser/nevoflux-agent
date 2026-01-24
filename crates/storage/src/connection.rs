//! Database connection management.

use rusqlite::Connection;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::error::{Result, StorageError};
use crate::migrations;

/// Thread-safe database connection wrapper.
#[derive(Clone)]
pub struct Database {
    conn: Arc<Mutex<Connection>>,
}

impl Database {
    /// Open database at the given path, creating if necessary.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let conn = Connection::open(path)?;
        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        db.run_migrations()?;
        Ok(db)
    }

    /// Create an in-memory database for testing.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        db.run_migrations()?;
        Ok(db)
    }

    /// Execute a function with the connection.
    pub fn with_connection<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T>,
    {
        let conn = self
            .conn
            .lock()
            .map_err(|e| StorageError::Pool(format!("Failed to acquire connection lock: {}", e)))?;
        f(&conn)
    }

    /// Execute a function with mutable connection (for transactions).
    pub fn with_connection_mut<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T>,
    {
        let mut conn = self
            .conn
            .lock()
            .map_err(|e| StorageError::Pool(format!("Failed to acquire connection lock: {}", e)))?;
        f(&mut conn)
    }

    fn run_migrations(&self) -> Result<()> {
        self.with_connection_mut(migrations::run_all)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_open_in_memory() {
        let db = Database::open_in_memory();
        assert!(db.is_ok());
    }

    #[test]
    fn test_with_connection() {
        let db = Database::open_in_memory().unwrap();
        let result = db.with_connection(|conn| {
            let mut stmt = conn.prepare("SELECT 1")?;
            let val: i32 = stmt.query_row([], |row| row.get(0))?;
            Ok(val)
        });
        assert_eq!(result.unwrap(), 1);
    }

    #[test]
    fn test_tables_exist_after_migration() {
        let db = Database::open_in_memory().unwrap();
        let tables = db
            .with_connection(|conn| {
                let mut stmt = conn
                    .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")?;
                let names: Vec<String> = stmt
                    .query_map([], |row| row.get(0))?
                    .filter_map(|r| r.ok())
                    .collect();
                Ok(names)
            })
            .unwrap();

        assert!(tables.contains(&"sessions".to_string()));
        assert!(tables.contains(&"messages".to_string()));
        assert!(tables.contains(&"permissions".to_string()));
        assert!(tables.contains(&"config".to_string()));
    }
}
