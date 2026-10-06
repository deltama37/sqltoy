//! Row versions, snapshots, and snapshot-isolation visibility.
//!
//! A stored user-table record is a 17-byte version header followed by the
//! ADR-0006 row bytes. `xmin` is the transaction that created the version.
//! `xmax` is the transaction that deleted it, or 0 when it is live. Bit 0 of
//! the flag byte means `xmin` is committed. Bit 1 means `xmax` is committed.
//! Those bits are written at commit, so a transaction that has neither
//! committed nor rolled back is the only kind that leaves a flag unset.
//!
//! A write-write conflict is [`ErrorKind::Other`] with the message
//! `serialization failure: row was modified by a concurrent transaction`.
//! sqltoy does not wait: the first updater wins and the other statement fails.

use std::collections::BTreeSet;
use std::io::{self, ErrorKind};

use crate::record::RecordId;

/// Bytes in front of every versioned row.
pub const VERSION_HEADER_LEN: usize = 17;

/// Flag bit: `xmin` committed.
pub const XMIN_COMMITTED: u8 = 1;

/// Flag bit: `xmax` committed.
pub const XMAX_COMMITTED: u8 = 2;

/// `xmin`, `xmax`, and commit flags for one row version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionHeader {
    /// Transaction that created this version. Never 0 for a real version.
    pub xmin: u64,
    /// Transaction that deleted this version, or 0 if it has not been deleted.
    pub xmax: u64,
    /// Bit 0 is `xmin` committed. Bit 1 is `xmax` committed.
    pub flags: u8,
}

impl VersionHeader {
    /// Header for a version created by `xmin` and not yet deleted.
    pub fn created_by(xmin: u64) -> VersionHeader {
        VersionHeader {
            xmin,
            xmax: 0,
            flags: 0,
        }
    }

    /// Whether bit 0 is set.
    pub fn xmin_committed(self) -> bool {
        self.flags & XMIN_COMMITTED != 0
    }

    /// Whether bit 1 is set.
    pub fn xmax_committed(self) -> bool {
        self.flags & XMAX_COMMITTED != 0
    }

    /// Reads a header. Any 17-byte pattern is a header.
    pub fn decode(bytes: &[u8]) -> Option<VersionHeader> {
        if bytes.len() < VERSION_HEADER_LEN {
            return None;
        }
        Some(VersionHeader {
            xmin: read_u64(&bytes[0..8]),
            xmax: read_u64(&bytes[8..16]),
            flags: bytes[16],
        })
    }

    /// Writes the 17 header bytes.
    pub fn encode(self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.xmin.to_le_bytes());
        out.extend_from_slice(&self.xmax.to_le_bytes());
        out.push(self.flags);
    }
}

/// Splits a stored record into its version header and row bytes.
///
/// A record shorter than the header is [`ErrorKind::InvalidData`]
/// (`invalid row: {id}`).
pub fn split_record(id: RecordId, bytes: &[u8]) -> io::Result<(VersionHeader, &[u8])> {
    let header = VersionHeader::decode(bytes).ok_or_else(|| invalid_row(id))?;
    Ok((header, &bytes[VERSION_HEADER_LEN..]))
}

/// Prepends `header` to `row`.
pub fn encode_record(header: VersionHeader, row: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(VERSION_HEADER_LEN + row.len());
    header.encode(&mut bytes);
    bytes.extend_from_slice(row);
    bytes
}

/// Sets bit 0 on a stored record.
pub fn set_xmin_committed(bytes: &mut [u8]) -> io::Result<()> {
    set_flag(bytes, XMIN_COMMITTED)
}

/// Sets bit 1 on a stored record.
pub fn set_xmax_committed(bytes: &mut [u8]) -> io::Result<()> {
    set_flag(bytes, XMAX_COMMITTED)
}

/// Writes `xmax` into the header without changing flags.
pub fn set_xmax(bytes: &mut [u8], xmax: u64) -> io::Result<()> {
    let slot = bytes.get_mut(8..16).ok_or_else(invalid_header)?;
    slot.copy_from_slice(&xmax.to_le_bytes());
    Ok(())
}

/// Clears `xmax` and bit 1. Used when a deleting transaction rolls back.
pub fn clear_xmax(bytes: &mut [u8]) -> io::Result<()> {
    set_xmax(bytes, 0)?;
    let flags = bytes.get_mut(16).ok_or_else(invalid_header)?;
    *flags &= !XMAX_COMMITTED;
    Ok(())
}

/// What one transaction could see when it started.
///
/// `bound` is the next transaction id at start, after this transaction took
/// its own id. `active` is every other transaction that was running then.
/// The set is not updated when those transactions later commit or roll back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// This transaction's id.
    pub xid: u64,
    /// First id that had not been assigned when the snapshot was taken.
    pub bound: u64,
    /// Other transactions that were in progress at the snapshot.
    pub active: BTreeSet<u64>,
}

/// Whether `xid` looks committed in `snapshot`.
///
/// The version's commit flag must be set, the id must be below the snapshot
/// bound, and the id must not be one of the transactions that were running
/// when the snapshot was taken.
pub fn committed_in(xid: u64, flag: bool, snapshot: &Snapshot) -> bool {
    flag && xid < snapshot.bound && !snapshot.active.contains(&xid)
}

/// Whether transaction `snapshot` can see version `header`.
///
/// The creator must be this transaction or committed in the snapshot, and the
/// deleter must be absent, or neither this transaction nor committed in the
/// snapshot.
pub fn visible(header: &VersionHeader, snapshot: &Snapshot) -> bool {
    let created =
        header.xmin == snapshot.xid || committed_in(header.xmin, header.xmin_committed(), snapshot);
    let deleted = header.xmax != 0
        && (header.xmax == snapshot.xid
            || committed_in(header.xmax, header.xmax_committed(), snapshot));
    created && !deleted
}

/// First-updater-wins conflict. [`ErrorKind::Other`].
pub fn serialization_failure() -> io::Error {
    io::Error::new(
        ErrorKind::Other,
        "serialization failure: row was modified by a concurrent transaction",
    )
}

fn set_flag(bytes: &mut [u8], bit: u8) -> io::Result<()> {
    let flags = bytes.get_mut(16).ok_or_else(invalid_header)?;
    *flags |= bit;
    Ok(())
}

fn invalid_row(id: RecordId) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, format!("invalid row: {id}"))
}

fn invalid_header() -> io::Error {
    io::Error::new(ErrorKind::InvalidData, "invalid row version header")
}

fn read_u64(bytes: &[u8]) -> u64 {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(bytes);
    u64::from_le_bytes(buf)
}

#[cfg(test)]
mod tests {
    use super::{
        clear_xmax, committed_in, encode_record, serialization_failure, set_xmax,
        set_xmax_committed, set_xmin_committed, split_record, visible, Snapshot, VersionHeader,
        VERSION_HEADER_LEN, XMAX_COMMITTED, XMIN_COMMITTED,
    };
    use crate::page::PageId;
    use crate::record::RecordId;
    use std::io::ErrorKind;

    fn snap(xid: u64, bound: u64, active: &[u64]) -> Snapshot {
        Snapshot {
            xid,
            bound,
            active: active.iter().copied().collect(),
        }
    }

    fn header(xmin: u64, xmax: u64, flags: u8) -> VersionHeader {
        VersionHeader { xmin, xmax, flags }
    }

    #[test]
    fn header_round_trip_is_seventeen_little_endian_bytes() {
        let version = VersionHeader {
            xmin: 0x0102_0304_0506_0708,
            xmax: 0x1112_1314_1516_1718,
            flags: XMIN_COMMITTED | XMAX_COMMITTED,
        };
        let record = encode_record(version, b"row");
        assert_eq!(record.len(), VERSION_HEADER_LEN + 3);
        assert_eq!(&record[0..8], &version.xmin.to_le_bytes());
        assert_eq!(&record[8..16], &version.xmax.to_le_bytes());
        assert_eq!(record[16], 0b0000_0011);
        assert_eq!(&record[17..], b"row");
        let id = RecordId {
            page_id: PageId(2),
            slot_id: 1,
        };
        let (decoded, row) = split_record(id, &record).unwrap();
        assert_eq!(decoded, version);
        assert_eq!(row, b"row");
        assert!(version.xmin_committed());
        assert!(version.xmax_committed());
    }

    #[test]
    fn short_record_is_an_invalid_row() {
        let id = RecordId {
            page_id: PageId(2),
            slot_id: 0,
        };
        let err = split_record(id, b"short").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "invalid row: 2:0");
    }

    #[test]
    fn flag_helpers_set_and_clear_xmax() {
        let mut bytes = encode_record(VersionHeader::created_by(4), b"v");
        set_xmax(&mut bytes, 9).unwrap();
        set_xmin_committed(&mut bytes).unwrap();
        set_xmax_committed(&mut bytes).unwrap();
        let (header, _) = split_record(
            RecordId {
                page_id: PageId(1),
                slot_id: 0,
            },
            &bytes,
        )
        .unwrap();
        assert_eq!(header.xmin, 4);
        assert_eq!(header.xmax, 9);
        assert!(header.xmin_committed());
        assert!(header.xmax_committed());
        clear_xmax(&mut bytes).unwrap();
        let (header, row) = split_record(
            RecordId {
                page_id: PageId(1),
                slot_id: 0,
            },
            &bytes,
        )
        .unwrap();
        assert_eq!(header.xmax, 0);
        assert!(header.xmin_committed());
        assert!(!header.xmax_committed());
        assert_eq!(row, b"v");
    }

    #[test]
    fn committed_in_requires_flag_bound_and_absence_from_the_active_set() {
        let snapshot = snap(5, 10, &[3, 7]);
        assert!(committed_in(2, true, &snapshot));
        assert!(committed_in(9, true, &snapshot));
        assert!(!committed_in(2, false, &snapshot));
        assert!(!committed_in(3, true, &snapshot));
        assert!(!committed_in(7, true, &snapshot));
        assert!(!committed_in(10, true, &snapshot));
        assert!(!committed_in(11, true, &snapshot));
        // 0 is not assigned, but the predicate does not special-case it.
        assert!(committed_in(0, true, &snapshot));
        assert!(!committed_in(0, false, &snapshot));
    }

    #[test]
    fn visibility_truth_table() {
        // (xmin, xmax, flags, own xid, bound, active, visible)
        let cases = [
            // Own insert, not deleted.
            (5, 0, 0u8, 5, 6, &[] as &[u64], true),
            // Own insert that this transaction deleted.
            (5, 5, 0, 5, 6, &[], false),
            // Own insert deleted by someone else, delete not visible.
            (5, 8, 0, 5, 6, &[8], true),
            // Own insert deleted by a transaction committed in the snapshot.
            (5, 2, XMAX_COMMITTED, 5, 6, &[], false),
            // Committed insert, still live.
            (2, 0, XMIN_COMMITTED, 5, 6, &[], true),
            // Committed insert, committed delete.
            (2, 3, XMIN_COMMITTED | XMAX_COMMITTED, 5, 6, &[], false),
            // Delete is flagged but the deleter was in progress at the snapshot.
            (2, 3, XMIN_COMMITTED | XMAX_COMMITTED, 5, 6, &[3], true),
            // Delete is flagged but the deleter started after the snapshot.
            (2, 9, XMIN_COMMITTED | XMAX_COMMITTED, 5, 6, &[], true),
            // Creator was in progress. Even a later commit flag is not visible.
            (3, 0, XMIN_COMMITTED, 5, 6, &[3], false),
            // Creator started after the snapshot.
            (9, 0, XMIN_COMMITTED, 5, 6, &[], false),
            // Creator has not committed and is not us.
            (4, 0, 0, 5, 6, &[4], false),
            // Both commit flags, xmin below the bound, xmax is us: hidden.
            (2, 5, XMIN_COMMITTED | XMAX_COMMITTED, 5, 6, &[], false),
            // xmax is 0, so a stray xmax flag does not hide the row.
            (2, 0, XMIN_COMMITTED | XMAX_COMMITTED, 5, 6, &[], true),
            // xmin flag missing, so a committed-looking xmax does not matter.
            (2, 3, XMAX_COMMITTED, 5, 6, &[], false),
            // Aborted deleter (xmax set, flag clear, not us, not active): still visible.
            (2, 4, XMIN_COMMITTED, 5, 6, &[], true),
            // Own xmax hides the row even when the creator is committed.
            (2, 5, XMIN_COMMITTED, 5, 6, &[], false),
            // Bound equal to xmin excludes it. Own xid is a separate path.
            (5, 0, XMIN_COMMITTED, 9, 5, &[], false),
            // Empty active set, xmin just below the bound.
            (4, 0, XMIN_COMMITTED, 9, 5, &[], true),
        ];

        for (index, (xmin, xmax, flags, xid, bound, active, expect)) in cases.iter().enumerate() {
            let snapshot = snap(*xid, *bound, active);
            let version = header(*xmin, *xmax, *flags);
            assert_eq!(
                visible(&version, &snapshot),
                *expect,
                "case {index}: {version:?} snap {snapshot:?}"
            );
        }
    }

    #[test]
    fn own_writes_follow_xmin_and_xmax_equality() {
        let snapshot = snap(7, 8, &[2, 4]);
        let inserted = VersionHeader::created_by(7);
        assert!(visible(&inserted, &snapshot));
        let mut deleted = inserted;
        deleted.xmax = 7;
        assert!(!visible(&deleted, &snapshot));
        // Another transaction's uncommitted row stays invisible.
        assert!(!visible(&VersionHeader::created_by(4), &snapshot));
        // A committed row from before the snapshot is visible.
        assert!(visible(&header(1, 0, XMIN_COMMITTED), &snapshot));
    }

    #[test]
    fn serialization_failure_uses_error_kind_other() {
        let err = serialization_failure();
        assert_eq!(err.kind(), ErrorKind::Other);
        assert_eq!(
            err.to_string(),
            "serialization failure: row was modified by a concurrent transaction"
        );
    }
}
