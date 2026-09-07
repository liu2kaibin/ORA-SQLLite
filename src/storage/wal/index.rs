use std::collections::BTreeMap;

use super::{
    WAL_HEADER_AREA_LEN,
    format::{CommitState, RECORD_HEADER_LEN},
};
use crate::{
    DbError, ErrorCode, Result,
    storage::{Lsn, PageId, header::DatabaseHeader},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WalVersion {
    pub(crate) lsn: Lsn,
    pub(crate) offset: u64,
    pub(crate) generation: u64,
}

#[derive(Debug, Default)]
pub(crate) struct WalIndex {
    pages: BTreeMap<PageId, Vec<WalVersion>>,
    states: BTreeMap<Lsn, CommitState>,
}

impl WalIndex {
    pub(crate) fn publish(
        &mut self,
        lsn: Lsn,
        state: Option<CommitState>,
        frames: &[(PageId, u64, u64)],
    ) -> Result<()> {
        if lsn.0 == 0 {
            return Err(corruption("WAL commit LSN must be nonzero"));
        }
        let state = state.ok_or_else(|| corruption("WAL commit state is missing"))?;
        if self.states.contains_key(&lsn) {
            return Err(corruption("WAL commit LSN is already published"));
        }

        let mut pending: Vec<(PageId, WalVersion)> = Vec::with_capacity(frames.len());
        for &(page_id, offset, generation) in frames {
            if page_id.0 == 0 {
                return Err(corruption("WAL version cannot target page zero"));
            }
            if generation == 0 {
                return Err(corruption("WAL version generation must be nonzero"));
            }
            if offset < WAL_HEADER_AREA_LEN
                || offset.checked_add(RECORD_HEADER_LEN as u64).is_none()
            {
                return Err(corruption("WAL version offset is outside the record area"));
            }
            if pending
                .iter()
                .any(|(pending_page, _)| *pending_page == page_id)
            {
                return Err(corruption("WAL transaction repeats a page version"));
            }
            if self
                .pages
                .get(&page_id)
                .and_then(|versions| versions.last())
                .is_some_and(|version| version.lsn >= lsn)
            {
                return Err(corruption("WAL page versions must have increasing LSNs"));
            }
            pending.push((
                page_id,
                WalVersion {
                    lsn,
                    offset,
                    generation,
                },
            ));
        }

        // Every recoverable validation is complete before either map changes, so a rejected
        // transaction cannot expose a partial frame set or a state without its frames.
        self.states.insert(lsn, state);
        for (page_id, version) in pending {
            self.pages.entry(page_id).or_default().push(version);
        }
        Ok(())
    }

    pub(crate) fn visible_version(&self, page_id: PageId, snapshot: Lsn) -> Option<WalVersion> {
        self.pages
            .get(&page_id)
            .and_then(|versions| {
                versions
                    .iter()
                    .rev()
                    .find(|version| version.lsn <= snapshot)
            })
            .copied()
    }

    pub(crate) fn state_at(
        &self,
        checkpoint: &DatabaseHeader,
        snapshot: Lsn,
    ) -> Result<CommitState> {
        if snapshot == checkpoint.checkpoint_lsn() {
            return Ok(CommitState {
                page_count: checkpoint.page_count(),
                catalog_root: checkpoint.catalog_root(),
                freelist_root: checkpoint.freelist_root(),
                feature_bits: checkpoint.features(),
            });
        }
        if snapshot < checkpoint.checkpoint_lsn() {
            return Err(corruption("snapshot predates the database checkpoint"));
        }
        self.states
            .get(&snapshot)
            .copied()
            .ok_or_else(|| corruption("WAL commit state is missing for the snapshot"))
    }

    pub(crate) fn discard_through(&mut self, checkpoint: Lsn) {
        self.pages.retain(|_, versions| {
            versions.retain(|version| version.lsn > checkpoint);
            !versions.is_empty()
        });
        self.states.retain(|lsn, _| *lsn > checkpoint);
    }

    pub(crate) fn checkpoint_versions(&self, from: Lsn, target: Lsn) -> Vec<(PageId, WalVersion)> {
        self.pages
            .iter()
            .filter_map(|(&page_id, versions)| {
                versions
                    .iter()
                    .rev()
                    .find(|version| version.lsn <= target)
                    .filter(|version| version.lsn > from)
                    .map(|version| (page_id, *version))
            })
            .collect()
    }
}

fn corruption(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Corruption, message)
}

#[cfg(test)]
mod tests {
    use super::{WalIndex, WalVersion};
    use crate::{
        ErrorCode,
        storage::{Lsn, PageId, header::DatabaseHeader, wal::format::CommitState},
    };

    #[test]
    fn visible_version_selects_the_newest_committed_version_at_or_before_the_snapshot() {
        let mut index = WalIndex::default();
        let state = CommitState {
            page_count: 8,
            catalog_root: PageId(2),
            freelist_root: PageId(3),
            feature_bits: 0,
        };
        for (lsn, offset, generation) in [(2, 8_320, 11), (4, 16_640, 12), (7, 24_960, 13)] {
            index
                .publish(Lsn(lsn), Some(state), &[(PageId(5), offset, generation)])
                .expect("publish a committed page version");
        }

        assert_eq!(index.visible_version(PageId(5), Lsn(1)), None);
        assert_eq!(
            index.visible_version(PageId(5), Lsn(2)),
            Some(WalVersion {
                lsn: Lsn(2),
                offset: 8_320,
                generation: 11
            })
        );
        assert_eq!(
            index.visible_version(PageId(5), Lsn(6)),
            Some(WalVersion {
                lsn: Lsn(4),
                offset: 16_640,
                generation: 12
            })
        );
        assert_eq!(
            index.visible_version(PageId(5), Lsn(7)),
            Some(WalVersion {
                lsn: Lsn(7),
                offset: 24_960,
                generation: 13
            })
        );
    }

    #[test]
    fn state_at_is_exact_and_discard_through_keeps_only_post_checkpoint_versions() {
        let checkpoint = DatabaseHeader::new(4096, [0x11; 16])
            .expect("valid header")
            .with_checkpoint(Lsn(4), 5, PageId(1), PageId(0), 1)
            .expect("valid checkpoint");
        let mut index = WalIndex::default();
        let states = [
            (
                Lsn(2),
                CommitState {
                    page_count: 3,
                    catalog_root: PageId(1),
                    freelist_root: PageId(0),
                    feature_bits: 0,
                },
            ),
            (
                Lsn(4),
                CommitState {
                    page_count: 5,
                    catalog_root: PageId(1),
                    freelist_root: PageId(0),
                    feature_bits: 0,
                },
            ),
            (
                Lsn(7),
                CommitState {
                    page_count: 9,
                    catalog_root: PageId(7),
                    freelist_root: PageId(8),
                    feature_bits: 0,
                },
            ),
        ];
        for (lsn, state) in states {
            index
                .publish(lsn, Some(state), &[(PageId(5), lsn.0 * 4_160, lsn.0 + 20)])
                .expect("publish version");
        }

        assert_eq!(
            index
                .state_at(&checkpoint, Lsn(7))
                .expect("state at commit"),
            CommitState {
                page_count: 9,
                catalog_root: PageId(7),
                freelist_root: PageId(8),
                feature_bits: 0
            }
        );
        index.discard_through(Lsn(4));

        assert_eq!(index.visible_version(PageId(5), Lsn(6)), None);
        assert_eq!(
            index.visible_version(PageId(5), Lsn(7)),
            Some(WalVersion {
                lsn: Lsn(7),
                offset: 29_120,
                generation: 27
            })
        );
        assert_eq!(
            index
                .state_at(&checkpoint, Lsn(4))
                .expect("checkpoint state"),
            CommitState {
                page_count: 5,
                catalog_root: PageId(1),
                freelist_root: PageId(0),
                feature_bits: 0
            }
        );
    }

    #[test]
    fn publish_rejects_a_missing_state_before_changing_any_index_entries() {
        let mut index = WalIndex::default();
        let state = CommitState {
            page_count: 2,
            catalog_root: PageId(1),
            freelist_root: PageId(0),
            feature_bits: 0,
        };
        index
            .publish(Lsn(2), Some(state), &[(PageId(1), 8_192, 3)])
            .expect("initial publication");

        assert_eq!(
            index
                .publish(Lsn(3), None, &[(PageId(1), 12_352, 4)])
                .expect_err("a commit must include its state")
                .code(),
            ErrorCode::Corruption
        );
        assert_eq!(
            index.visible_version(PageId(1), Lsn(3)),
            Some(WalVersion {
                lsn: Lsn(2),
                offset: 8_192,
                generation: 3
            })
        );
        assert_eq!(
            index
                .state_at(&DatabaseHeader::new(4096, [0x12; 16]).unwrap(), Lsn(3))
                .expect_err("failed publish must not add a state")
                .code(),
            ErrorCode::Corruption
        );
    }
}
