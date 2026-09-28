use super::*;

fn physical_temporary_directory() -> tempfile::TempDir {
    let parent = std::fs::canonicalize(std::env::temp_dir()).unwrap();
    tempfile::tempdir_in(parent).unwrap()
}

const FIXTURE: Migration = Migration {
    id: "storage.test.v1",
    sql: "CREATE TABLE parents (id INTEGER PRIMARY KEY); CREATE TABLE children (id INTEGER PRIMARY KEY, parent INTEGER NOT NULL REFERENCES parents(id));",
};

#[test]
fn metadata_persists_and_preserves_foreign_key_constraints() {
    let directory = physical_temporary_directory();
    let path = directory.path().join("metadata.sqlite3");
    {
        let store = MetadataStore::open(&path).unwrap();
        store.migrate(&[FIXTURE]).unwrap();
        store
            .transaction(|tx| -> StorageResult<()> {
                tx.execute("INSERT INTO parents VALUES (1)", [])?;
                tx.execute("INSERT INTO children VALUES (2, 1)", [])?;
                Ok(())
            })
            .unwrap();
        assert!(
            store
                .transaction(|tx| -> StorageResult<()> {
                    tx.execute("INSERT INTO children VALUES (3, 999)", [])?;
                    Ok(())
                })
                .is_err()
        );
    }
    let reopened = MetadataStore::open(&path).unwrap();
    reopened.migrate(&[FIXTURE]).unwrap();
    let value: i64 = reopened
        .read(|conn| -> StorageResult<i64> {
            Ok(conn.query_row("SELECT count(*) FROM children", [], |row| row.get(0))?)
        })
        .unwrap();
    assert_eq!(value, 1);
}

#[test]
fn typed_feature_failure_rolls_back_all_effects() {
    #[derive(Debug)]
    #[allow(dead_code)]
    enum FeatureError {
        Conflict,
        Storage(StorageError),
    }
    impl From<StorageError> for FeatureError {
        fn from(error: StorageError) -> Self {
            Self::Storage(error)
        }
    }
    let store = MetadataStore::in_memory().unwrap();
    store.migrate(&[FIXTURE]).unwrap();
    let result = store.transaction(|tx| -> Result<(), FeatureError> {
        tx.execute("INSERT INTO parents VALUES (1)", [])
            .map_err(StorageError::from)?;
        Err(FeatureError::Conflict)
    });
    assert!(matches!(result, Err(FeatureError::Conflict)));
    let count = store
        .read(|conn| -> StorageResult<i64> {
            Ok(conn.query_row("SELECT count(*) FROM parents", [], |row| row.get(0))?)
        })
        .unwrap();
    assert_eq!(count, 0);
    assert!(!store.is_mutation_closed());
}

#[test]
fn migration_batch_is_atomic_and_changed_sql_is_rejected() {
    let store = MetadataStore::in_memory().unwrap();
    let invalid = Migration {
        id: "broken.v1",
        sql: "THIS IS NOT SQL",
    };
    assert!(store.migrate(&[FIXTURE, invalid]).is_err());
    let count = store
        .read(|conn| -> StorageResult<i64> {
            Ok(conn.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name = 'parents'",
                [],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(count, 0);
    store.migrate(&[FIXTURE]).unwrap();
    let changed = Migration {
        id: FIXTURE.id,
        sql: "CREATE TABLE different (id INTEGER);",
    };
    assert!(matches!(
        store.migrate(&[changed]),
        Err(StorageError::InvalidMigration(_))
    ));
    store.migrate(&[FIXTURE]).unwrap();
}

#[test]
fn duplicate_migration_identifiers_are_rejected_before_sql() {
    let store = MetadataStore::in_memory().unwrap();
    assert!(matches!(
        store.migrate(&[FIXTURE, FIXTURE]),
        Err(StorageError::InvalidMigration(_))
    ));
}

#[test]
fn reads_cannot_mutate_metadata() {
    let store = MetadataStore::in_memory().unwrap();
    store.migrate(&[FIXTURE]).unwrap();
    assert!(
        store
            .read(|conn| -> StorageResult<()> {
                conn.execute("INSERT INTO parents VALUES (1)", [])?;
                Ok(())
            })
            .is_err()
    );
    // The failed read resets query-only state, so the real transaction can write.
    store
        .transaction(|tx| -> StorageResult<()> {
            tx.execute("INSERT INTO parents VALUES (1)", [])?;
            Ok(())
        })
        .unwrap();
}

#[test]
fn invalid_database_bytes_are_preserved() {
    let directory = physical_temporary_directory();
    let path = directory.path().join("metadata.sqlite3");
    let original = b"not a sqlite database: user data must remain unchanged";
    std::fs::write(&path, original).unwrap();
    assert!(matches!(
        MetadataStore::open(&path),
        Err(StorageError::Corrupt)
    ));
    assert_eq!(std::fs::read(&path).unwrap(), original);
}

#[test]
fn another_application_database_is_preserved() {
    let directory = physical_temporary_directory();
    let path = directory.path().join("metadata.sqlite3");
    {
        let connection = Connection::open(&path).unwrap();
        connection
            .pragma_update(None, "application_id", 123)
            .unwrap();
        connection
            .execute_batch(
                "CREATE TABLE valuable (value TEXT); INSERT INTO valuable VALUES ('retained');",
            )
            .unwrap();
    }
    let original = std::fs::read(&path).unwrap();
    assert!(matches!(
        MetadataStore::open(&path),
        Err(StorageError::WrongApplication)
    ));
    assert_eq!(std::fs::read(&path).unwrap(), original);
}

#[test]
fn unmarked_nonempty_database_is_not_adopted() {
    let directory = physical_temporary_directory();
    let path = directory.path().join("unmarked.sqlite3");
    {
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("CREATE TABLE user_data (value TEXT); INSERT INTO user_data VALUES ('preserved');").unwrap();
    }
    let original = std::fs::read(&path).unwrap();
    assert!(matches!(MetadataStore::open(&path), Err(StorageError::WrongApplication)));
    assert_eq!(std::fs::read(&path).unwrap(), original);
}

#[test]
fn costly_sql_has_a_bounded_execution_budget() {
    let store = MetadataStore::in_memory_with_limits(StorageLimits {
        max_vm_instructions: 10_000,
        ..StorageLimits::default()
    })
    .unwrap();
    let result = store.read(|conn| -> StorageResult<i64> {
        Ok(conn.query_row("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<100000000) SELECT sum(x) FROM n", [], |row| row.get(0))?)
    });
    assert!(matches!(result, Err(StorageError::BudgetExceeded)));
    assert!(!store.is_mutation_closed());
}

#[test]
fn independent_connection_lock_wait_is_bounded() {
    let directory = physical_temporary_directory();
    let path = directory.path().join("metadata.sqlite3");
    let store = MetadataStore::open_with_limits(
        &path,
        StorageLimits {
            busy_timeout: Duration::from_millis(20),
            ..StorageLimits::default()
        },
    )
    .unwrap();
    store.migrate(&[FIXTURE]).unwrap();
    let external = Connection::open(&path).unwrap();
    external.execute_batch("BEGIN IMMEDIATE;").unwrap();
    let started = Instant::now();
    let result = store.transaction(|_| -> StorageResult<()> { Ok(()) });
    assert!(matches!(result, Err(StorageError::Busy)));
    assert!(started.elapsed() < Duration::from_secs(2));
    external.execute_batch("ROLLBACK;").unwrap();
}

#[test]
fn interrupted_callback_rolls_back_and_poison_closes_owner() {
    let store = MetadataStore::in_memory().unwrap();
    store.migrate(&[FIXTURE]).unwrap();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _: StorageResult<()> = store.transaction(|tx| {
            tx.execute("INSERT INTO parents VALUES (1)", [])?;
            panic!("simulated process callback interruption");
        });
    }));
    assert!(panic.is_err());
    assert!(matches!(
        store.transaction(|_| -> StorageResult<()> { Ok(()) }),
        Err(StorageError::Poisoned)
    ));
    assert!(store.is_mutation_closed());
    let connection = store.connection.lock().unwrap_err().into_inner();
    let count: i64 = connection
        .query_row("SELECT count(*) FROM parents", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0);
}

#[cfg(unix)]
#[test]
fn metadata_file_symbolic_link_is_not_followed() {
    let directory = physical_temporary_directory();
    let original = directory.path().join("original.sqlite3");
    drop(MetadataStore::open(&original).unwrap());
    let bytes = std::fs::read(&original).unwrap();
    let link = directory.path().join("link.sqlite3");
    std::os::unix::fs::symlink(&original, &link).unwrap();
    assert!(MetadataStore::open(&link).is_err());
    assert_eq!(std::fs::read(&original).unwrap(), bytes);
}

#[test]
fn process_crash_recovery_distinguishes_committed_and_uncommitted_rows() {
    use std::process::{Command, Stdio};
    for committed in [false, true] {
        let directory = physical_temporary_directory();
        let path = directory.path().join("metadata.sqlite3");
        let ready = directory.path().join("child-ready");
        let store = MetadataStore::open(&path).unwrap();
        store.migrate(&[FIXTURE]).unwrap();
        drop(store);
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "storage::tests::storage_crash_fixture_child",
                "--ignored",
                "--nocapture",
            ])
            .env("AXIAL_STORAGE_CRASH_DATABASE", &path)
            .env("AXIAL_STORAGE_CRASH_READY", &ready)
            .env(
                "AXIAL_STORAGE_CRASH_COMMITTED",
                if committed { "true" } else { "false" },
            )
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.exists() && Instant::now() < deadline {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let observed_ready = ready.exists();
        let _ = child.kill();
        child.wait().unwrap();
        assert!(
            observed_ready,
            "storage crash fixture did not reach the commit boundary"
        );
        let reopened = MetadataStore::open(&path).unwrap();
        let count = reopened
            .read(|conn| -> StorageResult<i64> {
                Ok(conn.query_row("SELECT count(*) FROM parents", [], |row| row.get(0))?)
            })
            .unwrap();
        assert_eq!(count, i64::from(committed));
        reopened.check_integrity().unwrap();
    }
}

/// Launched only by the parent test using a fresh isolated fixture directory.
#[test]
#[ignore = "subprocess fixture, started by process_crash_recovery test"]
fn storage_crash_fixture_child() {
    let Ok(path) = std::env::var("AXIAL_STORAGE_CRASH_DATABASE") else {
        return;
    };
    let ready = std::env::var("AXIAL_STORAGE_CRASH_READY").unwrap();
    let committed = std::env::var("AXIAL_STORAGE_CRASH_COMMITTED").unwrap() == "true";
    let store = MetadataStore::open(path).unwrap();
    store
        .transaction(|transaction| -> StorageResult<()> {
            transaction.execute("INSERT INTO parents VALUES (1)", [])?;
            if !committed {
                std::fs::write(&ready, b"uncommitted").unwrap();
                loop {
                    std::thread::park_timeout(Duration::from_secs(1));
                }
            }
            Ok(())
        })
        .unwrap();
    std::fs::write(ready, b"committed").unwrap();
    loop {
        std::thread::park_timeout(Duration::from_secs(1));
    }
}
