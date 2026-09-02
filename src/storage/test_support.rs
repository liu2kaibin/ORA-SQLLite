use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use crate::Result;

use super::pager::{Pager, PagerConfig};

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

impl PagerConfig {
    pub(super) fn test_default(page_size: u32) -> Self {
        Self::new(page_size, [0x5a; 16]).expect("test pager config uses a supported page size")
    }
}

pub(super) fn test_pager(page_size: u32) -> Result<(TempDatabase, Pager)> {
    let temp = TempDatabase::new("pager");
    let pager = Pager::create(temp.path(), PagerConfig::test_default(page_size))?;
    Ok((temp, pager))
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
