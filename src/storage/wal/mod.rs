pub(super) mod format;

use std::{
    fmt,
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use super::{Lsn, file::OpenMode, header::DatabaseHeader};
use crate::{
    DbError, ErrorCode, Result,
    fault::{FaultInjector, FaultPoint, NoFaults},
};

use self::format::{WAL_HEADER_SLOT_LEN, WalHeader};

const WAL_HEADER_AREA_LEN: u64 = (WAL_HEADER_SLOT_LEN as u64) * 2;

static NEXT_WAL_SALT: AtomicU64 = AtomicU64::new(1);

pub(super) struct Wal {
    file: File,
    path: PathBuf,
    header: WalHeader,
    end_offset: u64,
    faults: Arc<dyn FaultInjector>,
}

impl fmt::Debug for Wal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Wal")
            .field("path", &self.path)
            .field("header", &self.header)
            .field("end_offset", &self.end_offset)
            .finish_non_exhaustive()
    }
}

impl Wal {
    pub(super) fn create(database_path: &Path, database_header: &DatabaseHeader) -> Result<Self> {
        let path = wal_path(database_path);
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| io_error("create WAL", error))?;
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
            faults: Arc::new(NoFaults),
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
        })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(in crate::storage::wal) const fn header(&self) -> WalHeader {
        self.header
    }

    pub(super) fn append_record(&mut self, bytes: &[u8]) -> Result<u64> {
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
        Ok(start)
    }

    pub(super) fn read_exact_at(&mut self, offset: u64, bytes: &mut [u8]) -> Result<()> {
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
        self.faults.check(FaultPoint::BeforeWalSync)?;
        self.file
            .sync_all()
            .map_err(|error| io_error("sync WAL", error))?;
        self.faults.check(FaultPoint::AfterWalSync)
    }

    pub(super) const fn len(&self) -> u64 {
        self.end_offset
    }

    pub(super) fn truncate(&mut self, len: u64) -> Result<()> {
        if !(WAL_HEADER_AREA_LEN..=self.end_offset).contains(&len) {
            return Err(corruption(
                "WAL truncate length is outside the file boundary",
            ));
        }
        self.faults.check(FaultPoint::BeforeWalTruncate)?;
        self.file
            .set_len(len)
            .map_err(|error| io_error("truncate WAL", error))?;
        self.faults.check(FaultPoint::AfterWalTruncate)?;
        self.end_offset = len;
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn with_faults(mut self, faults: Arc<dyn FaultInjector>) -> Self {
        self.faults = faults;
        self
    }

    fn write_header(&mut self, offset: u64, header: WalHeader) -> Result<()> {
        self.write_at(offset, "write WAL header", &header.encode()?)
    }

    fn write_at(&mut self, offset: u64, operation: &str, bytes: &[u8]) -> Result<()> {
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|error| io_error(operation, error))?;
        self.faults.check(FaultPoint::BeforeWalWrite)?;
        self.file
            .write_all(bytes)
            .map_err(|error| io_error(operation, error))?;
        self.faults.check(FaultPoint::AfterWalWrite)
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
        sync::Arc,
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
}
