use std::{
    ffi::OsString,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

pub struct TempPaths {
    directory: PathBuf,
    main: PathBuf,
    wal: PathBuf,
    missing: PathBuf,
}

impl TempPaths {
    pub fn new(label: &str) -> Self {
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
        loop {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let directory = std::env::temp_dir().join(format!(
                "ora-sqllite-api-{}-{label}-{sequence}",
                std::process::id()
            ));
            match fs::create_dir(&directory) {
                Ok(()) => {
                    let main = directory.join("database.db");
                    let mut wal = OsString::from(main.as_os_str());
                    wal.push("-wal");
                    return Self {
                        missing: directory.join("missing.db"),
                        directory,
                        main,
                        wal: PathBuf::from(wal),
                    };
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create isolated API test directory: {error}"),
            }
        }
    }

    pub fn main(&self) -> &Path {
        &self.main
    }

    pub fn wal(&self) -> &Path {
        &self.wal
    }

    pub fn missing(&self) -> &Path {
        &self.missing
    }

    #[cfg(test)]
    fn directory(&self) -> &Path {
        &self.directory
    }
}

impl Drop for TempPaths {
    fn drop(&mut self) {
        for path in [&self.main, &self.wal, &self.missing] {
            if let Err(error) = fs::remove_file(path) {
                if error.kind() != ErrorKind::NotFound {
                    // Exact, best-effort cleanup must not broaden to another path.
                }
            }
        }
        if let Err(error) = fs::remove_dir(&self.directory) {
            if !matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::DirectoryNotEmpty
            ) {
                // Exact, best-effort cleanup must never become recursive cleanup.
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::TempPaths;

    #[test]
    fn cleanup_removes_only_owned_paths_and_never_recurses() {
        let (directory, main, wal, missing, sentinel);
        {
            let paths = TempPaths::new("cleanup-scope");
            directory = paths.directory().to_path_buf();
            main = paths.main().to_path_buf();
            wal = paths.wal().to_path_buf();
            missing = paths.missing().to_path_buf();
            sentinel = directory.join("retain-me");
            fs::write(&main, b"main").unwrap();
            fs::write(&wal, b"wal").unwrap();
            fs::write(&missing, b"missing candidate").unwrap();
            fs::write(&sentinel, b"unrelated").unwrap();
        }

        assert!(!main.exists());
        assert!(!wal.exists());
        assert!(!missing.exists());
        assert_eq!(fs::read(&sentinel).unwrap(), b"unrelated");
        assert!(directory.is_dir());
        fs::remove_file(sentinel).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn labels_cannot_escape_the_direct_temp_directory() {
        let paths = TempPaths::new("unsafe/../label");
        assert_eq!(
            paths.directory().parent(),
            Some(std::env::temp_dir().as_path())
        );
    }
}
