use std::{
    fmt,
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
    sync::Arc,
};

use fs4::{FileExt, TryLockError};

use super::{
    PageId,
    header::{DatabaseHeader, SUPERBLOCK_AREA_LEN, SUPERBLOCK_SLOT_LEN, SuperblockSlot},
};
use crate::{
    DbError, ErrorCode, Result,
    fault::{FaultInjector, FaultPoint, NoFaults},
};

pub(super) const DEFAULT_MAX_DATABASE_BYTES: u64 = 10 * 1024 * 1024 * 1024;
const FORMAT_1_MAGIC: &[u8; 16] = b"ORA-SQLLITE\0\0\0\0\0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenMode {
    ReadOnly,
    ReadWrite,
}

pub(super) struct DatabaseFile {
    file: File,
    header: DatabaseHeader,
    active_slot: SuperblockSlot,
    page_count: u64,
    max_bytes: u64,
    initial_pair: bool,
    faults: Arc<dyn FaultInjector>,
}

impl fmt::Debug for DatabaseFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DatabaseFile")
            .field("header", &self.header)
            .field("active_slot", &self.active_slot)
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
        Self::create_with_faults(path, page_size, uuid, max_bytes, Arc::new(NoFaults))
    }

    pub(super) fn create_with_faults(
        path: &Path,
        page_size: u32,
        uuid: [u8; 16],
        max_bytes: u64,
        faults: Arc<dyn FaultInjector>,
    ) -> Result<Self> {
        let header = DatabaseHeader::new(page_size, uuid)?;
        checked_database_len(header.page_count(), page_size, max_bytes)?;
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| io_error("create database", error))?;
        lock_database(&file)?;

        let mut database = Self {
            file,
            header,
            active_slot: SuperblockSlot::A,
            page_count: 1,
            max_bytes,
            initial_pair: true,
            faults,
        };
        database.write_superblock_slot(SuperblockSlot::A, header)?;
        database.sync_all()?;
        let second = header.with_checkpoint(
            header.checkpoint_lsn(),
            header.page_count(),
            header.catalog_root(),
            header.freelist_root(),
            header.wal_generation(),
        )?;
        database.write_superblock_slot(SuperblockSlot::B, second)?;
        database.sync_all()?;
        database.header = second;
        database.active_slot = SuperblockSlot::B;
        sync_parent_directory(
            path,
            &*database.faults,
            FaultPoint::BeforeMainDirectorySync,
            FaultPoint::AfterMainDirectorySync,
        )?;
        Ok(database)
    }

    pub(super) fn open(path: &Path, mode: OpenMode, max_bytes: u64) -> Result<Self> {
        Self::open_with_checkpoint_tail(path, mode, max_bytes, false)
    }

    /// A checkpoint may persist complete data pages before publishing its superblock.
    /// Pager must subsequently prove the tail against the recovered committed page count.
    pub(super) fn open_for_recovery(path: &Path, mode: OpenMode, max_bytes: u64) -> Result<Self> {
        Self::open_with_checkpoint_tail(path, mode, max_bytes, true)
    }

    fn open_with_checkpoint_tail(
        path: &Path,
        mode: OpenMode,
        max_bytes: u64,
        allow_tail: bool,
    ) -> Result<Self> {
        let mut options = OpenOptions::new();
        options.read(true);
        if mode == OpenMode::ReadWrite {
            options.write(true);
        }
        let mut file = options
            .open(path)
            .map_err(|error| io_error("open database", error))?;
        lock_database(&file)?;

        let mut magic = [0; 16];
        file.read_exact(&mut magic)
            .map_err(|error| read_error("read database superblock magic", error))?;
        if &magic == FORMAT_1_MAGIC {
            return Err(DbError::new(
                ErrorCode::Unsupported,
                "database format 1 is unsupported; recreate the database",
            ));
        }
        file.seek(SeekFrom::Start(0))
            .map_err(|error| io_error("seek database superblocks", error))?;
        let mut slots = [0; SUPERBLOCK_SLOT_LEN * 2];
        file.read_exact(&mut slots)
            .map_err(|error| read_error("read database superblocks", error))?;
        let (header, active_slot) =
            DatabaseHeader::select(&slots[..SUPERBLOCK_SLOT_LEN], &slots[SUPERBLOCK_SLOT_LEN..])?;
        let initial = DatabaseHeader::new(header.page_size(), header.uuid())?;
        let second = initial.with_checkpoint(super::Lsn(0), 1, PageId(0), PageId(0), 1)?;
        let initial_pair = slots[..SUPERBLOCK_SLOT_LEN] == initial.encode_slot()
            && slots[SUPERBLOCK_SLOT_LEN..] == second.encode_slot();
        header.validate_open(mode)?;

        let len = file
            .metadata()
            .map_err(|error| io_error("read database length", error))?
            .len();
        if len > max_bytes {
            return Err(corruption("database file exceeds configured maximum size"));
        }
        let page_count = header.page_count();
        let expected_len = checked_database_len(page_count, header.page_size(), max_bytes)?;
        if len < expected_len
            || (!allow_tail && len != expected_len)
            || (len - SUPERBLOCK_AREA_LEN) % u64::from(header.page_size()) != 0
        {
            return Err(corruption(
                "database file length does not match the published page count",
            ));
        }

        Ok(Self {
            file,
            header,
            active_slot,
            page_count,
            max_bytes,
            initial_pair,
            faults: Arc::new(NoFaults),
        })
    }

    pub(super) fn validate_recovered_tail(&self, recovered_pages: u64) -> Result<()> {
        let len = self
            .file
            .metadata()
            .map_err(|error| io_error("read recovered database length", error))?
            .len();
        let recovered_len =
            checked_database_len(recovered_pages, self.page_size(), self.max_bytes)?;
        if len > recovered_len {
            return Err(corruption(
                "database checkpoint tail exceeds the recovered committed page count",
            ));
        }
        Ok(())
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

    pub(super) fn can_recreate_wal(&self) -> bool {
        self.initial_pair && self.header.publish_sequence() <= 2
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
        self.write_at(
            offset,
            &operation,
            bytes,
            FaultPoint::BeforeMainPageWrite,
            FaultPoint::AfterMainPageWrite,
        )
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

        self.faults.check(FaultPoint::BeforeMainExtend)?;
        self.file
            .set_len(new_len)
            .map_err(|error| io_error("extend database", error))?;
        self.faults.check(FaultPoint::AfterMainExtend)?;
        self.page_count = page_count;
        Ok(())
    }

    pub(super) fn publish_superblock(&mut self, candidate: DatabaseHeader) -> Result<()> {
        if candidate.page_size() != self.page_size() {
            return Err(corruption(
                "database superblock page size does not match the file",
            ));
        }
        if candidate.uuid() != self.header.uuid() {
            return Err(corruption(
                "database superblock UUID does not match the file",
            ));
        }
        if candidate.page_count() != self.page_count {
            return Err(corruption(
                "database superblock page count does not match the file",
            ));
        }
        validate_root_page("catalog root", candidate.catalog_root(), self.page_count)?;
        validate_root_page("freelist root", candidate.freelist_root(), self.page_count)?;
        let expected_sequence = self
            .header
            .publish_sequence()
            .checked_add(1)
            .ok_or_else(|| corruption("database publish sequence overflow"))?;
        if candidate.publish_sequence() != expected_sequence {
            return Err(corruption(
                "database superblock publish sequence is not the next sequence",
            ));
        }

        let inactive_slot = match self.active_slot {
            SuperblockSlot::A => SuperblockSlot::B,
            SuperblockSlot::B => SuperblockSlot::A,
        };
        self.write_superblock_slot(inactive_slot, candidate)?;
        self.sync_all()?;
        self.header = candidate;
        self.active_slot = inactive_slot;
        Ok(())
    }

    /// Sequences 1 and 2 belong only to create; sequence 3 durably acknowledges WAL setup.
    pub(super) fn finish_wal_initialization(&mut self) -> Result<()> {
        while self.header.publish_sequence() <= 2 {
            let header = self.header;
            self.publish_superblock(header.with_checkpoint(
                header.checkpoint_lsn(),
                header.page_count(),
                header.catalog_root(),
                header.freelist_root(),
                header.wal_generation(),
            )?)?;
        }
        Ok(())
    }

    pub(super) fn sync_all(&self) -> Result<()> {
        self.faults.check(FaultPoint::BeforeMainSync)?;
        self.file
            .sync_all()
            .map_err(|error| io_error("sync database", error))?;
        self.faults.check(FaultPoint::AfterMainSync)
    }

    #[cfg(test)]
    pub(super) fn with_faults(mut self, faults: Arc<dyn FaultInjector>) -> Self {
        self.faults = faults;
        self
    }

    fn ordinary_page_offset(&self, page_id: PageId) -> Result<u64> {
        if page_id.0 == 0 {
            return Err(corruption("ordinary page I/O cannot access page zero"));
        }
        if page_id.0 >= self.page_count {
            return Err(corruption(format!(
                "page id {} is outside the database",
                page_id.0
            )));
        }
        checked_page_offset(page_id, self.page_size(), self.max_bytes)
    }

    fn write_at(
        &mut self,
        offset: u64,
        operation: &str,
        bytes: &[u8],
        before: FaultPoint,
        after: FaultPoint,
    ) -> Result<()> {
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|error| io_error(operation, error))?;
        self.faults.check(before)?;
        self.file
            .write_all(bytes)
            .map_err(|error| io_error(operation, error))?;
        self.faults.check(after)
    }

    fn write_superblock_slot(
        &mut self,
        slot: SuperblockSlot,
        header: DatabaseHeader,
    ) -> Result<()> {
        let offset = match slot {
            SuperblockSlot::A => 0,
            SuperblockSlot::B => {
                u64::try_from(SUPERBLOCK_SLOT_LEN).expect("superblock slot length fits u64")
            }
        };
        self.write_at(
            offset,
            "write database superblock",
            &header.encode_slot(),
            FaultPoint::BeforeSuperblockWrite,
            FaultPoint::AfterSuperblockWrite,
        )
    }
}

/// Publish a newly created name. Errors are fatal to creation, never a no-op fallback.
/// Windows requires a writable directory handle; read-only handles cannot be flushed.
pub(super) fn sync_parent_directory(
    path: &Path,
    faults: &dyn FaultInjector,
    before: FaultPoint,
    after: FaultPoint,
) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    faults.check(before)?;
    #[cfg(windows)]
    let directory = {
        use std::os::windows::fs::OpenOptionsExt;
        OpenOptions::new()
            .write(true)
            .custom_flags(0x0200_0000)
            .open(parent)
    };
    #[cfg(unix)]
    let directory = File::open(parent);
    #[cfg(not(any(windows, unix)))]
    let directory: io::Result<File> = Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "directory durability is unsupported on this platform",
    ));
    directory
        .map_err(|error| io_error("open parent directory for sync", error))?
        .sync_all()
        .map_err(|error| io_error("sync parent directory", error))?;
    faults.check(after)
}

pub(super) fn checked_page_offset(page_id: PageId, page_size: u32, max_bytes: u64) -> Result<u64> {
    let ordinary_index = page_id
        .0
        .checked_sub(1)
        .ok_or_else(|| corruption("ordinary page I/O cannot access page zero"))?;
    let page_size = u64::from(page_size);
    let offset = SUPERBLOCK_AREA_LEN
        .checked_add(
            ordinary_index
                .checked_mul(page_size)
                .ok_or_else(|| corruption("page offset overflow"))?,
        )
        .ok_or_else(|| corruption("page offset overflow"))?;
    let end = offset
        .checked_add(page_size)
        .ok_or_else(|| corruption("page end offset overflow"))?;
    if end > max_bytes {
        return Err(corruption("page exceeds configured maximum database size"));
    }
    Ok(offset)
}

fn checked_database_len(page_count: u64, page_size: u32, max_bytes: u64) -> Result<u64> {
    let ordinary_pages = page_count
        .checked_sub(1)
        .ok_or_else(|| corruption("database page count must be nonzero"))?;
    let data_len = ordinary_pages
        .checked_mul(u64::from(page_size))
        .ok_or_else(|| corruption("database file length overflow"))?;
    let len = SUPERBLOCK_AREA_LEN
        .checked_add(data_len)
        .ok_or_else(|| corruption("database file length overflow"))?;
    if len > max_bytes {
        return Err(corruption("database exceeds configured maximum size"));
    }
    Ok(len)
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

fn lock_database(file: &File) -> Result<()> {
    match FileExt::try_lock(file) {
        Ok(()) => Ok(()),
        Err(TryLockError::WouldBlock) => Err(DbError::new(
            ErrorCode::Busy,
            "database file is locked by another process",
        )),
        Err(TryLockError::Error(error)) => Err(io_error("lock database", error)),
    }
}

fn corruption(message: impl Into<String>) -> DbError {
    DbError::new(ErrorCode::Corruption, message)
}

#[cfg(test)]
pub(super) fn read_all_for_test(database: &mut DatabaseFile) -> Result<Vec<u8>> {
    let len = database
        .file
        .metadata()
        .map_err(|error| io_error("read database test length", error))?
        .len();
    let len =
        usize::try_from(len).map_err(|_| corruption("database test length does not fit memory"))?;
    let mut bytes = vec![0; len];
    database
        .file
        .seek(SeekFrom::Start(0))
        .map_err(|error| io_error("seek database test bytes", error))?;
    database
        .file
        .read_exact(&mut bytes)
        .map_err(|error| read_error("read database test bytes", error))?;
    Ok(bytes)
}

#[cfg(test)]
use super::{Lsn, test_support::TempDatabase};

#[test]
fn create_is_exclusive_and_writes_two_synced_superblocks() {
    let temp = TempDatabase::new("create");
    let db = DatabaseFile::create(temp.path(), 4096, [4; 16], 10 * 1024 * 1024).unwrap();
    assert_eq!(db.page_count(), 1);
    drop(db);
    let bytes = std::fs::read(temp.path()).unwrap();
    assert_eq!(bytes.len(), 8192);
    let slot_a = DatabaseHeader::decode_slot(&bytes[..4096]).unwrap();
    let slot_b = DatabaseHeader::decode_slot(&bytes[4096..8192]).unwrap();
    assert_eq!(slot_a.publish_sequence(), 1);
    assert_eq!(slot_b.publish_sequence(), 2);
    assert_eq!(slot_a.uuid(), [4; 16]);
    assert_eq!(slot_b.uuid(), [4; 16]);
    assert_eq!(
        DatabaseFile::create(temp.path(), 4096, [4; 16], 10 * 1024 * 1024)
            .unwrap_err()
            .code(),
        ErrorCode::Io
    );
}

#[test]
fn checked_offset_rejects_overflow_and_limit() {
    assert_eq!(checked_page_offset(PageId(1), 4096, 12_288).unwrap(), 8192);
    assert_eq!(
        checked_page_offset(PageId(0), 4096, 12_288)
            .unwrap_err()
            .code(),
        ErrorCode::Corruption
    );
    assert_eq!(
        checked_page_offset(PageId(u64::MAX), 16384, u64::MAX)
            .unwrap_err()
            .code(),
        ErrorCode::Corruption
    );
    assert_eq!(
        checked_page_offset(PageId(3), 4096, 12_288)
            .unwrap_err()
            .code(),
        ErrorCode::Corruption
    );
}

#[test]
fn page_io_requires_full_pages_and_valid_bounds() {
    let temp = TempDatabase::new("page-io");
    let mut db = DatabaseFile::create(temp.path(), 4096, [1; 16], 12_288).unwrap();
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
    let mut db = DatabaseFile::create(temp.path(), 4096, [2; 16], 12_288)
        .unwrap()
        .with_faults(faults.clone());
    db.extend_to_pages(2).unwrap();
    db.write_page(PageId(1), &vec![0; 4096]).unwrap();
    let header = db
        .header()
        .with_checkpoint(Lsn(0), 2, PageId(0), PageId(0), 1)
        .unwrap();
    db.publish_superblock(header).unwrap();

    assert_eq!(
        faults
            .recorded()
            .iter()
            .map(|point| format!("{point:?}"))
            .collect::<Vec<_>>(),
        vec![
            "BeforeMainExtend",
            "AfterMainExtend",
            "BeforeMainPageWrite",
            "AfterMainPageWrite",
            "BeforeSuperblockWrite",
            "AfterSuperblockWrite",
            "BeforeMainSync",
            "AfterMainSync",
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
        DatabaseFile::open(unaligned.path(), OpenMode::ReadOnly, 8193)
            .unwrap_err()
            .code(),
        ErrorCode::Corruption
    );

    let invalid_root = TempDatabase::new("invalid-root");
    let mut header = DatabaseHeader::new(4096, [5; 16]).unwrap();
    header.set_catalog_root(PageId(1));
    let mut bytes = vec![0; 8192];
    bytes[..4096].copy_from_slice(&header.encode_slot());
    bytes[4096..].copy_from_slice(&header.encode_slot());
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
    let header = created
        .header()
        .with_checkpoint(Lsn(0), 2, PageId(0), PageId(0), 1)
        .unwrap();
    created.publish_superblock(header).unwrap();
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
fn publish_superblock_alternates_slots_and_preserves_the_other_copy() {
    let temp = TempDatabase::new("publish-superblock");
    let mut db = DatabaseFile::create(temp.path(), 4096, [7; 16], 12_288).unwrap();
    db.extend_to_pages(2).unwrap();
    let header = db
        .header()
        .with_checkpoint(Lsn(0), 2, PageId(0), PageId(0), 1)
        .unwrap();
    db.publish_superblock(header).unwrap();
    drop(db);

    let bytes = std::fs::read(temp.path()).unwrap();
    assert_eq!(&bytes[..4096], &header.encode_slot());
    assert_eq!(
        DatabaseHeader::decode_slot(&bytes[4096..8192])
            .unwrap()
            .publish_sequence(),
        2
    );
}

#[test]
fn ordinary_page_io_rejects_header_page_and_nonfull_writes() {
    let temp = TempDatabase::new("page-arguments");
    let mut db = DatabaseFile::create(temp.path(), 4096, [9; 16], 12_288).unwrap();
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
    let mut db = DatabaseFile::create(limited.path(), 4096, [10; 16], 12_288).unwrap();
    db.extend_to_pages(2).unwrap();
    let header = db
        .header()
        .with_checkpoint(Lsn(0), 2, PageId(0), PageId(0), 1)
        .unwrap();
    db.publish_superblock(header).unwrap();
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
    for slot_start in [0, 4096] {
        bytes[slot_start + 18..slot_start + 20].copy_from_slice(&1u16.to_le_bytes());
        let checksum = super::checksum::crc32c(&bytes[slot_start..slot_start + 4092]).to_le_bytes();
        bytes[slot_start + 4092..slot_start + 4096].copy_from_slice(&checksum);
    }
    std::fs::write(future_minor.path(), bytes).unwrap();

    DatabaseFile::open(future_minor.path(), OpenMode::ReadOnly, 8192).unwrap();
    assert_eq!(
        DatabaseFile::open(future_minor.path(), OpenMode::ReadWrite, 8192)
            .unwrap_err()
            .code(),
        ErrorCode::Unsupported
    );
}

#[test]
fn open_uses_one_valid_superblock_and_rejects_two_corrupt_slots() {
    let temp = TempDatabase::new("superblock-corruption");
    let db = DatabaseFile::create(temp.path(), 4096, [12; 16], 8192).unwrap();
    drop(db);

    let mut bytes = std::fs::read(temp.path()).unwrap();
    bytes[95] ^= 1;
    std::fs::write(temp.path(), &bytes).unwrap();
    let opened = DatabaseFile::open(temp.path(), OpenMode::ReadOnly, 8192).unwrap();
    assert_eq!(opened.header().publish_sequence(), 2);
    drop(opened);

    bytes[4096 + 95] ^= 1;
    std::fs::write(temp.path(), bytes).unwrap();
    assert_eq!(
        DatabaseFile::open(temp.path(), OpenMode::ReadOnly, 8192)
            .unwrap_err()
            .code(),
        ErrorCode::Corruption
    );
}

#[test]
fn open_reports_format_1_as_unsupported() {
    let temp = TempDatabase::new("format-1");
    let mut bytes = vec![0; 8192];
    bytes[..16].copy_from_slice(FORMAT_1_MAGIC);
    std::fs::write(temp.path(), bytes).unwrap();
    assert_eq!(
        DatabaseFile::open(temp.path(), OpenMode::ReadOnly, 8192)
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
