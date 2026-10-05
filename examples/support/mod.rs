use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use ora_sqllite::{Database, DbError, ErrorCode, Options};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

pub struct OwnedExampleDatabase {
    directory: PathBuf,
    main_path: PathBuf,
}

impl OwnedExampleDatabase {
    pub fn create(label: &str) -> ora_sqllite::Result<(Self, Database)> {
        let label: String = label
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                    character
                } else {
                    '_'
                }
            })
            .collect();
        let epoch_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| DbError::new(ErrorCode::Internal, error.to_string()))?
            .as_nanos();
        let process_id = std::process::id();

        for _ in 0..32 {
            let candidate = std::env::temp_dir().join(format!(
                "ora-sqllite-{label}-{process_id}-{epoch_nanos}-{}",
                NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
            ));

            match std::fs::create_dir(&candidate) {
                Ok(()) => {
                    let owned = Self {
                        main_path: candidate.join("example.db"),
                        directory: candidate,
                    };
                    let database = Database::create(owned.main_path(), Options::default())?;
                    return Ok((owned, database));
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(DbError::new(ErrorCode::Io, error.to_string())),
            }
        }

        Err(DbError::new(
            ErrorCode::Io,
            "could not acquire a unique example database directory",
        ))
    }

    pub fn main_path(&self) -> &Path {
        &self.main_path
    }

    #[cfg(test)]
    fn directory(&self) -> &Path {
        &self.directory
    }
}

impl Drop for OwnedExampleDatabase {
    fn drop(&mut self) {
        for path in [
            self.main_path.clone(),
            self.directory.join("example.db-wal"),
        ] {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(_) => {}
            }
        }

        match std::fs::remove_dir(&self.directory) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs::{File, OpenOptions},
        io::{ErrorKind, Write},
        path::{Path, PathBuf},
    };

    use super::OwnedExampleDatabase;

    fn acquire_owned_sibling_fixture(parent: &Path) -> (PathBuf, File) {
        for suffix in 0..32 {
            let candidate = parent.join(format!(
                "ora-sqllite-sibling-{}-{suffix}",
                std::process::id()
            ));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(file) => return (candidate, file),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("acquire sibling fixture: {error}"),
            }
        }

        panic!("could not acquire a unique sibling fixture after 32 attempts");
    }

    #[test]
    fn owned_database_stays_below_the_system_temp_directory() {
        let (owned, database) = OwnedExampleDatabase::create("path-test").unwrap();
        assert!(owned.main_path().starts_with(std::env::temp_dir()));
        assert_eq!(owned.main_path().parent(), Some(owned.directory()));
        drop(database);
    }

    #[test]
    fn labels_cannot_escape_the_direct_temp_directory() {
        let (owned, database) = OwnedExampleDatabase::create("../escape-test").unwrap();
        assert_eq!(
            owned.directory().parent(),
            Some(std::env::temp_dir().as_path())
        );
        drop(database);
    }

    #[test]
    fn sibling_fixture_skips_existing_paths_without_changing_them() {
        let (owned, database) = OwnedExampleDatabase::create("sibling-fixture-test").unwrap();
        let collision = owned
            .directory()
            .join(format!("ora-sqllite-sibling-{}-0", std::process::id()));
        let mut collision_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&collision)
            .unwrap();
        collision_file.write_all(b"caller").unwrap();
        drop(collision_file);

        let (sibling, file) = acquire_owned_sibling_fixture(owned.directory());
        drop(file);

        assert_ne!(sibling, collision);
        assert_eq!(std::fs::read(&collision).unwrap(), b"caller");

        std::fs::remove_file(sibling).unwrap();
        std::fs::remove_file(collision).unwrap();
        drop(database);
    }

    #[test]
    fn cleanup_removes_owned_files_but_preserves_a_sibling() {
        let (sibling, mut sibling_file) = acquire_owned_sibling_fixture(&std::env::temp_dir());
        sibling_file.write_all(b"keep").unwrap();
        drop(sibling_file);
        {
            let (owned, database) = OwnedExampleDatabase::create("cleanup-test").unwrap();
            assert!(owned.main_path().exists());
            drop(database);
        }
        assert_eq!(std::fs::read(&sibling).unwrap(), b"keep");
        std::fs::remove_file(sibling).unwrap();
    }

    #[test]
    fn nonempty_owned_directory_is_not_recursively_deleted() {
        let (owned, database) = OwnedExampleDatabase::create("nonempty-test").unwrap();
        let directory = owned.directory().to_path_buf();
        let witness = directory.join("unexpected.txt");
        std::fs::write(&witness, b"preserve").unwrap();
        drop(database);
        drop(owned);
        assert_eq!(std::fs::read(&witness).unwrap(), b"preserve");
        std::fs::remove_file(witness).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
}
