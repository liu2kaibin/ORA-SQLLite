use super::{
    WAL_HEADER_AREA_LEN, Wal,
    format::{CommitPayload, CommitState, RecordHeader, RecordType, record_end, zero_padding_len},
    index::WalIndex,
};
use crate::{
    DbError, ErrorCode, Result,
    storage::{
        Lsn, PageId, checksum::Crc32c, file::OpenMode, header::DatabaseHeader, page::PageFrame,
    },
};

#[derive(Debug)]
pub(crate) struct RecoveryResult {
    pub(crate) latest_lsn: Lsn,
    pub(crate) latest_state: CommitState,
    pub(crate) index: WalIndex,
    pub(crate) committed_end: u64,
    pub(crate) truncate_to: Option<u64>,
}

pub(super) fn recover(
    wal: &mut Wal,
    database_header: &DatabaseHeader,
    mode: OpenMode,
) -> Result<RecoveryResult> {
    let result = scan(wal, database_header)?;
    if mode == OpenMode::ReadWrite {
        if let Some(truncate_to) = result.truncate_to {
            wal.truncate(truncate_to)?;
            wal.sync_all()?;
        }
    }
    Ok(result)
}

fn scan(wal: &mut Wal, database_header: &DatabaseHeader) -> Result<RecoveryResult> {
    let wal_header = wal.header();
    let mut offset = WAL_HEADER_AREA_LEN;
    let mut committed_end = WAL_HEADER_AREA_LEN;
    let mut latest_lsn = database_header.checkpoint_lsn();
    let mut latest_state = checkpoint_state(database_header);
    let mut index = WalIndex::default();
    let mut transactions = TransactionScanner::new(Some(wal_header.base_checkpoint_lsn));

    loop {
        let header_bytes = match read_header(wal, offset)? {
            HeaderRead::End => {
                return Ok(RecoveryResult {
                    latest_lsn,
                    latest_state,
                    index,
                    committed_end,
                    truncate_to: transactions.pending_start(),
                });
            }
            HeaderRead::Incomplete => {
                return Ok(RecoveryResult {
                    latest_lsn,
                    latest_state,
                    index,
                    committed_end,
                    truncate_to: Some(transactions.pending_start().unwrap_or(offset)),
                });
            }
            HeaderRead::Complete(bytes) => bytes,
        };

        if offset == WAL_HEADER_AREA_LEN {
            if let Some(old_generation) =
                previous_generation(&header_bytes, wal_header.wal_generation)
            {
                verify_old_generation_suffix(wal, database_header, old_generation)?;
                return Ok(RecoveryResult {
                    latest_lsn,
                    latest_state,
                    index,
                    committed_end,
                    truncate_to: Some(WAL_HEADER_AREA_LEN),
                });
            }
        }

        let record = read_record(
            wal,
            offset,
            header_bytes,
            database_header.page_size(),
            wal_header.wal_generation,
        )?;
        let Some(record) = record else {
            return Ok(RecoveryResult {
                latest_lsn,
                latest_state,
                index,
                committed_end,
                truncate_to: Some(transactions.pending_start().unwrap_or(offset)),
            });
        };
        let end = record.end;
        if let Some(transaction) = transactions.consume(record, offset)? {
            if transaction.lsn == database_header.checkpoint_lsn()
                && transaction.state != checkpoint_state(database_header)
            {
                return Err(corruption(
                    "WAL commit disagrees with the matching checkpoint state",
                ));
            }
            committed_end = end;
            if transaction.lsn > database_header.checkpoint_lsn() {
                if transaction.state.page_count < latest_state.page_count {
                    return Err(corruption("WAL commit shrinks the checkpoint page count"));
                }
                index.publish(
                    transaction.lsn,
                    Some(transaction.state),
                    &transaction.frames,
                )?;
                latest_lsn = transaction.lsn;
                latest_state = transaction.state;
            }
        }
        offset = end;
    }
}

struct PendingTransaction {
    start: u64,
    lsn: Lsn,
    frames: Vec<(PageId, u64, u64)>,
    last_page_id: Option<PageId>,
    transaction_crc: Crc32c,
}

impl PendingTransaction {
    fn new(start: u64, lsn: Lsn) -> Self {
        Self {
            start,
            lsn,
            frames: Vec::new(),
            last_page_id: None,
            transaction_crc: Crc32c::new(),
        }
    }
}

struct ValidatedTransaction {
    lsn: Lsn,
    state: CommitState,
    frames: Vec<(PageId, u64, u64)>,
}

struct TransactionScanner {
    previous_lsn: Option<Lsn>,
    previous_state: Option<CommitState>,
    pending: Option<PendingTransaction>,
}

impl TransactionScanner {
    fn new(previous_lsn: Option<Lsn>) -> Self {
        Self {
            previous_lsn,
            previous_state: None,
            pending: None,
        }
    }

    fn pending_start(&self) -> Option<u64> {
        self.pending.as_ref().map(|transaction| transaction.start)
    }

    fn consume(
        &mut self,
        record: DecodedRecord,
        offset: u64,
    ) -> Result<Option<ValidatedTransaction>> {
        if self
            .previous_lsn
            .is_some_and(|base_lsn| record.header.transaction_lsn <= base_lsn)
        {
            return Err(corruption(
                "WAL record LSN does not follow the base checkpoint",
            ));
        }

        match record.header.record_type {
            RecordType::PageFrame => {
                let transaction = match self.pending.as_mut() {
                    Some(transaction) => {
                        if record.header.transaction_lsn != transaction.lsn {
                            return Err(corruption("WAL page frame changes LSN before commit"));
                        }
                        transaction
                    }
                    None => {
                        if let Some(previous_lsn) = self.previous_lsn {
                            if record.header.transaction_lsn != next_lsn(previous_lsn)? {
                                return Err(corruption("WAL transaction LSN is not consecutive"));
                            }
                        }
                        self.pending.insert(PendingTransaction::new(
                            offset,
                            record.header.transaction_lsn,
                        ))
                    }
                };
                let ordinal = u32::try_from(transaction.frames.len())
                    .map_err(|_| corruption("WAL transaction has too many page frames"))?;
                if record.header.ordinal != ordinal {
                    return Err(corruption("WAL page frame ordinal is not consecutive"));
                }
                if transaction
                    .last_page_id
                    .is_some_and(|last_page_id| record.header.page_id <= last_page_id)
                {
                    return Err(corruption("WAL page frames are not strictly page ordered"));
                }
                transaction.transaction_crc.update(&record.header_bytes);
                transaction.transaction_crc.update(&record.payload);
                PageFrame::decode(
                    record.payload,
                    record.header.page_id,
                    None,
                    Some(record.header.generation),
                )?;
                transaction
                    .frames
                    .push((record.header.page_id, offset, record.header.generation));
                transaction.last_page_id = Some(record.header.page_id);
                Ok(None)
            }
            RecordType::Commit => {
                let transaction = self.pending.take().ok_or_else(|| {
                    corruption("WAL commit record has no preceding page-frame records")
                })?;
                if record.header.transaction_lsn != transaction.lsn {
                    return Err(corruption("WAL commit LSN does not match its page frames"));
                }
                let frame_count = u32::try_from(transaction.frames.len())
                    .map_err(|_| corruption("WAL transaction has too many page frames"))?;
                if record.header.ordinal != frame_count {
                    return Err(corruption(
                        "WAL commit ordinal does not match its page frames",
                    ));
                }
                let commit = CommitPayload::decode(&record.payload)?;
                if self
                    .previous_lsn
                    .is_some_and(|previous_lsn| commit.previous_lsn != previous_lsn)
                {
                    return Err(corruption("WAL commit previous LSN is not consecutive"));
                }
                if transaction.lsn != next_lsn(commit.previous_lsn)? {
                    return Err(corruption("WAL transaction LSN is not consecutive"));
                }
                if commit.frame_count != frame_count {
                    return Err(corruption(
                        "WAL commit frame count does not match its page frames",
                    ));
                }
                if commit.transaction_crc32c != transaction.transaction_crc.finish() {
                    return Err(corruption(
                        "WAL transaction checksum does not match its page frames",
                    ));
                }

                if transaction
                    .frames
                    .iter()
                    .any(|(page_id, _, _)| page_id.0 >= commit.state.page_count)
                {
                    return Err(corruption(
                        "WAL page frame lies outside the commit page count",
                    ));
                }
                if self.previous_state.is_some_and(|previous| {
                    commit.state.page_count < previous.page_count
                        || commit.state.feature_bits != previous.feature_bits
                }) {
                    return Err(corruption(
                        "WAL commit regresses the logical database state",
                    ));
                }

                self.previous_lsn = Some(transaction.lsn);
                self.previous_state = Some(commit.state);
                Ok(Some(ValidatedTransaction {
                    lsn: transaction.lsn,
                    state: commit.state,
                    frames: transaction.frames,
                }))
            }
        }
    }
}

struct DecodedRecord {
    header: RecordHeader,
    header_bytes: [u8; 64],
    payload: Vec<u8>,
    end: u64,
}

enum HeaderRead {
    End,
    Incomplete,
    Complete([u8; 64]),
}

fn read_header(wal: &mut Wal, offset: u64) -> Result<HeaderRead> {
    if offset > wal.len() {
        return Err(corruption("WAL scan offset is outside the file boundary"));
    }
    let remaining = wal.len() - offset;
    if remaining == 0 {
        return Ok(HeaderRead::End);
    }
    if remaining < 64 {
        return Ok(HeaderRead::Incomplete);
    }
    let mut bytes = [0; 64];
    wal.read_exact_at(offset, &mut bytes)?;
    Ok(HeaderRead::Complete(bytes))
}

fn read_record(
    wal: &mut Wal,
    offset: u64,
    header_bytes: [u8; 64],
    page_size: u32,
    expected_wal_generation: u64,
) -> Result<Option<DecodedRecord>> {
    let header = RecordHeader::decode(&header_bytes, page_size, expected_wal_generation)?;
    match header.record_type {
        RecordType::PageFrame if header.payload_len > page_size => {
            return Err(corruption(
                "WAL page-frame payload exceeds the database page size",
            ));
        }
        RecordType::Commit if header.payload_len > 64 => {
            return Err(corruption("WAL commit payload exceeds its fixed size"));
        }
        _ => {}
    }
    let end = record_end(offset, header.payload_len)?;
    if end > wal.len() {
        return Ok(None);
    }
    let payload_len = usize::try_from(header.payload_len)
        .map_err(|_| corruption("WAL record payload length does not fit memory"))?;
    let payload_offset = offset
        .checked_add(64)
        .ok_or_else(|| corruption("WAL payload offset overflow"))?;
    let mut payload = vec![0; payload_len];
    wal.read_exact_at(payload_offset, &mut payload)?;
    let padding_len = zero_padding_len(header.payload_len);
    let mut padding = [0; 7];
    if padding_len != 0 {
        let padding_offset = payload_offset
            .checked_add(u64::from(header.payload_len))
            .ok_or_else(|| corruption("WAL padding offset overflow"))?;
        wal.read_exact_at(padding_offset, &mut padding[..padding_len])?;
    }
    header.validate_payload(&payload, &padding[..padding_len])?;
    Ok(Some(DecodedRecord {
        header,
        header_bytes,
        payload,
        end,
    }))
}

fn verify_old_generation_suffix(
    wal: &mut Wal,
    database_header: &DatabaseHeader,
    old_generation: u64,
) -> Result<()> {
    let mut offset = WAL_HEADER_AREA_LEN;
    let mut transactions = TransactionScanner::new(None);
    loop {
        let header_bytes = match read_header(wal, offset)? {
            HeaderRead::End => {
                if transactions.pending_start().is_some() {
                    return Err(corruption(
                        "old-generation WAL suffix ends without a commit",
                    ));
                }
                if transactions
                    .previous_lsn
                    .is_some_and(|lsn| lsn > database_header.checkpoint_lsn())
                    || transactions.previous_state.is_some_and(|state| {
                        state.page_count > database_header.page_count()
                            || (transactions.previous_lsn == Some(database_header.checkpoint_lsn())
                                && state != checkpoint_state(database_header))
                    })
                {
                    return Err(corruption(
                        "old-generation WAL state exceeds or disagrees with the checkpoint",
                    ));
                }
                return Ok(());
            }
            HeaderRead::Incomplete => {
                return Err(corruption(
                    "old-generation WAL suffix ends inside a record header",
                ));
            }
            HeaderRead::Complete(bytes) => bytes,
        };
        if raw_wal_generation(&header_bytes) != old_generation {
            return Err(corruption(
                "old-generation WAL suffix contains another generation",
            ));
        }
        let record = read_record(
            wal,
            offset,
            header_bytes,
            database_header.page_size(),
            old_generation,
        )?
        .ok_or_else(|| corruption("old-generation WAL suffix ends inside a record"))?;
        let end = record.end;
        let _ = transactions.consume(record, offset)?;
        offset = end;
    }
}

fn previous_generation(header_bytes: &[u8; 64], current_generation: u64) -> Option<u64> {
    current_generation
        .checked_sub(1)
        .filter(|&previous_generation| raw_wal_generation(header_bytes) == previous_generation)
}

fn raw_wal_generation(header_bytes: &[u8; 64]) -> u64 {
    u64::from_le_bytes(
        header_bytes[56..64]
            .try_into()
            .expect("fixed WAL header generation field"),
    )
}

fn checkpoint_state(header: &DatabaseHeader) -> CommitState {
    CommitState {
        page_count: header.page_count(),
        catalog_root: header.catalog_root(),
        freelist_root: header.freelist_root(),
        feature_bits: header.features(),
    }
}

fn next_lsn(previous_lsn: Lsn) -> Result<Lsn> {
    previous_lsn
        .0
        .checked_add(1)
        .map(Lsn)
        .ok_or_else(|| corruption("WAL LSN overflow"))
}

fn corruption(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Corruption, message)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::{Arc, Mutex},
    };

    use crate::{
        ErrorCode, Result,
        fault::{FaultInjector, FaultPoint},
        storage::{
            Lsn, PageId, PageRef,
            checksum::{Crc32c, crc32c},
            file::OpenMode,
            header::DatabaseHeader,
            page::{PageFrame, PageType},
            test_support::{TempDatabase, write_wal_fixture},
            wal::format::{CommitPayload, CommitState, RecordHeader, WalHeader, zero_padding_len},
            wal::{WAL_HEADER_AREA_LEN, Wal},
        },
    };

    const PAGE_SIZE: u32 = 4096;
    const WAL_GENERATION: u64 = 1;
    const RECORD_AREA_START: u64 = 8192;
    const PAGE_RECORD_LEN: usize = 64 + PAGE_SIZE as usize;
    const COMMIT_RECORD_LEN: usize = 128;

    #[test]
    #[ignore = "exhaustive real-file truncation of every page-frame/commit byte"]
    fn phase_3_every_record_byte_truncation_matrix() {
        let database = DatabaseHeader::new(PAGE_SIZE, [0x39; 16]).unwrap();
        let first = encoded_transaction(1, 0, &[(PageId(1), 1)], commit_state(2, 0, 0));
        let second = encoded_transaction(2, 1, &[(PageId(1), 2)], commit_state(2, 0, 0));
        assert_eq!(second.len(), 4288); // 64+4096 PageFrame, 64+64 Commit, zero padding
        let (temp, wal) = fixture_wal(&database, 1, Lsn(0), &first, OpenMode::ReadWrite);
        let path = wal.path().to_owned();
        drop(wal);
        let prefix = fs::read(&path).unwrap();
        for mode in [OpenMode::ReadOnly, OpenMode::ReadWrite] {
            for cut in 0..=4288 {
                let bytes = [&prefix[..], &second[..cut]].concat();
                fs::write(&path, &bytes).unwrap();
                let mut wal = Wal::open(temp.path(), &database, mode).unwrap();
                let result = wal.recover(&database, mode).unwrap();
                assert_eq!(
                    result.latest_lsn,
                    Lsn(if cut == 4288 { 2 } else { 1 }),
                    "{mode:?}/{cut}"
                );
                let after = fs::read(&path).unwrap();
                assert_eq!(
                    after,
                    if mode == OpenMode::ReadWrite && cut < 4288 {
                        prefix.clone()
                    } else {
                        bytes
                    },
                    "{mode:?}/{cut}"
                );
            }
        }
        eprintln!(
            "8578 real-file recovery cases (4289 byte boundaries x RO/RW), all passed; legal padding length is zero"
        );
    }

    #[test]
    fn final_i1_full_crc_out_of_range_frames_and_shrink_reject_without_mutation() {
        use crate::storage::pager::{Pager, PagerConfig};
        for case in ["out-of-range", "shrink", "checkpoint-shrink"] {
            for old_generation in [false, true] {
                if case == "checkpoint-shrink" && !old_generation {
                    continue;
                }
                for mode in [OpenMode::ReadOnly, OpenMode::ReadWrite] {
                    let mut records = encoded_transaction(
                        1,
                        0,
                        &[(PageId(1), 1)],
                        commit_state(if case == "out-of-range" { 1 } else { 3 }, 0, 0),
                    );
                    if case == "shrink" {
                        records.extend(encoded_transaction(
                            2,
                            1,
                            &[(PageId(1), 2)],
                            commit_state(2, 0, 0),
                        ));
                    }
                    let checkpoint = if old_generation { 2 } else { 0 };
                    let generation = if old_generation { 2 } else { 1 };
                    let database = DatabaseHeader::new(PAGE_SIZE, [0x31; 16])
                        .unwrap()
                        .with_checkpoint(Lsn(checkpoint), 1, PageId(0), PageId(0), generation)
                        .unwrap()
                        .with_checkpoint(Lsn(checkpoint), 1, PageId(0), PageId(0), generation)
                        .unwrap();
                    let (temp, wal) =
                        fixture_wal(&database, generation, Lsn(checkpoint), &records, mode);
                    let wal_path = wal.path().to_owned();
                    drop(wal);
                    let main = [database.encode_slot(), database.encode_slot()].concat();
                    fs::write(temp.path(), &main).unwrap();
                    let before_wal = fs::read(&wal_path).unwrap();
                    let error = match Pager::open(
                        temp.path(),
                        mode,
                        PagerConfig::test_default(PAGE_SIZE),
                    ) {
                        Ok(_) => panic!("accepted {case}, old={old_generation}, mode={mode:?}"),
                        Err(error) => error,
                    };
                    assert_eq!(
                        error.code(),
                        ErrorCode::Corruption,
                        "{case}, old={old_generation}, mode={mode:?}"
                    );
                    assert_eq!(fs::read(temp.path()).unwrap(), main);
                    assert_eq!(fs::read(&wal_path).unwrap(), before_wal);
                }
            }
        }
    }

    #[test]
    fn recovery_publishes_two_valid_consecutive_commits_at_hand_derived_offsets() {
        let database = DatabaseHeader::new(PAGE_SIZE, [0x31; 16]).expect("database header");
        let first_state = commit_state(2, 1, 0);
        let second_state = commit_state(3, 2, 0);
        let first = encoded_transaction(1, 0, &[(PageId(1), 5)], first_state);
        let second = encoded_transaction(2, 1, &[(PageId(1), 6)], second_state);
        let mut records = first.clone();
        records.extend_from_slice(&second);

        let (temp, mut wal) = fixture_wal(
            &database,
            WAL_GENERATION,
            database.checkpoint_lsn(),
            &records,
            OpenMode::ReadWrite,
        );
        let result = wal
            .recover(&database, OpenMode::ReadWrite)
            .expect("recover valid commits");

        assert_eq!(result.latest_lsn, Lsn(2));
        assert_eq!(result.latest_state, second_state);
        assert_eq!(result.committed_end, RECORD_AREA_START + 2 * 4_288);
        assert_eq!(result.truncate_to, None);
        assert_eq!(
            result.index.visible_version(PageId(1), Lsn(1)),
            Some(super::super::index::WalVersion {
                lsn: Lsn(1),
                offset: RECORD_AREA_START,
                generation: 5
            })
        );
        assert_eq!(
            result.index.visible_version(PageId(1), Lsn(2)),
            Some(super::super::index::WalVersion {
                lsn: Lsn(2),
                offset: RECORD_AREA_START + 4_288,
                generation: 6
            })
        );
        assert_eq!(
            fs::metadata(wal.path()).unwrap().len(),
            RECORD_AREA_START + 8_576
        );
        drop(wal);
        drop(temp);
    }

    #[test]
    fn recovery_truncates_only_a_final_incomplete_transaction_and_syncs_in_read_write_mode() {
        let database = DatabaseHeader::new(PAGE_SIZE, [0x32; 16]).expect("database header");
        let committed = encoded_transaction(1, 0, &[(PageId(1), 5)], commit_state(2, 1, 0));
        let incomplete = encoded_transaction(2, 1, &[(PageId(1), 6)], commit_state(2, 1, 0));
        let partial_second = &incomplete[..PAGE_RECORD_LEN + 17];
        let mut records = committed.clone();
        records.extend_from_slice(partial_second);
        let (temp, wal) = fixture_wal(
            &database,
            WAL_GENERATION,
            database.checkpoint_lsn(),
            &records,
            OpenMode::ReadWrite,
        );
        let observed = Arc::new(RecordingFaults::default());
        let mut wal = wal.with_faults(observed.clone());

        let result = wal
            .recover(&database, OpenMode::ReadWrite)
            .expect("recover partial final transaction");

        assert_eq!(result.latest_lsn, Lsn(1));
        assert_eq!(result.committed_end, RECORD_AREA_START + 4_288);
        assert_eq!(result.truncate_to, Some(RECORD_AREA_START + 4_288));
        assert_eq!(
            fs::metadata(wal.path()).unwrap().len(),
            RECORD_AREA_START + 4_288
        );
        assert_eq!(
            observed.observed(),
            vec![
                FaultPoint::BeforeWalTruncate,
                FaultPoint::AfterWalTruncate,
                FaultPoint::BeforeWalSync,
                FaultPoint::AfterWalSync,
            ]
        );
        drop(wal);
        drop(temp);
    }

    #[test]
    fn recovery_table_classifies_final_eof_tails_without_mutating_read_only_wals() {
        let database = DatabaseHeader::new(PAGE_SIZE, [0x33; 16]).expect("database header");
        let transaction = encoded_transaction(1, 0, &[(PageId(1), 5)], commit_state(2, 1, 0));
        let frame = &transaction[..PAGE_RECORD_LEN];
        let cases = [
            (
                "record header",
                transaction[..17].to_vec(),
                RECORD_AREA_START,
            ),
            (
                "record payload",
                transaction[..64 + 211].to_vec(),
                RECORD_AREA_START,
            ),
            (
                "complete frame without commit",
                frame.to_vec(),
                RECORD_AREA_START,
            ),
            // Legal WAL page and commit payloads are always 8-byte aligned. This final byte is
            // therefore the only reachable partial suffix after a complete PageFrame.
            (
                "post-frame partial suffix",
                [frame, &[0]].concat(),
                RECORD_AREA_START,
            ),
        ];

        for (name, records, expected_truncate) in cases {
            let (temp, mut wal) = fixture_wal(
                &database,
                WAL_GENERATION,
                database.checkpoint_lsn(),
                &records,
                OpenMode::ReadOnly,
            );
            let before = fs::metadata(wal.path()).unwrap().len();
            let result = wal.recover(&database, OpenMode::ReadOnly).expect(name);

            assert_eq!(result.latest_lsn, Lsn(0), "{name}");
            assert_eq!(result.committed_end, RECORD_AREA_START, "{name}");
            assert_eq!(result.truncate_to, Some(expected_truncate), "{name}");
            assert_eq!(fs::metadata(wal.path()).unwrap().len(), before, "{name}");
            drop(wal);
            drop(temp);
        }
    }

    #[test]
    fn recovery_rejects_complete_corruption_without_truncating_it() {
        let database = DatabaseHeader::new(PAGE_SIZE, [0x34; 16]).expect("database header");
        let state = commit_state(4, 1, 0);
        let transaction = encoded_transaction(1, 0, &[(PageId(1), 5)], state);
        let mut bad_header_crc = transaction.clone();
        bad_header_crc[52] ^= 1;
        let mut bad_payload_crc = transaction.clone();
        bad_payload_crc[64 + 100] ^= 1;
        let mut bad_transaction_crc = transaction.clone();
        let commit = bad_transaction_crc.len() - COMMIT_RECORD_LEN;
        bad_transaction_crc[commit + 64 + 36] ^= 1;
        restamp_record(&mut bad_transaction_crc, commit);
        let mut bad_ordinal = transaction.clone();
        bad_ordinal[24..28].copy_from_slice(&1_u32.to_le_bytes());
        restamp_record_header(&mut bad_ordinal, 0);
        let bad_order = encoded_transaction(1, 0, &[(PageId(2), 5), (PageId(1), 6)], state);
        let bad_duplicate = encoded_transaction(1, 0, &[(PageId(1), 5), (PageId(1), 6)], state);
        let mut bad_previous_lsn = transaction.clone();
        let commit = bad_previous_lsn.len() - COMMIT_RECORD_LEN;
        bad_previous_lsn[commit + 64..commit + 72].copy_from_slice(&9_u64.to_le_bytes());
        restamp_record(&mut bad_previous_lsn, commit);
        let mut bad_generation = transaction;
        bad_generation[56..64].copy_from_slice(&(WAL_GENERATION + 2).to_le_bytes());
        restamp_record_header(&mut bad_generation, 0);
        let cases = [
            ("header CRC", bad_header_crc),
            ("payload CRC", bad_payload_crc),
            ("transaction CRC", bad_transaction_crc),
            ("ordinal", bad_ordinal),
            ("page order", bad_order),
            ("duplicate page", bad_duplicate),
            ("previous LSN", bad_previous_lsn),
            ("generation", bad_generation),
        ];

        for (name, records) in cases {
            let (temp, mut wal) = fixture_wal(
                &database,
                WAL_GENERATION,
                database.checkpoint_lsn(),
                &records,
                OpenMode::ReadWrite,
            );
            let before = fs::metadata(wal.path()).unwrap().len();
            assert_eq!(
                wal.recover(&database, OpenMode::ReadWrite)
                    .expect_err(name)
                    .code(),
                ErrorCode::Corruption,
                "{name}"
            );
            assert_eq!(fs::metadata(wal.path()).unwrap().len(), before, "{name}");
            drop(wal);
            drop(temp);
        }
    }

    #[test]
    fn recovery_discards_an_entire_proven_old_generation_record_area_after_reset() {
        let database = DatabaseHeader::new(PAGE_SIZE, [0x35; 16])
            .expect("database header")
            .with_checkpoint(Lsn(1), 2, PageId(1), PageId(0), 2)
            .expect("reset superblock");
        let old_records =
            encoded_transaction_for_generation(1, 0, &[(PageId(1), 5)], commit_state(2, 1, 0), 1);
        let (temp, mut wal) = fixture_wal(&database, 2, Lsn(1), &old_records, OpenMode::ReadWrite);

        let result = wal
            .recover(&database, OpenMode::ReadWrite)
            .expect("discard old suffix");

        assert_eq!(result.latest_lsn, Lsn(1));
        assert_eq!(result.latest_state, commit_state(2, 1, 0));
        assert_eq!(result.committed_end, WAL_HEADER_AREA_LEN);
        assert_eq!(result.truncate_to, Some(WAL_HEADER_AREA_LEN));
        assert_eq!(fs::metadata(wal.path()).unwrap().len(), WAL_HEADER_AREA_LEN);
        drop(wal);
        drop(temp);
    }

    #[test]
    fn recovery_rejects_current_generation_records_after_an_old_generation_suffix() {
        let database = DatabaseHeader::new(PAGE_SIZE, [0x36; 16])
            .expect("database header")
            .with_checkpoint(Lsn(1), 2, PageId(1), PageId(0), 2)
            .expect("reset superblock");
        let mut records =
            encoded_transaction_for_generation(1, 0, &[(PageId(1), 5)], commit_state(2, 1, 0), 1);
        records.extend_from_slice(&encoded_transaction_for_generation(
            2,
            1,
            &[(PageId(1), 6)],
            commit_state(2, 1, 0),
            2,
        ));
        let (temp, mut wal) = fixture_wal(&database, 2, Lsn(1), &records, OpenMode::ReadWrite);
        let before = fs::metadata(wal.path()).unwrap().len();

        assert_eq!(
            wal.recover(&database, OpenMode::ReadWrite)
                .expect_err("mixed old/current generations")
                .code(),
            ErrorCode::Corruption
        );
        assert_eq!(fs::metadata(wal.path()).unwrap().len(), before);
        drop(wal);
        drop(temp);
    }

    #[test]
    fn recovery_rejects_corrupt_old_generation_suffix_without_truncating_in_any_mode() {
        let database = DatabaseHeader::new(PAGE_SIZE, [0x37; 16])
            .expect("database header")
            .with_checkpoint(Lsn(1), 2, PageId(1), PageId(0), 2)
            .expect("reset superblock");
        let mut old_records =
            encoded_transaction_for_generation(1, 0, &[(PageId(1), 5)], commit_state(2, 1, 0), 1);
        let commit = old_records.len() - COMMIT_RECORD_LEN;
        old_records[commit + 64 + 36] ^= 1;
        restamp_record(&mut old_records, commit);

        for mode in [OpenMode::ReadWrite, OpenMode::ReadOnly] {
            let (temp, mut wal) = fixture_wal(&database, 2, Lsn(1), &old_records, mode);
            let before = fs::metadata(wal.path()).expect("fixture length").len();

            assert_eq!(
                wal.recover(&database, mode)
                    .expect_err("corrupt old transaction must be rejected")
                    .code(),
                ErrorCode::Corruption,
                "{mode:?}"
            );
            assert_eq!(
                fs::metadata(wal.path()).expect("fixture length").len(),
                before
            );
            drop(wal);
            drop(temp);
        }
    }

    fn fixture_wal(
        database: &DatabaseHeader,
        wal_generation: u64,
        base_checkpoint_lsn: Lsn,
        records: &[u8],
        open_mode: OpenMode,
    ) -> (TempDatabase, Wal) {
        let temp = TempDatabase::new("wal-recovery");
        let first = WalHeader {
            page_size: database.page_size(),
            database_uuid: database.uuid(),
            wal_generation,
            base_checkpoint_lsn,
            salt: [0x57; 16],
            header_sequence: 1,
        };
        let second = WalHeader {
            header_sequence: 2,
            ..first
        };
        let mut bytes = first.encode().expect("first WAL header").to_vec();
        bytes.extend_from_slice(&second.encode().expect("second WAL header"));
        bytes.extend_from_slice(records);
        write_wal_fixture(temp.path(), &bytes).expect("write WAL fixture");
        let wal = Wal::open(temp.path(), database, open_mode).expect("open WAL fixture");
        (temp, wal)
    }

    fn commit_state(page_count: u64, catalog_root: u64, freelist_root: u64) -> CommitState {
        CommitState {
            page_count,
            catalog_root: PageId(catalog_root),
            freelist_root: PageId(freelist_root),
            feature_bits: 0,
        }
    }

    fn encoded_transaction(
        lsn: u64,
        previous_lsn: u64,
        pages: &[(PageId, u64)],
        state: CommitState,
    ) -> Vec<u8> {
        encoded_transaction_for_generation(lsn, previous_lsn, pages, state, WAL_GENERATION)
    }

    fn encoded_transaction_for_generation(
        lsn: u64,
        previous_lsn: u64,
        pages: &[(PageId, u64)],
        state: CommitState,
        wal_generation: u64,
    ) -> Vec<u8> {
        let lsn = Lsn(lsn);
        let mut transaction = Vec::new();
        let mut transaction_crc = Crc32c::new();
        for (ordinal, &(page_id, generation)) in pages.iter().enumerate() {
            let ordinal = u32::try_from(ordinal).expect("small test ordinal");
            let payload = page_payload(page_id, generation);
            let header = RecordHeader::page_frame(
                lsn,
                ordinal,
                page_id,
                generation,
                wal_generation,
                &payload,
            )
            .expect("page frame header");
            let encoded_header = header.encode(PAGE_SIZE).expect("encode page header");
            transaction_crc.update(&encoded_header);
            transaction_crc.update(&payload);
            transaction.extend_from_slice(&encoded_header);
            transaction.extend_from_slice(&payload);
            transaction.extend(std::iter::repeat_n(0, zero_padding_len(header.payload_len)));
        }
        let payload = CommitPayload {
            previous_lsn: Lsn(previous_lsn),
            state,
            frame_count: u32::try_from(pages.len()).expect("small test frame count"),
            transaction_crc32c: transaction_crc.finish(),
        }
        .encode()
        .expect("commit payload");
        let header = RecordHeader::commit(
            lsn,
            u32::try_from(pages.len()).expect("small test frame count"),
            wal_generation,
            &payload,
        )
        .expect("commit header");
        transaction.extend_from_slice(&header.encode(PAGE_SIZE).expect("encode commit header"));
        transaction.extend_from_slice(&payload);
        transaction.extend(std::iter::repeat_n(0, zero_padding_len(header.payload_len)));
        transaction
    }

    fn page_payload(page_id: PageId, generation: u64) -> Vec<u8> {
        let reference = PageRef::new(page_id, generation).expect("page reference");
        let mut page =
            PageFrame::new_zeroed(PageType::TableLeaf, reference, PAGE_SIZE).expect("page frame");
        page.prepare_for_write().expect("prepare page frame");
        page.bytes().to_vec()
    }

    fn restamp_record(bytes: &mut [u8], record_start: usize) {
        let payload_len = u32::from_le_bytes(
            bytes[record_start + 12..record_start + 16]
                .try_into()
                .expect("payload length field"),
        );
        let payload_len = usize::try_from(payload_len).expect("test payload length");
        let payload_start = record_start + 64;
        let payload_end = payload_start + payload_len;
        let checksum = crc32c(&bytes[payload_start..payload_end]).to_le_bytes();
        bytes[record_start + 48..record_start + 52].copy_from_slice(&checksum);
        restamp_record_header(bytes, record_start);
    }

    fn restamp_record_header(bytes: &mut [u8], record_start: usize) {
        bytes[record_start + 52..record_start + 56].fill(0);
        let checksum = crc32c(&bytes[record_start..record_start + 64]);
        bytes[record_start + 52..record_start + 56].copy_from_slice(&checksum.to_le_bytes());
    }

    #[derive(Default)]
    struct RecordingFaults(Mutex<Vec<FaultPoint>>);

    impl RecordingFaults {
        fn observed(&self) -> Vec<FaultPoint> {
            self.0.lock().expect("fault record lock").clone()
        }
    }

    impl FaultInjector for RecordingFaults {
        fn check(&self, point: FaultPoint) -> Result<()> {
            self.0.lock().expect("fault record lock").push(point);
            Ok(())
        }
    }
}
