//! SQLite metadata ownership. Features own SQL and their domain transactions.
//!
//! All callbacks are synchronous and must contain only short metadata work. Async
//! callers use `spawn_blocking`; a callback must never perform network or process
//! work, await, or call this store recursively. SQLite execution and lock waits
//! are bounded independently. The application supplies an isolated, admitted
//! metadata path; payload paths and file authority never belong in this owner.

use std::{
    collections::HashSet,
    path::Path,
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub use rusqlite;
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior};

pub type StorageResult<T> = Result<T, StorageError>;

/// Feature-owned migration identifiers are stable and globally unique. The
/// composition owner supplies migrations in dependency order. Applied SQL is
/// preserved verbatim so editing an already applied migration is refused.
#[derive(Clone, Copy, Debug)]
pub struct Migration {
    pub id: &'static str,
    pub sql: &'static str,
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("metadata is corrupt; the existing files were preserved")]
    Corrupt,
    #[error("metadata is busy")]
    Busy,
    #[error("metadata execution exceeded its time or instruction budget")]
    BudgetExceeded,
    #[error("metadata mutation is closed after an unresolved storage failure")]
    Closed,
    #[error("metadata owner was interrupted during a transaction")]
    Poisoned,
    #[error("invalid or changed metadata migration: {0}")]
    InvalidMigration(String),
    #[error("metadata database belongs to another application")]
    WrongApplication,
    #[error("metadata callback left an unowned transaction")]
    UnownedTransaction,
    #[error("invalid metadata execution limits")]
    InvalidLimits,
    #[error("metadata database failed: {0}")]
    Sqlite(rusqlite::Error),
}

impl From<rusqlite::Error> for StorageError {
    fn from(error: rusqlite::Error) -> Self {
        match error.sqlite_error_code() {
            Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase) => {
                Self::Corrupt
            }
            Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) => {
                Self::Busy
            }
            Some(rusqlite::ErrorCode::OperationInterrupted) => Self::BudgetExceeded,
            _ => Self::Sqlite(error),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct StorageLimits {
    pub busy_timeout: Duration,
    pub execution_timeout: Duration,
    pub max_vm_instructions: u64,
}

impl Default for StorageLimits {
    fn default() -> Self {
        Self {
            busy_timeout: Duration::from_millis(750),
            execution_timeout: Duration::from_secs(5),
            max_vm_instructions: 100_000_000,
        }
    }
}

/// A single connection deliberately serializes short feature transactions. It
/// is not an application state container or a generic feature repository.
#[derive(Debug)]
pub struct MetadataStore {
    connection: Mutex<Connection>,
    mutation_closed: AtomicBool,
    limits: StorageLimits,
}

const APPLICATION_ID: i64 = 0x4158_4952; // AXIR, distinct replacement profile.
const MIGRATION_TABLE: &str = "CREATE TABLE IF NOT EXISTS _axial_schema_migrations (
    id TEXT PRIMARY KEY NOT NULL CHECK(length(id) BETWEEN 1 AND 128),
    sql TEXT NOT NULL CHECK(length(sql) BETWEEN 1 AND 1048576)
) STRICT;";

impl MetadataStore {
    /// The parent directory must already be admitted and present. This method
    /// never creates payload directories, recreates corrupt data, or follows a
    /// symbolic link in place of the metadata file.
    pub fn open(path: impl AsRef<Path>) -> StorageResult<Self> {
        Self::open_with_limits(path, StorageLimits::default())
    }

    pub fn open_with_limits(path: impl AsRef<Path>, limits: StorageLimits) -> StorageResult<Self> {
        Self::validate_limits(limits)?;
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let connection = Connection::open_with_flags(path, flags)?;
        Self::initialize(connection, limits, false)
    }

    pub fn in_memory() -> StorageResult<Self> {
        Self::in_memory_with_limits(StorageLimits::default())
    }

    pub fn in_memory_with_limits(limits: StorageLimits) -> StorageResult<Self> {
        Self::validate_limits(limits)?;
        Self::initialize(Connection::open_in_memory()?, limits, true)
    }

    fn validate_limits(limits: StorageLimits) -> StorageResult<()> {
        if limits.busy_timeout > Duration::from_secs(30)
            || limits.execution_timeout.is_zero()
            || limits.execution_timeout > Duration::from_secs(30)
            || limits.max_vm_instructions < 1_000
        {
            return Err(StorageError::InvalidLimits);
        }
        Ok(())
    }

    fn initialize(
        connection: Connection,
        limits: StorageLimits,
        memory: bool,
    ) -> StorageResult<Self> {
        connection.busy_timeout(limits.busy_timeout)?;
        let budget = ExecutionBudget::new(&connection, limits);
        // Validate before any pragma that could persistently alter an existing
        // database. Corruption is an error, never a cue to rename/delete/rebuild.
        verify_integrity(&connection)?;
        let application_id: i64 =
            connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
        if application_id != 0 && application_id != APPLICATION_ID {
            return Err(StorageError::WrongApplication);
        }
        if application_id == 0 {
            let user_objects: i64 = connection.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get(0),
            )?;
            if user_objects != 0 {
                return Err(StorageError::WrongApplication);
            }
        }
        connection.pragma_update(None, "foreign_keys", true)?;
        connection.pragma_update(None, "trusted_schema", false)?;
        connection.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
        // Metadata stays in the isolated application root. DELETE journaling
        // avoids assuming WAL/shared-memory support on all admitted filesystems.
        let mode: String = connection.pragma_update_and_check(
            None,
            "journal_mode",
            if memory { "MEMORY" } else { "DELETE" },
            |row| row.get(0),
        )?;
        if !mode.eq_ignore_ascii_case(if memory { "memory" } else { "delete" }) {
            return Err(StorageError::Sqlite(rusqlite::Error::InvalidQuery));
        }
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "application_id", APPLICATION_ID)?;
        drop(budget);
        Ok(Self {
            connection: Mutex::new(connection),
            mutation_closed: AtomicBool::new(false),
            limits,
        })
    }

    pub fn is_mutation_closed(&self) -> bool {
        self.mutation_closed.load(Ordering::Acquire)
    }

    fn lock(&self) -> StorageResult<MutexGuard<'_, Connection>> {
        self.connection.lock().map_err(|_| {
            self.mutation_closed.store(true, Ordering::Release);
            StorageError::Poisoned
        })
    }

    /// Runs a query-only callback. Domain errors remain typed. A read-only
    /// snapshot transaction must finish inside the callback; transactions may
    /// not escape and connection configuration must not change.
    pub fn read<T, E>(&self, read: impl FnOnce(&Connection) -> Result<T, E>) -> Result<T, E>
    where
        E: From<StorageError>,
    {
        let connection = self.lock().map_err(E::from)?;
        let budget = ExecutionBudget::new(&connection, self.limits);
        connection
            .pragma_update(None, "query_only", true)
            .map_err(StorageError::from)
            .map_err(E::from)?;
        let result = read(&connection);
        drop(budget);
        if !connection.is_autocommit() {
            let _ = connection.execute_batch("ROLLBACK");
            self.mutation_closed.store(true, Ordering::Release);
            let _ = connection.pragma_update(None, "query_only", false);
            return Err(StorageError::UnownedTransaction.into());
        }
        if let Err(error) = connection.pragma_update(None, "query_only", false) {
            self.mutation_closed.store(true, Ordering::Release);
            return Err(StorageError::from(error).into());
        }
        if result.is_err() {
            self.check_after_failed_callback(&connection);
        }
        result
    }

    /// Executes one immediate transaction and commits only an `Ok` result.
    /// Returning a feature error always rolls back. The lifetime of the supplied
    /// transaction cannot escape the callback into an asynchronous operation.
    pub fn transaction<T, E>(
        &self,
        write: impl FnOnce(&Transaction<'_>) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<StorageError>,
    {
        let connection = self.lock().map_err(E::from)?;
        if self.is_mutation_closed() {
            return Err(StorageError::Closed.into());
        }
        let budget = ExecutionBudget::new(&connection, self.limits);
        // The budget only borrows the connection for its Drop cleanup; use an
        // unchecked_transaction with explicit IMMEDIATE behavior under our
        // exclusive connection mutex to avoid overlapping mutable borrows.
        let transaction =
            match Transaction::new_unchecked(&connection, TransactionBehavior::Immediate) {
                Ok(transaction) => transaction,
                Err(error) => {
                    let error = StorageError::from(error);
                    if matches!(error, StorageError::Corrupt) {
                        self.mutation_closed.store(true, Ordering::Release);
                    }
                    return Err(error.into());
                }
            };
        let result = match write(&transaction) {
            Ok(value) => transaction
                .commit()
                .map(|()| value)
                .map_err(StorageError::from)
                .map_err(E::from),
            Err(error) => {
                if transaction.rollback().is_err() {
                    self.mutation_closed.store(true, Ordering::Release);
                }
                Err(error)
            }
        };
        drop(budget);
        if result.is_err() {
            self.check_after_failed_callback(&connection);
        }
        // Make accidental transaction ownership changes a closed-mutation
        // state even when SQLite's transaction destructor has settled them.
        if !connection.is_autocommit() {
            let _ = connection.execute_batch("ROLLBACK");
            self.mutation_closed.store(true, Ordering::Release);
            return Err(StorageError::UnownedTransaction.into());
        }
        result
    }

    /// Applies a complete feature migration group atomically. Calls from feature
    /// constructors are idempotent; global dependency order belongs to composition.
    pub fn migrate(&self, migrations: &[Migration]) -> StorageResult<()> {
        let mut ids = HashSet::new();
        for migration in migrations {
            if migration.id.is_empty()
                || migration.id.len() > 128
                || !migration
                    .id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
                || migration.sql.is_empty()
                || migration.sql.len() > 1_048_576
                || !ids.insert(migration.id)
            {
                return Err(StorageError::InvalidMigration(migration.id.to_owned()));
            }
        }
        self.transaction(|transaction| -> StorageResult<()> {
            transaction.execute_batch(MIGRATION_TABLE)?;
            for migration in migrations {
                let applied: Option<String> = transaction
                    .query_row(
                        "SELECT sql FROM _axial_schema_migrations WHERE id = ?1",
                        [migration.id],
                        |row| row.get(0),
                    )
                    .optional()?;
                match applied {
                    Some(sql) if sql == migration.sql => continue,
                    Some(_) => return Err(StorageError::InvalidMigration(migration.id.to_owned())),
                    None => {}
                }
                transaction.execute_batch(migration.sql)?;
                transaction.execute(
                    "INSERT INTO _axial_schema_migrations (id, sql) VALUES (?1, ?2)",
                    (migration.id, migration.sql),
                )?;
            }
            Ok(())
        })
    }

    /// Diagnostic check does not repair or silently reopen mutation admission.
    pub fn check_integrity(&self) -> StorageResult<()> {
        let connection = self.lock()?;
        let _budget = ExecutionBudget::new(&connection, self.limits);
        let result = verify_integrity(&connection);
        if result.is_err() {
            self.mutation_closed.store(true, Ordering::Release);
        }
        result
    }

    fn check_after_failed_callback(&self, connection: &Connection) {
        // A feature error may wrap SQLite's corruption error. Checking here
        // permits typed feature errors without losing storage's fail-closed rule.
        let _budget = ExecutionBudget::new(connection, self.limits);
        if verify_integrity(connection).is_err() {
            self.mutation_closed.store(true, Ordering::Release);
        }
    }
}

fn verify_integrity(connection: &Connection) -> StorageResult<()> {
    let status: String = connection.query_row("PRAGMA quick_check(1)", [], |row| row.get(0))?;
    if status != "ok" {
        return Err(StorageError::Corrupt);
    }
    Ok(())
}

struct ExecutionBudget<'a>(&'a Connection);

impl<'a> ExecutionBudget<'a> {
    fn new(connection: &'a Connection, limits: StorageLimits) -> Self {
        let started = Instant::now();
        let mut instructions = 0_u64;
        let _ = connection.progress_handler(
            1_000,
            Some(move || {
                instructions = instructions.saturating_add(1_000);
                instructions > limits.max_vm_instructions
                    || started.elapsed() >= limits.execution_timeout
            }),
        );
        Self(connection)
    }
}

impl Drop for ExecutionBudget<'_> {
    fn drop(&mut self) {
        let _ = self.0.progress_handler(0, None::<fn() -> bool>);
    }
}

#[cfg(test)]
mod tests;
