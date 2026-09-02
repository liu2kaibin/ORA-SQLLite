use std::{
    fmt,
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
    sync::Arc,
};

use super::{PageId, header::DatabaseHeader};
use crate::{
    DbError, ErrorCode, Result,
    fault::{FaultInjector, FaultPoint, NoFaults},
};

pub(super) const DEFAULT_MAX_DATABASE_BYTES: u64 = 10 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OpenMode {
    ReadOnly,
    ReadWrite,
}

pub(super) struct DatabaseFile {
    file: File,
    header: DatabaseHeader,
    page_count: u64,
    max_bytes: u64,
    faults: Arc<dyn FaultInjector>,
}

impl fmt::Debug for DatabaseFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DatabaseFile")
            .field("header", &self.header)
            .field("page_count", &self.page_count)
            .field("max_bytes", &self.max_bytes)
            .finish_non_exhaustive()
    }
}

impl DatabaseFile {
    pub(super) fn create(
        path: &Path,
        page_size: u32,
        uuid: [u8; 16],
        max_bytes: u64,
    ) -> Result<Self> {
        let header = DatabaseHeader::new(page_size, uuid)?;
        checked_page_offset(PageId(0), page_size, max_bytes)?;
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| io_error("create database", error))?;

        let mut database = Self {
            file,
            header,
            page_count: 1,
            max_bytes,
            faults: Arc::new(NoFaults),
        };
        database.write_header(header)?;
        database.sync_all()?;
        Ok(database)
    }

    pub(super) fn open(path: &Path, mode: OpenMode, max_bytes: u64) -> Result<Self> {
        let mut options = OpenOptions::new();
        options.read(true);
        if mode == OpenMode::ReadWrite {
            options.write(true);
        }
        let mut file = options
            .open(path)
            .map_err(|error| io_error("open database", error))?;

        let mut encoded_header = [0; 128];
        file.read_exact(&mut encoded_header)
            .map_err(|error| read_error("read database header", error))?;
        let header = DatabaseHeader::decode(&encoded_header)?;
        header.validate_open(mode)?;

        let len = file
            .metadata()
            .map_err(|error| io_error("read database length", error))?
            .len();
        let page_size = u64::from(header.page_size());
        if len < page_size {
            return Err(corruption("database file is shorter than its header page"));
        }
        if len % page_size != 0 {
            return Err(corruption(
                "database file length is not a whole number of pages",
            ));
        }
        if len > max_bytes {
            return Err(corruption("database file exceeds configured maximum size"));
        }

        let page_count = len / page_size;
        validate_root_page("catalog root", header.catalog_root(), page_count)?;
        validate_root_page("freelist root", header.freelist_root(), page_count)?;

        Ok(Self {
            file,
            header,
            page_count,
            max_bytes,
            faults: Arc::new(NoFaults),
        })
    }

    pub(super) const fn page_size(&self) -> u32 {
        self.header.page_size()
    }

    pub(super) const fn page_count(&self) -> u64 {
        self.page_count
    }

    pub(super) const fn header(&self) -> DatabaseHeader {
        self.header
    }

    pub(super) fn read_page(&mut self, page_id: PageId) -> Result<Vec<u8>> {
        let offset = self.ordinary_page_offset(page_id)?;
        let operation = format!("read page {}", page_id.0);
        let mut page = vec![0; self.page_size() as usize];
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|error| io_error(&operation, error))?;
        self.file
            .read_exact(&mut page)
            .map_err(|error| read_error(&operation, error))?;
        Ok(page)
    }

    pub(super) fn write_page(&mut self, page_id: PageId, bytes: &[u8]) -> Result<()> {
        if bytes.len() != self.page_size() as usize {
            return Err(corruption(format!(
                "write page {} requires exactly {} bytes",
                page_id.0,
                self.page_size()
            )));
        }
        let offset = self.ordinary_page_offset(page_id)?;
        let operation = format!("write page {}", page_id.0);
        self.write_at(offset, &operation, bytes)
    }

    pub(super) fn extend_to_pages(&mut self, page_count: u64) -> Result<()> {
        if page_count < self.page_count {
            return Err(corruption("database file extension cannot shrink pages"));
        }
        if page_count == self.page_count {
            return Ok(());
        }

        let final_page_offset =
            checked_page_offset(PageId(page_count - 1), self.page_size(), self.max_bytes)?;
        let new_len = final_page_offset
            .checked_add(u64::from(self.page_size()))
            .ok_or_else(|| corruption("database file length overflow"))?;

        self.faults.check(FaultPoint::BeforeFileWrite)?;
        self.file
            .set_len(new_len)
            .map_err(|error| io_error("extend database", error))?;
        self.faults.check(FaultPoint::AfterFileWrite)?;
        self.page_count = page_count;
        Ok(())
    }

    pub(super) fn write_header(&mut self, header: DatabaseHeader) -> Result<()> {
        if header.page_size() != self.page_size() {
            return Err(corruption(
                "database header page size does not match the file",
            ));
        }
        validate_root_page("catalog root", header.catalog_root(), self.page_count)?;
        validate_root_page("freelist root", header.freelist_root(), self.page_count)?;

        let mut page = vec![0; self.page_size() as usize];
        page[..128].copy_from_slice(&header.encode());
        self.write_at(0, "write database header", &page)?;
        self.header = header;
        Ok(())
    }

    pub(super) fn sync_all(&self) -> Result<()> {
        self.faults.check(FaultPoint::BeforeFileSync)?;
        self.file
            .sync_all()
            .map_err(|error| io_error("sync database", error))?;
        self.faults.check(FaultPoint::AfterFileSync)
    }

    #[cfg(test)]
    pub(super) fn with_faults(mut self, faults: Arc<dyn FaultInjector>) -> Self {
        self.faults = faults;
        self
    }

    fn ordinary_page_offset(&self, page_id: PageId) -> Result<u64> {
        if page_id.0 == 0 {
            return Err(corruption(
                "ordinary page I/O cannot access the database header",
            ));
        }
        if page_id.0 >= self.page_count {
            return Err(corruption(format!(
                "page id {} is outside the database",
                page_id.0
            )));
        }
        checked_page_offset(page_id, self.page_size(), self.max_bytes)
    }

    fn write_at(&mut self, offset: u64, operation: &str, bytes: &[u8]) -> Result<()> {
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|error| io_error(operation, error))?;
        self.faults.check(FaultPoint::BeforeFileWrite)?;
        self.file
            .write_all(bytes)
            .map_err(|error| io_error(operation, error))?;
        self.faults.check(FaultPoint::AfterFileWrite)
    }
}

fn checked_page_offset(page_id: PageId, page_size: u32, max_bytes: u64) -> Result<u64> {
    let page_size = u64::from(page_size);
    let offset = page_id
        .0
        .checked_mul(page_size)
        .ok_or_else(|| corruption("page offset overflow"))?;
    let end = offset
        .checked_add(page_size)
        .ok_or_else(|| corruption("page end offset overflow"))?;
    if end > max_bytes {
        return Err(corruption("page exceeds configured maximum database size"));
    }
    Ok(offset)
}

fn validate_root_page(name: &str, page_id: PageId, page_count: u64) -> Result<()> {
    if page_id.0 != 0 && page_id.0 >= page_count {
        return Err(corruption(format!(
            "{name} page id {} is outside the database",
            page_id.0
        )));
    }
    Ok(())
}

fn read_error(operation: &str, error: io::Error) -> DbError {
    if error.kind() == io::ErrorKind::UnexpectedEof {
        corruption(format!("{operation}: truncated data"))
    } else {
        io_error(operation, error)
    }
}

fn io_error(operation: &str, error: io::Error) -> DbError {
    DbError::new(ErrorCode::Io, format!("{operation}: {error}"))
}

fn corruption(message: impl Into<String>) -> DbError {
    DbError::new(ErrorCode::Corruption, message)
}

#[cfg(test)]
use super::test_support::TempDatabase;

#[test]
fn create_is_exclusive_and_writes_one_synced_header_page() {
    let temp = TempDatabase::new("create");
    let db = DatabaseFile::create(temp.path(), 4096, [4; 16], 10 * 1024 * 1024).unwrap();
    assert_eq!(db.page_count(), 1);
    assert_eq!(std::fs::metadata(temp.path()).unwrap().len(), 4096);
    assert_eq!(
        DatabaseFile::create(temp.path(), 4096, [4; 16], 10 * 1024 * 1024)
            .unwrap_err()
            .code(),
        ErrorCode::Io
    );
    drop(db);
}

#[test]
fn checked_offset_rejects_overflow_and_limit() {
    assert_eq!(
        checked_page_offset(PageId(u64::MAX), 16384, u64::MAX)
            .unwrap_err()
            .code(),
        ErrorCode::Corruption
    );
    assert_eq!(
        checked_page_offset(PageId(3), 4096, 8192)
            .unwrap_err()
            .code(),
        ErrorCode::Corruption
    );
}

#[test]
fn page_io_requires_full_pages_and_valid_bounds() {
    let temp = TempDatabase::new("page-io");
    let mut db = DatabaseFile::create(temp.path(), 4096, [1; 16], 8192).unwrap();
    db.extend_to_pages(2).unwrap();
    let page = vec![0xa5; 4096];
    db.write_page(PageId(1), &page).unwrap();
    assert_eq!(db.read_page(PageId(1)).unwrap(), page);
    assert_eq!(
        db.read_page(PageId(2)).unwrap_err().code(),
        ErrorCode::Corruption
    );
    assert_eq!(
        db.extend_to_pages(3).unwrap_err().code(),
        ErrorCode::Corruption
    );
}

#[test]
fn file_writes_and_syncs_cross_fault_boundaries() {
    let temp = TempDatabase::new("faults");
    let faults = Arc::new(RecordingFaults::default());
    let mut db = DatabaseFile::create(temp.path(), 4096, [2; 16], 8192)
        .unwrap()
        .with_faults(faults.clone());
    db.extend_to_pages(2).unwrap();
    db.write_page(PageId(1), &vec![0; 4096]).unwrap();
    db.write_header(db.header()).unwrap();
    db.sync_all().unwrap();

    assert_eq!(
        faults.recorded(),
        vec![
            FaultPoint::BeforeFileWrite,
            FaultPoint::AfterFileWrite,
            FaultPoint::BeforeFileWrite,
            FaultPoint::AfterFileWrite,
            FaultPoint::BeforeFileWrite,
            FaultPoint::AfterFileWrite,
            FaultPoint::BeforeFileSync,
            FaultPoint::AfterFileSync,
        ]
    );
}

#[test]
fn open_rejects_truncation_unaligned_length_and_out_of_bounds_roots() {
    let truncated = TempDatabase::new("truncated");
    std::fs::write(truncated.path(), [0; 127]).unwrap();
    assert_eq!(
        DatabaseFile::open(truncated.path(), OpenMode::ReadOnly, 4096)
            .unwrap_err()
            .code(),
        ErrorCode::Corruption
    );

    let unaligned = TempDatabase::new("unaligned");
    let db = DatabaseFile::create(unaligned.path(), 4096, [3; 16], 8192).unwrap();
    drop(db);
    std::fs::OpenOptions::new()
        .append(true)
        .open(unaligned.path())
        .unwrap()
        .write_all(&[0])
        .unwrap();
    assert_eq!(
        DatabaseFile::open(unaligned.path(), OpenMode::ReadOnly, 8192)
            .unwrap_err()
            .code(),
        ErrorCode::Corruption
    );

    let invalid_root = TempDatabase::new("invalid-root");
    let mut header = DatabaseHeader::new(4096, [5; 16]).unwrap();
    header.set_catalog_root(PageId(1));
    let mut bytes = vec![0; 4096];
    bytes[..128].copy_from_slice(&header.encode());
    std::fs::write(invalid_root.path(), bytes).unwrap();
    assert_eq!(
        DatabaseFile::open(invalid_root.path(), OpenMode::ReadOnly, 8192)
            .unwrap_err()
            .code(),
        ErrorCode::Corruption
    );
}

#[test]
fn open_preserves_header_and_read_only_rejects_page_writes() {
    let temp = TempDatabase::new("open-read-only");
    let mut created = DatabaseFile::create(temp.path(), 8192, [6; 16], 16384).unwrap();
    created.extend_to_pages(2).unwrap();
    drop(created);

    let mut opened = DatabaseFile::open(temp.path(), OpenMode::ReadOnly, 16384).unwrap();
    assert_eq!(opened.page_size(), 8192);
    assert_eq!(opened.page_count(), 2);
    assert_eq!(opened.header().uuid(), [6; 16]);
    let error = opened.write_page(PageId(1), &vec![0; 8192]).unwrap_err();
    assert_eq!(error.code(), ErrorCode::Io);
    assert!(error.to_string().contains("write page 1"));
}

#[test]
fn write_header_replaces_the_complete_zero_filled_header_page() {
    let temp = TempDatabase::new("write-header");
    let mut db = DatabaseFile::create(temp.path(), 4096, [7; 16], 8192).unwrap();
    let header = DatabaseHeader::new(4096, [8; 16]).unwrap();
    db.write_header(header).unwrap();
    db.sync_all().unwrap();
    drop(db);

    let bytes = std::fs::read(temp.path()).unwrap();
    assert_eq!(&bytes[..128], &header.encode());
    assert_eq!(&bytes[128..], &[0; 4096 - 128]);
}

#[test]
fn ordinary_page_io_rejects_header_page_and_nonfull_writes() {
    let temp = TempDatabase::new("page-arguments");
    let mut db = DatabaseFile::create(temp.path(), 4096, [9; 16], 8192).unwrap();
    db.extend_to_pages(2).unwrap();

    assert_eq!(
        db.read_page(PageId(0)).unwrap_err().code(),
        ErrorCode::Corruption
    );
    assert_eq!(
        db.write_page(PageId(0), &vec![0; 4096]).unwrap_err().code(),
        ErrorCode::Corruption
    );
    assert_eq!(
        db.write_page(PageId(1), &[0; 4095]).unwrap_err().code(),
        ErrorCode::Corruption
    );
}

#[test]
fn open_enforces_file_limit_and_future_minor_write_policy() {
    let limited = TempDatabase::new("limit");
    let mut db = DatabaseFile::create(limited.path(), 4096, [10; 16], 8192).unwrap();
    db.extend_to_pages(2).unwrap();
    drop(db);
    assert_eq!(
        DatabaseFile::open(limited.path(), OpenMode::ReadOnly, 4096)
            .unwrap_err()
            .code(),
        ErrorCode::Corruption
    );

    let future_minor = TempDatabase::new("future-minor");
    let db = DatabaseFile::create(future_minor.path(), 4096, [11; 16], 8192).unwrap();
    drop(db);
    let mut bytes = std::fs::read(future_minor.path()).unwrap();
    bytes[18..20].copy_from_slice(&1u16.to_le_bytes());
    let checksum = super::checksum::crc32c(&bytes[..124]).to_le_bytes();
    bytes[124..128].copy_from_slice(&checksum);
    std::fs::write(future_minor.path(), bytes).unwrap();

    DatabaseFile::open(future_minor.path(), OpenMode::ReadOnly, 8192).unwrap();
    assert_eq!(
        DatabaseFile::open(future_minor.path(), OpenMode::ReadWrite, 8192)
            .unwrap_err()
            .code(),
        ErrorCode::Unsupported
    );
}

#[derive(Default)]
struct RecordingFaults {
    points: std::sync::Mutex<Vec<FaultPoint>>,
}

impl RecordingFaults {
    fn recorded(&self) -> Vec<FaultPoint> {
        self.points.lock().expect("fault record lock").clone()
    }
}

impl FaultInjector for RecordingFaults {
    fn check(&self, point: FaultPoint) -> Result<()> {
        self.points.lock().expect("fault record lock").push(point);
        Ok(())
    }
}
