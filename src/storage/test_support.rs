use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_TEMP_DATABASE: AtomicU64 = AtomicU64::new(0);

pub(super) struct TempDatabase {
    path: PathBuf,
}

impl TempDatabase {
    pub(super) fn new(label: &str) -> Self {
        let process_id = std::process::id();
        let label = sanitized_label(label);

        loop {
            let sequence = NEXT_TEMP_DATABASE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("ora-sqllite-{process_id}-{label}-{sequence}.db"));
            if !path.exists() {
                return Self { path };
            }
        }
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDatabase {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_file(&self.path) {
            if error.kind() != ErrorKind::NotFound {
                // Test cleanup is best effort; a later test must never remove a different path.
            }
        }
    }
}

fn sanitized_label(label: &str) -> String {
    let label: String = label
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect();
    if label.is_empty() {
        "database".to_owned()
    } else {
        label
    }
}
