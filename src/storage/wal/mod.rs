pub(super) mod format;
pub(crate) mod index;
pub(crate) mod recovery;

use std::{
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use super::{Lsn, PageId, checksum::Crc32c, file::OpenMode, header::DatabaseHeader};
use crate::{
    DbError, ErrorCode, Result,
    fault::{FaultInjector, FaultPoint, NoFaults},
};

use self::{
    format::{
        CommitPayload, CommitState, RecordHeader, RecordType, WAL_HEADER_SLOT_LEN, WalHeader,
        record_end, zero_padding_len,
    },
    index::WalVersion,
};

const WAL_HEADER_AREA_LEN: u64 = (WAL_HEADER_SLOT_LEN as u64) * 2;

static NEXT_WAL_SALT: AtomicU64 = AtomicU64::new(1);

pub(super) struct Wal {
    file: File,
    path: PathBuf,
    header: WalHeader,
    end_offset: u64,
    faults: Arc<dyn FaultInjector>,
    poisoned: bool,
}

struct WalCreateRollback {
    path: PathBuf,
    active: bool,
}

impl WalCreateRollback {
    fn new(path: PathBuf) -> Self {
        Self { path, active: true }
    }

    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for WalCreateRollback {
    fn drop(&mut self) {
        if self.active {
            let _ = fs::remove_file(&self.path);
        }
    }
}

impl fmt::Debug for Wal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Wal")
            .field("path", &self.path)
            .field("header", &self.header)
            .field("end_offset", &self.end_offset)
            .field("poisoned", &self.poisoned)
            .finish_non_exhaustive()
    }
}

impl Wal {
    pub(super) fn create(database_path: &Path, database_header: &DatabaseHeader) -> Result<Self> {
        Self::create_with_faults(database_path, database_header, Arc::new(NoFaults))
    }

    fn create_with_faults(
        database_path: &Path,
        database_header: &DatabaseHeader,
        faults: Arc<dyn FaultInjector>,
    ) -> Result<Self> {
        let path = wal_path(database_path);
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| io_error("create WAL", error))?;
        let mut rollback = WalCreateRollback::new(path.clone());
        let first_header = WalHeader {
            page_size: database_header.page_size(),
            database_uuid: database_header.uuid(),
            wal_generation: database_header.wal_generation(),
            base_checkpoint_lsn: database_header.checkpoint_lsn(),
            salt: new_salt(),
            header_sequence: 1,
        };
        let mut wal = Self {
            file,
            path,
            header: first_header,
            end_offset: 0,
            faults,
            poisoned: false,
        };
        wal.write_header(0, first_header)?;
        wal.sync_all()?;

        let second_header = WalHeader {
            header_sequence: 2,
            ..first_header
        };
        wal.write_header(
            u64::try_from(WAL_HEADER_SLOT_LEN).expect("WAL header slot length fits u64"),
            second_header,
        )?;
        wal.sync_all()?;
        wal.header = second_header;
        wal.end_offset = WAL_HEADER_AREA_LEN;
        rollback.disarm();
        Ok(wal)
    }

    pub(super) fn open(
        database_path: &Path,
        database_header: &DatabaseHeader,
        mode: OpenMode,
    ) -> Result<Self> {
        let path = wal_path(database_path);
        let mut options = OpenOptions::new();
        options.read(true);
        if mode == OpenMode::ReadWrite {
            options.write(true);
        }
        let mut file = match options.open(&path) {
            Ok(file) => file,
            Err(error)
                if error.kind() == io::ErrorKind::NotFound && initial_empty(database_header) =>
            {
                if mode == OpenMode::ReadWrite {
                    return Self::create(database_path, database_header);
                }
                return Err(io_error("open WAL", error));
            }
            Err(error) => return Err(io_error("open WAL", error)),
        };
        let end_offset = file
            .metadata()
            .map_err(|error| io_error("read WAL length", error))?
            .len();
        if end_offset < WAL_HEADER_AREA_LEN {
            return Err(corruption("WAL file is shorter than its header area"));
        }

        let mut slots = [0; WAL_HEADER_SLOT_LEN * 2];
        file.read_exact(&mut slots)
            .map_err(|error| read_error("read WAL header slots", error))?;
        let header = select_header(
            &slots[..WAL_HEADER_SLOT_LEN],
            &slots[WAL_HEADER_SLOT_LEN..],
            database_header,
        )?;
        Ok(Self {
            file,
            path,
            header,
            end_offset,
            faults: Arc::new(NoFaults),
            poisoned: false,
        })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(in crate::storage::wal) const fn header(&self) -> WalHeader {
        self.header
    }

    /// `AfterWalWrite` can report an applied-but-unacknowledged append; its boundary is retained.
    pub(super) fn append_record(&mut self, bytes: &[u8]) -> Result<u64> {
        self.require_ready()?;
        if bytes.is_empty() {
            return Err(corruption("cannot append an empty WAL record"));
        }
        let start = self.end_offset;
        let byte_count = u64::try_from(bytes.len())
            .map_err(|_| corruption("WAL record length does not fit u64"))?;
        let end = start
            .checked_add(byte_count)
            .ok_or_else(|| corruption("WAL record end overflow"))?;
        self.write_at(start, "append WAL record", bytes)?;
        self.end_offset = end;
        self.after_write()?;
        Ok(start)
    }

    /// Builds a complete, prevalidated transaction before performing its one append operation.
    pub(crate) fn append_transaction(
        &mut self,
        transaction_lsn: Lsn,
        previous_lsn: Lsn,
        state: CommitState,
        frames: &[(PageId, u64, Vec<u8>)],
    ) -> Result<Vec<(PageId, u64, u64)>> {
        self.require_ready()?;
        if frames.is_empty() {
            return Err(corruption(
                "WAL transactions require at least one page frame",
            ));
        }
        if transaction_lsn.0 == 0
            || previous_lsn
                .0
                .checked_add(1)
                .is_none_or(|next| next != transaction_lsn.0)
        {
            return Err(corruption("WAL transaction LSN is not consecutive"));
        }

        let frame_count = u32::try_from(frames.len())
            .map_err(|_| corruption("WAL transaction has too many page frames"))?;
        let mut transaction = Vec::new();
        let mut transaction_crc = Crc32c::new();
        let mut versions = Vec::new();
        versions
            .try_reserve_exact(frames.len())
            .map_err(|_| corruption("WAL version allocation failed"))?;
        let mut next_offset = self.end_offset;
        let mut previous_page_id = None;

        for (ordinal, (page_id, generation, payload)) in frames.iter().enumerate() {
            if previous_page_id.is_some_and(|previous| *page_id <= previous) {
                return Err(corruption("WAL page frames are not strictly page ordered"));
            }
            previous_page_id = Some(*page_id);
            let ordinal = u32::try_from(ordinal)
                .map_err(|_| corruption("WAL transaction has too many page frames"))?;
            let record = RecordHeader::page_frame(
                transaction_lsn,
                ordinal,
                *page_id,
                *generation,
                self.header.wal_generation,
                payload,
            )?;
            let encoded_header = record.encode(self.header.page_size)?;
            let end = record_end(next_offset, record.payload_len)?;
            transaction_crc.update(&encoded_header);
            transaction_crc.update(payload);
            transaction.extend_from_slice(&encoded_header);
            transaction.extend_from_slice(payload);
            transaction.extend(std::iter::repeat_n(0, zero_padding_len(record.payload_len)));
            versions.push((*page_id, next_offset, *generation));
            next_offset = end;
        }

        let commit_payload = CommitPayload {
            previous_lsn,
            state,
            frame_count,
            transaction_crc32c: transaction_crc.finish(),
        }
        .encode()?;
        let commit = RecordHeader::commit(
            transaction_lsn,
            frame_count,
            self.header.wal_generation,
            &commit_payload,
        )?;
        let encoded_header = commit.encode(self.header.page_size)?;
        let _transaction_end = record_end(next_offset, commit.payload_len)?;
        transaction.extend_from_slice(&encoded_header);
        transaction.extend_from_slice(&commit_payload);
        transaction.extend(std::iter::repeat_n(0, zero_padding_len(commit.payload_len)));
        self.append_record(&transaction)?;
        Ok(versions)
    }

    /// Revalidates an indexed page-frame record before exposing its immutable decoded image.
    pub(crate) fn read_page_frame(
        &mut self,
        page_id: PageId,
        version: WalVersion,
        page_size: u32,
    ) -> Result<super::page::PageFrame> {
        self.require_ready()?;
        let mut header_bytes = [0; 64];
        self.read_exact_at(version.offset, &mut header_bytes)?;
        let header = RecordHeader::decode(&header_bytes, page_size, self.header.wal_generation)?;
        if header.record_type != RecordType::PageFrame
            || header.transaction_lsn != version.lsn
            || header.page_id != page_id
            || header.generation != version.generation
        {
            return Err(corruption(
                "WAL index points to a different page-frame record",
            ));
        }
        let end = record_end(version.offset, header.payload_len)?;
        if end > self.end_offset {
            return Err(corruption(
                "WAL page-frame record exceeds the file boundary",
            ));
        }
        let payload_len = usize::try_from(header.payload_len)
            .map_err(|_| corruption("WAL page-frame length does not fit memory"))?;
        let mut payload = vec![0; payload_len];
        let payload_offset = version
            .offset
            .checked_add(64)
            .ok_or_else(|| corruption("WAL page-frame payload offset overflow"))?;
        self.read_exact_at(payload_offset, &mut payload)?;
        let padding_len = zero_padding_len(header.payload_len);
        let mut padding = [0; 7];
        if padding_len != 0 {
            self.read_exact_at(
                payload_offset
                    .checked_add(u64::from(header.payload_len))
                    .ok_or_else(|| corruption("WAL page-frame padding offset overflow"))?,
                &mut padding[..padding_len],
            )?;
        }
        header.validate_payload(&payload, &padding[..padding_len])?;
        super::page::PageFrame::decode(payload, page_id, None, Some(version.generation))
    }

    pub(super) fn read_exact_at(&mut self, offset: u64, bytes: &mut [u8]) -> Result<()> {
        self.require_ready()?;
        let byte_count = u64::try_from(bytes.len())
            .map_err(|_| corruption("WAL read length does not fit u64"))?;
        let end = offset
            .checked_add(byte_count)
            .ok_or_else(|| corruption("WAL read end overflow"))?;
        if end > self.end_offset {
            return Err(corruption("WAL read is outside the file boundary"));
        }
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|error| io_error("seek WAL", error))?;
        self.file
            .read_exact(bytes)
            .map_err(|error| read_error("read WAL", error))
    }

    pub(super) fn sync_all(&self) -> Result<()> {
        self.require_ready()?;
        self.faults.check(FaultPoint::BeforeWalSync)?;
        self.file
            .sync_all()
            .map_err(|error| io_error("sync WAL", error))?;
        self.faults.check(FaultPoint::AfterWalSync)
    }

    pub(super) const fn len(&self) -> u64 {
        self.end_offset
    }

    pub(super) fn recover(
        &mut self,
        database_header: &DatabaseHeader,
        mode: OpenMode,
    ) -> Result<recovery::RecoveryResult> {
        recovery::recover(self, database_header, mode)
    }

    /// `AfterWalTruncate` can report an applied-but-unacknowledged truncate; its boundary is retained.
    pub(super) fn truncate(&mut self, len: u64) -> Result<()> {
        self.require_ready()?;
        if !(WAL_HEADER_AREA_LEN..=self.end_offset).contains(&len) {
            return Err(corruption(
                "WAL truncate length is outside the file boundary",
            ));
        }
        self.faults.check(FaultPoint::BeforeWalTruncate)?;
        if let Err(error) = self.file.set_len(len) {
            return Err(self.poison_after_mutating_io("truncate WAL", error));
        }
        self.end_offset = len;
        self.faults.check(FaultPoint::AfterWalTruncate)?;
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn with_faults(mut self, faults: Arc<dyn FaultInjector>) -> Self {
        self.faults = faults;
        self
    }

    fn write_header(&mut self, offset: u64, header: WalHeader) -> Result<()> {
        self.write_at(offset, "write WAL header", &header.encode()?)?;
        self.after_write()
    }

    fn write_at(&mut self, offset: u64, operation: &str, bytes: &[u8]) -> Result<()> {
        self.require_ready()?;
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|error| io_error(operation, error))?;
        self.faults.check(FaultPoint::BeforeWalWrite)?;
        if let Err(error) = self.file.write_all(bytes) {
            return Err(self.poison_after_mutating_io(operation, error));
        }
        Ok(())
    }

    fn after_write(&self) -> Result<()> {
        self.faults.check(FaultPoint::AfterWalWrite)
    }

    fn require_ready(&self) -> Result<()> {
        if self.poisoned {
            return Err(DbError::new(
                ErrorCode::Io,
                "WAL is unusable after a failed mutating I/O operation",
            ));
        }
        Ok(())
    }

    fn poison_after_mutating_io(&mut self, operation: &str, error: io::Error) -> DbError {
        self.poisoned = true;
        if let Ok(metadata) = self.file.metadata() {
            self.end_offset = metadata.len();
        }
        io_error(operation, error)
    }
}

pub(super) fn wal_path(database_path: &Path) -> PathBuf {
    let mut path = database_path.as_os_str().to_os_string();
    path.push("-wal");
    PathBuf::from(path)
}

fn select_header(
    slot_a: &[u8],
    slot_b: &[u8],
    database_header: &DatabaseHeader,
) -> Result<WalHeader> {
    let header_a = matching_header(slot_a, database_header);
    let header_b = matching_header(slot_b, database_header);
    match (header_a, header_b) {
        (None, None) => Err(corruption("no WAL header matches the database identity")),
        (Some(header), None) | (None, Some(header)) => Ok(header),
        (Some(a), Some(b)) if a.header_sequence == b.header_sequence && a != b => Err(corruption(
            "WAL header slots reuse a sequence for different state",
        )),
        (Some(a), Some(b)) if a.header_sequence >= b.header_sequence => Ok(a),
        (Some(_), Some(b)) => Ok(b),
    }
}

fn matching_header(bytes: &[u8], database_header: &DatabaseHeader) -> Option<WalHeader> {
    let header = WalHeader::decode(bytes, database_header.uuid()).ok()?;
    (header.page_size == database_header.page_size()
        && header.wal_generation == database_header.wal_generation()
        && header.base_checkpoint_lsn.0 <= database_header.checkpoint_lsn().0)
        .then_some(header)
}

fn initial_empty(header: &DatabaseHeader) -> bool {
    header.checkpoint_lsn() == Lsn(0)
        && header.page_count() == 1
        && header.catalog_root().0 == 0
        && header.freelist_root().0 == 0
}

fn new_salt() -> [u8; 16] {
    let sequence = NEXT_WAL_SALT.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let first = (nanos as u64) ^ (u64::from(std::process::id()) << 32) ^ sequence;
    let second = ((nanos >> 64) as u64).rotate_left(17) ^ sequence.rotate_left(31);
    let mut salt = [0; 16];
    salt[..8].copy_from_slice(&first.to_le_bytes());
    salt[8..].copy_from_slice(&second.to_le_bytes());
    if salt.iter().all(|&byte| byte == 0) {
        salt[0] = 1;
    }
    salt
}

fn read_error(operation: &str, error: io::Error) -> DbError {
    if error.kind() == io::ErrorKind::UnexpectedEof {
        corruption("WAL data is truncated")
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
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
    };

    use super::{Wal, wal_path};
    use crate::{
        DbError, ErrorCode, Result,
        fault::{FaultInjector, FaultPoint},
        storage::{
            Lsn, PageId,
            file::{DEFAULT_MAX_DATABASE_BYTES, DatabaseFile, OpenMode},
            header::DatabaseHeader,
            test_support::TempDatabase,
        },
    };

    #[test]
    fn create_writes_two_synced_wal_header_slots() {
        let temp = TempDatabase::new("wal-create");
        let header = DatabaseHeader::new(4096, [7; 16]).unwrap();
        let wal = Wal::create(temp.path(), &header).unwrap();
        assert_eq!(fs::metadata(wal.path()).unwrap().len(), 8192);
        assert_eq!(wal.header().header_sequence, 2);
    }

    #[test]
    fn wal_path_preserves_non_ascii_os_strings() {
        assert_eq!(wal_path(Path::new("数据.db")), PathBuf::from("数据.db-wal"));
    }

    #[test]
    fn second_database_file_lock_returns_busy() {
        let temp = TempDatabase::new("lock");
        let first =
            DatabaseFile::create(temp.path(), 4096, [1; 16], DEFAULT_MAX_DATABASE_BYTES).unwrap();
        let error =
            DatabaseFile::open(temp.path(), OpenMode::ReadWrite, DEFAULT_MAX_DATABASE_BYTES)
                .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Busy);
        drop(first);
    }

    #[test]
    fn read_only_database_file_lock_returns_busy() {
        let temp = TempDatabase::new("readonly-lock");
        let created =
            DatabaseFile::create(temp.path(), 4096, [4; 16], DEFAULT_MAX_DATABASE_BYTES).unwrap();
        drop(created);

        let reader =
            DatabaseFile::open(temp.path(), OpenMode::ReadOnly, DEFAULT_MAX_DATABASE_BYTES)
                .unwrap();
        let error =
            DatabaseFile::open(temp.path(), OpenMode::ReadWrite, DEFAULT_MAX_DATABASE_BYTES)
                .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Busy);
        drop(reader);
    }

    #[test]
    fn reopened_wal_selects_the_newer_matching_header_slot() {
        let temp = TempDatabase::new("wal-reopen");
        let header = DatabaseHeader::new(4096, [5; 16]).unwrap();
        let wal = Wal::create(temp.path(), &header).unwrap();
        drop(wal);

        let reopened = Wal::open(temp.path(), &header, OpenMode::ReadWrite).unwrap();
        assert_eq!(reopened.header().header_sequence, 2);
    }

    #[test]
    fn wal_open_rejects_a_different_database_generation() {
        let temp = TempDatabase::new("wal-generation");
        let mut database =
            DatabaseFile::create(temp.path(), 4096, [6; 16], DEFAULT_MAX_DATABASE_BYTES).unwrap();
        let wal = Wal::create(temp.path(), &database.header()).unwrap();
        drop(wal);

        let later_generation = database
            .header()
            .with_checkpoint(Lsn(0), 1, PageId(0), PageId(0), 2)
            .unwrap();
        database.publish_superblock(later_generation).unwrap();

        let error = Wal::open(temp.path(), &database.header(), OpenMode::ReadWrite).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Corruption);
    }

    #[test]
    fn appended_bytes_can_be_read_then_truncated_at_a_checked_boundary() {
        let temp = TempDatabase::new("wal-append-read-truncate");
        let header = DatabaseHeader::new(4096, [7; 16]).unwrap();
        let mut wal = Wal::create(temp.path(), &header).unwrap();
        let start = wal.append_record(&[0x2a, 0x5c]).unwrap();
        let mut bytes = [0; 2];
        wal.read_exact_at(start, &mut bytes).unwrap();
        assert_eq!(bytes, [0x2a, 0x5c]);

        wal.truncate(start).unwrap();
        assert_eq!(wal.len(), start);
        assert_eq!(
            wal.read_exact_at(start, &mut [0; 1]).unwrap_err().code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn wal_write_sync_and_truncate_fail_at_their_named_fault_points() {
        let temp = TempDatabase::new("wal-fault-points");
        let header = DatabaseHeader::new(4096, [8; 16]).unwrap();

        let write_fault = Arc::new(FailAt(FaultPoint::BeforeWalWrite));
        let mut wal = Wal::create(temp.path(), &header)
            .unwrap()
            .with_faults(write_fault);
        assert_eq!(wal.append_record(&[1]).unwrap_err().code(), ErrorCode::Io);
        assert_eq!(wal.len(), 8192);
        drop(wal);

        let sync_fault = Arc::new(FailAt(FaultPoint::BeforeWalSync));
        let wal = Wal::open(temp.path(), &header, OpenMode::ReadWrite)
            .unwrap()
            .with_faults(sync_fault);
        assert_eq!(wal.sync_all().unwrap_err().code(), ErrorCode::Io);
        drop(wal);

        let truncate_fault = Arc::new(FailAt(FaultPoint::BeforeWalTruncate));
        let mut wal = Wal::open(temp.path(), &header, OpenMode::ReadWrite)
            .unwrap()
            .with_faults(truncate_fault);
        assert_eq!(wal.truncate(8192).unwrap_err().code(), ErrorCode::Io);
        assert_eq!(wal.len(), 8192);
    }

    #[test]
    fn after_write_failure_keeps_the_wal_boundary_at_the_persisted_record_end() {
        let temp = TempDatabase::new("wal-after-write");
        let header = DatabaseHeader::new(4096, [9; 16]).unwrap();
        let mut wal = Wal::create(temp.path(), &header)
            .unwrap()
            .with_faults(Arc::new(FailOnceAt::new(FaultPoint::AfterWalWrite)));
        let path = wal.path().to_path_buf();

        assert_eq!(
            wal.append_record(&[0xa1]).unwrap_err().code(),
            ErrorCode::Io
        );
        assert_eq!(wal.len(), 8193);
        assert_eq!(fs::metadata(&path).unwrap().len(), 8193);
        assert_eq!(wal.append_record(&[0xb2]).unwrap(), 8193);
        let mut bytes = [0; 2];
        wal.read_exact_at(8192, &mut bytes).unwrap();
        assert_eq!(bytes, [0xa1, 0xb2]);
    }

    #[test]
    fn after_truncate_failure_keeps_the_wal_boundary_at_the_persisted_length() {
        let temp = TempDatabase::new("wal-after-truncate");
        let header = DatabaseHeader::new(4096, [10; 16]).unwrap();
        let wal = Wal::create(temp.path(), &header).unwrap();
        let mut wal = wal.with_faults(Arc::new(FailOnceAt::new(FaultPoint::AfterWalTruncate)));
        let path = wal.path().to_path_buf();
        wal.append_record(&[0xa1, 0xb2]).unwrap();

        assert_eq!(wal.truncate(8192).unwrap_err().code(), ErrorCode::Io);
        assert_eq!(wal.len(), 8192);
        assert_eq!(fs::metadata(&path).unwrap().len(), 8192);
        assert_eq!(wal.append_record(&[0xc3]).unwrap(), 8192);
        assert_eq!(fs::metadata(&path).unwrap().len(), 8193);
    }

    #[test]
    fn every_failed_wal_create_stage_removes_only_its_exact_wal_path() {
        for (point, occurrence) in [
            (FaultPoint::BeforeWalWrite, 1),
            (FaultPoint::AfterWalWrite, 1),
            (FaultPoint::BeforeWalSync, 1),
            (FaultPoint::AfterWalSync, 1),
            (FaultPoint::BeforeWalWrite, 2),
            (FaultPoint::AfterWalWrite, 2),
            (FaultPoint::BeforeWalSync, 2),
            (FaultPoint::AfterWalSync, 2),
        ] {
            let temp = TempDatabase::new("wal-create-rollback");
            let header = DatabaseHeader::new(4096, [11; 16]).unwrap();
            let path = wal_path(temp.path());
            let mut neighbor = path.as_os_str().to_os_string();
            neighbor.push("-neighbor");
            let neighbor = PathBuf::from(neighbor);
            fs::write(&neighbor, b"retain adjacent file").unwrap();

            let error = Wal::create_with_faults(
                temp.path(),
                &header,
                Arc::new(FailAtOccurrence::new(point, occurrence)),
            )
            .unwrap_err();
            assert_eq!(
                error.code(),
                ErrorCode::Io,
                "{point:?} occurrence {occurrence}"
            );
            assert!(!path.exists(), "{point:?} occurrence {occurrence}");
            assert!(neighbor.exists(), "{point:?} occurrence {occurrence}");
            fs::remove_file(neighbor).unwrap();
        }
    }

    #[test]
    fn failed_os_write_poisoned_wal_rejects_further_io() {
        let temp = TempDatabase::new("wal-poisoned-write");
        let header = DatabaseHeader::new(4096, [12; 16]).unwrap();
        let wal = Wal::create(temp.path(), &header).unwrap();
        drop(wal);
        let mut wal = Wal::open(temp.path(), &header, OpenMode::ReadOnly).unwrap();

        assert_eq!(
            wal.append_record(&[0xa1]).unwrap_err().code(),
            ErrorCode::Io
        );
        assert_eq!(wal.sync_all().unwrap_err().code(), ErrorCode::Io);
        assert_eq!(
            wal.read_exact_at(0, &mut [0; 1]).unwrap_err().code(),
            ErrorCode::Io
        );
    }

    #[test]
    fn missing_wal_rejects_an_existing_database() {
        let temp = TempDatabase::new("missing-wal-existing");
        let mut database =
            DatabaseFile::create(temp.path(), 4096, [2; 16], DEFAULT_MAX_DATABASE_BYTES).unwrap();
        database.extend_to_pages(2).unwrap();
        let header = database
            .header()
            .with_checkpoint(Lsn(1), 2, PageId(0), PageId(0), 1)
            .unwrap();
        database.publish_superblock(header).unwrap();
        let wal = Wal::create(temp.path(), &header).unwrap();
        drop(wal);
        drop(database);

        fs::remove_file(wal_path(temp.path())).unwrap();
        let database =
            DatabaseFile::open(temp.path(), OpenMode::ReadWrite, DEFAULT_MAX_DATABASE_BYTES)
                .unwrap();
        let error = Wal::open(temp.path(), &database.header(), OpenMode::ReadWrite).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Io);
    }

    #[test]
    fn initial_empty_database_can_rebuild_a_missing_wal() {
        let temp = TempDatabase::new("missing-wal-initial");
        let created =
            DatabaseFile::create(temp.path(), 4096, [3; 16], DEFAULT_MAX_DATABASE_BYTES).unwrap();
        let header = created.header();
        drop(created);

        let database =
            DatabaseFile::open(temp.path(), OpenMode::ReadWrite, DEFAULT_MAX_DATABASE_BYTES)
                .unwrap();
        let wal = Wal::open(temp.path(), &database.header(), OpenMode::ReadWrite).unwrap();
        assert_eq!(wal.header().base_checkpoint_lsn, Lsn(0));
        drop(wal);
        drop(database);

        fs::remove_file(wal_path(temp.path())).unwrap();
        let database_bytes = fs::read(temp.path()).unwrap();
        let database =
            DatabaseFile::open(temp.path(), OpenMode::ReadOnly, DEFAULT_MAX_DATABASE_BYTES)
                .unwrap();
        let error = Wal::open(temp.path(), &header, OpenMode::ReadOnly).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Io);
        assert!(!wal_path(temp.path()).exists());
        drop(database);
        assert_eq!(fs::read(temp.path()).unwrap(), database_bytes);
    }

    struct FailAt(FaultPoint);

    impl FaultInjector for FailAt {
        fn check(&self, point: FaultPoint) -> Result<()> {
            if point == self.0 {
                return Err(DbError::new(ErrorCode::Io, "injected WAL I/O fault"));
            }
            Ok(())
        }
    }

    struct FailOnceAt {
        point: FaultPoint,
        fired: Mutex<bool>,
    }

    impl FailOnceAt {
        fn new(point: FaultPoint) -> Self {
            Self {
                point,
                fired: Mutex::new(false),
            }
        }
    }

    impl FaultInjector for FailOnceAt {
        fn check(&self, point: FaultPoint) -> Result<()> {
            let mut fired = self.fired.lock().unwrap();
            if point == self.point && !*fired {
                *fired = true;
                return Err(DbError::new(ErrorCode::Io, "injected WAL I/O fault"));
            }
            Ok(())
        }
    }

    struct FailAtOccurrence {
        point: FaultPoint,
        occurrence: usize,
        seen: Mutex<usize>,
    }

    impl FailAtOccurrence {
        fn new(point: FaultPoint, occurrence: usize) -> Self {
            Self {
                point,
                occurrence,
                seen: Mutex::new(0),
            }
        }
    }

    impl FaultInjector for FailAtOccurrence {
        fn check(&self, point: FaultPoint) -> Result<()> {
            if point == self.point {
                let mut seen = self.seen.lock().unwrap();
                *seen += 1;
                if *seen == self.occurrence {
                    return Err(DbError::new(ErrorCode::Io, "injected WAL I/O fault"));
                }
            }
            Ok(())
        }
    }
}
