//! In-memory slotted page.
//!
//! Record bytes are packed from the end of the page. A slot directory at the
//! front points at those bytes, so compaction can move a record without
//! changing its slot id. This module does not read or write the database file.

use std::io::{self, ErrorKind};

use crate::page::{Page, PageId, PAGE_SIZE};

/// Byte at the start of a record page.
const PAGE_TYPE_RECORD: u8 = 1;

const HEADER_LEN: usize = 8;
const SLOT_LEN: usize = 4;

const PAGE_TYPE_OFFSET: usize = 0;
const SLOT_COUNT_OFFSET: usize = 2;
const FREE_END_OFFSET: usize = 4;

/// Maximum record length in bytes (`4096 - 8 - 4`).
pub const MAX_RECORD_SIZE: usize = PAGE_SIZE - HEADER_LEN - SLOT_LEN;

/// One record page kept entirely in memory.
pub struct SlottedPage {
    page: Page,
}

impl SlottedPage {
    /// An empty record page (`slot_count = 0`, `free_end = 4096`).
    pub fn init() -> SlottedPage {
        let mut page = Page::zeroed();
        page.data_mut()[PAGE_TYPE_OFFSET] = PAGE_TYPE_RECORD;
        write_u16_le(
            &mut page.data_mut()[FREE_END_OFFSET..FREE_END_OFFSET + 2],
            PAGE_SIZE as u16,
        );
        SlottedPage { page }
    }

    /// Interprets `page` as a record page.
    ///
    /// Fails with [`ErrorKind::InvalidData`] when the page type byte is not a
    /// record page, or when the slot directory and record offsets disagree.
    pub fn from_page(page_id: PageId, page: Page) -> io::Result<SlottedPage> {
        if page.data().first().copied() != Some(PAGE_TYPE_RECORD) {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                format!("not a record page: {page_id}"),
            ));
        }
        let slotted = SlottedPage { page };
        slotted.check_layout(page_id)?;
        Ok(slotted)
    }

    /// The underlying page bytes.
    pub fn page(&self) -> &Page {
        &self.page
    }

    /// Unwraps the underlying page.
    pub fn into_page(self) -> Page {
        self.page
    }

    /// Number of slots, including tombstones.
    pub fn slot_count(&self) -> u16 {
        read_u16_le(&self.page.data()[SLOT_COUNT_OFFSET..SLOT_COUNT_OFFSET + 2])
    }

    /// Bytes of a live record.
    ///
    /// Returns `None` when `slot_id` is out of range or the slot is a tombstone.
    pub fn get(&self, slot_id: u16) -> Option<&[u8]> {
        let (offset, len) = self.live_slot(slot_id)?;
        let start = offset as usize;
        let end = start + len as usize;
        Some(&self.page.data()[start..end])
    }

    /// Inserts `record` and returns the slot id that now points at it.
    ///
    /// Reuses the lowest-numbered tombstone when one exists. Otherwise appends
    /// a slot. Returns `None` when the page cannot hold `record`, including
    /// when it is longer than [`MAX_RECORD_SIZE`]. The page is unchanged then.
    pub fn insert(&mut self, record: &[u8]) -> Option<u16> {
        if record.len() > MAX_RECORD_SIZE {
            return None;
        }
        let reused = self.lowest_tombstone();
        let slot_bytes = if reused.is_some() { 0 } else { SLOT_LEN };
        if self.free_space() < record.len() + slot_bytes {
            return None;
        }
        // Deletes and in-place shrinks leave holes. The total still counts
        // those holes; compaction turns them back into one gap.
        if self.contiguous_free() < record.len() + slot_bytes {
            self.compact();
        }
        let slot_id = if let Some(slot_id) = reused {
            slot_id
        } else {
            let slot_id = self.slot_count();
            self.set_slot_count(slot_id + 1);
            slot_id
        };
        self.write_record_at_free_end(slot_id, record);
        Some(slot_id)
    }

    /// Replaces the live record at `slot_id`.
    ///
    /// A new value that is no longer than the old one is written in place and
    /// the length is shrunk. A longer value is moved into free space on this
    /// page, compacting first when the free bytes are fragmented. The slot id
    /// does not change.
    ///
    /// Returns `Ok(false)` when the new bytes do not fit on this page. The
    /// page is left unchanged in that case. A missing or deleted slot is
    /// [`ErrorKind::NotFound`]. A record longer than [`MAX_RECORD_SIZE`] is
    /// [`ErrorKind::InvalidInput`].
    pub fn update(&mut self, slot_id: u16, record: &[u8]) -> io::Result<bool> {
        if record.len() > MAX_RECORD_SIZE {
            return Err(record_too_large(record.len()));
        }
        let Some((offset, old_len)) = self.live_slot(slot_id) else {
            return Err(slot_not_found(slot_id));
        };
        if record.len() <= old_len as usize {
            let start = offset as usize;
            self.page.data_mut()[start..start + record.len()].copy_from_slice(record);
            self.write_slot(slot_id, offset, record.len() as u16);
            return Ok(true);
        }
        // The bytes the slot currently owns become free once it stops pointing
        // at them, so they count toward the space available for the new value.
        let available = self.free_space() + old_len as usize;
        if available < record.len() {
            return Ok(false);
        }
        if self.contiguous_free() < record.len() {
            self.write_slot(slot_id, 0, 0);
            self.compact();
        }
        self.write_record_at_free_end(slot_id, record);
        Ok(true)
    }

    /// Marks `slot_id` as a tombstone.
    ///
    /// The slot directory is not shrunk, and the record bytes stay until the
    /// next compaction. A missing or deleted slot is [`ErrorKind::NotFound`].
    pub fn delete(&mut self, slot_id: u16) -> io::Result<()> {
        if self.live_slot(slot_id).is_none() {
            return Err(slot_not_found(slot_id));
        }
        self.write_slot(slot_id, 0, 0);
        Ok(())
    }

    /// Packs live records against the end of the page.
    ///
    /// Slot ids and tombstones stay as they are. Offsets and `free_end` are
    /// rewritten so the free space is one contiguous gap.
    pub fn compact(&mut self) {
        let count = self.slot_count();
        let mut live = Vec::with_capacity(count as usize);
        for slot_id in 0..count {
            if let Some(bytes) = self.get(slot_id) {
                live.push((slot_id, bytes.to_vec()));
            }
        }
        let mut cursor = PAGE_SIZE;
        for (slot_id, bytes) in live {
            cursor -= bytes.len();
            if !bytes.is_empty() {
                self.page.data_mut()[cursor..cursor + bytes.len()].copy_from_slice(&bytes);
            }
            // A zero-length record has no bytes. `cursor` is still past the
            // header, so the offset stays distinct from a tombstone.
            let offset = u16::try_from(cursor).expect("cursor fits in u16");
            let len = u16::try_from(bytes.len()).expect("record length fits in u16");
            self.write_slot(slot_id, offset, len);
        }
        let free_end = u16::try_from(cursor).expect("cursor fits in u16");
        self.set_free_end(free_end);
    }

    /// Free bytes, counting fragmented holes from deletes and shrinks.
    pub fn free_space(&self) -> usize {
        let mut used = HEADER_LEN + self.slot_count() as usize * SLOT_LEN;
        let count = self.slot_count();
        for slot_id in 0..count {
            if let Some((_, len)) = self.live_slot(slot_id) {
                used += len as usize;
            }
        }
        PAGE_SIZE - used
    }

    /// Bytes in the single gap between the slot directory and the records.
    pub fn contiguous_free(&self) -> usize {
        self.free_end() as usize - self.free_start()
    }

    /// Live records in slot order.
    pub fn iter_live(&self) -> impl Iterator<Item = (u16, &[u8])> {
        LiveRecords {
            page: self,
            next: 0,
        }
    }

    fn check_layout(&self, page_id: PageId) -> io::Result<()> {
        let count = self.slot_count();
        let start = HEADER_LEN + count as usize * SLOT_LEN;
        if start > PAGE_SIZE {
            return Err(invalid_page(page_id));
        }
        let free_end = self.free_end() as usize;
        if free_end < start || free_end > PAGE_SIZE {
            return Err(invalid_page(page_id));
        }
        for slot_id in 0..count {
            let (offset, len) = self.read_slot(slot_id);
            if offset == 0 {
                continue;
            }
            let offset = offset as usize;
            let end = offset + len as usize;
            if offset < free_end || end > PAGE_SIZE {
                return Err(invalid_page(page_id));
            }
        }
        Ok(())
    }

    fn free_start(&self) -> usize {
        HEADER_LEN + self.slot_count() as usize * SLOT_LEN
    }

    fn free_end(&self) -> u16 {
        read_u16_le(&self.page.data()[FREE_END_OFFSET..FREE_END_OFFSET + 2])
    }

    fn set_slot_count(&mut self, count: u16) {
        write_u16_le(
            &mut self.page.data_mut()[SLOT_COUNT_OFFSET..SLOT_COUNT_OFFSET + 2],
            count,
        );
    }

    fn set_free_end(&mut self, free_end: u16) {
        write_u16_le(
            &mut self.page.data_mut()[FREE_END_OFFSET..FREE_END_OFFSET + 2],
            free_end,
        );
    }

    fn lowest_tombstone(&self) -> Option<u16> {
        let count = self.slot_count();
        (0..count).find(|&slot_id| self.read_slot(slot_id).0 == 0)
    }

    fn live_slot(&self, slot_id: u16) -> Option<(u16, u16)> {
        if slot_id >= self.slot_count() {
            return None;
        }
        let (offset, len) = self.read_slot(slot_id);
        if offset == 0 {
            None
        } else {
            Some((offset, len))
        }
    }

    fn read_slot(&self, slot_id: u16) -> (u16, u16) {
        let start = HEADER_LEN + slot_id as usize * SLOT_LEN;
        let bytes = &self.page.data()[start..start + SLOT_LEN];
        let offset = read_u16_le(&bytes[..2]);
        let len = read_u16_le(&bytes[2..]);
        (offset, len)
    }

    fn write_slot(&mut self, slot_id: u16, offset: u16, len: u16) {
        let start = HEADER_LEN + slot_id as usize * SLOT_LEN;
        write_u16_le(&mut self.page.data_mut()[start..start + 2], offset);
        write_u16_le(&mut self.page.data_mut()[start + 2..start + SLOT_LEN], len);
    }

    fn write_record_at_free_end(&mut self, slot_id: u16, record: &[u8]) {
        let len = u16::try_from(record.len()).expect("record length fits in u16");
        let offset = self
            .free_end()
            .checked_sub(len)
            .expect("record fits below free_end");
        // Offset 0 is the header and means tombstone. `free_end` stays at or
        // above the slot directory, so a zero-length record can use it as-is.
        debug_assert_ne!(offset, 0);
        let start = offset as usize;
        self.page.data_mut()[start..start + record.len()].copy_from_slice(record);
        self.set_free_end(offset);
        self.write_slot(slot_id, offset, len);
    }
}

struct LiveRecords<'a> {
    page: &'a SlottedPage,
    next: u16,
}

impl<'a> Iterator for LiveRecords<'a> {
    type Item = (u16, &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        while self.next < self.page.slot_count() {
            let slot_id = self.next;
            self.next += 1;
            if let Some(bytes) = self.page.get(slot_id) {
                return Some((slot_id, bytes));
            }
        }
        None
    }
}

fn record_too_large(len: usize) -> io::Error {
    io::Error::new(
        ErrorKind::InvalidInput,
        format!("record too large: {len} bytes (max {MAX_RECORD_SIZE})"),
    )
}

fn slot_not_found(slot_id: u16) -> io::Error {
    io::Error::new(
        ErrorKind::NotFound,
        format!("record not found: slot {slot_id}"),
    )
}

fn invalid_page(page_id: PageId) -> io::Error {
    io::Error::new(
        ErrorKind::InvalidData,
        format!("invalid record page: {page_id}"),
    )
}

fn read_u16_le(bytes: &[u8]) -> u16 {
    let mut buf = [0u8; 2];
    buf.copy_from_slice(bytes);
    u16::from_le_bytes(buf)
}

fn write_u16_le(dest: &mut [u8], value: u16) {
    dest.copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::{SlottedPage, MAX_RECORD_SIZE};
    use crate::page::{Page, PageId, PAGE_SIZE};
    use std::io::ErrorKind;

    fn reload(page: SlottedPage) -> SlottedPage {
        SlottedPage::from_page(PageId(1), page.into_page()).expect("page stays valid")
    }

    #[test]
    fn init_layout_is_header_only() {
        let page = SlottedPage::init();
        let bytes = page.page().data();
        assert_eq!(bytes[0], 1);
        assert_eq!(bytes[1], 0);
        assert_eq!(&bytes[2..4], &0u16.to_le_bytes());
        assert_eq!(&bytes[4..6], &4096u16.to_le_bytes());
        assert_eq!(&bytes[6..8], &0u16.to_le_bytes());
        assert!(bytes[8..].iter().all(|byte| *byte == 0));
        assert_eq!(page.slot_count(), 0);
        assert_eq!(page.free_space(), PAGE_SIZE - 8);
        assert_eq!(page.contiguous_free(), PAGE_SIZE - 8);
    }

    #[test]
    fn insert_and_get_roundtrip_layout() {
        let mut page = SlottedPage::init();
        let slot = page.insert(b"ab").unwrap();
        assert_eq!(slot, 0);
        assert_eq!(page.get(0).unwrap(), b"ab");

        let bytes = page.page().data();
        assert_eq!(&bytes[2..4], &1u16.to_le_bytes());
        assert_eq!(&bytes[4..6], &4094u16.to_le_bytes());
        assert_eq!(&bytes[8..10], &4094u16.to_le_bytes());
        assert_eq!(&bytes[10..12], &2u16.to_le_bytes());
        assert_eq!(&bytes[4094..4096], b"ab");

        let page = reload(page);
        assert_eq!(page.get(0).unwrap(), b"ab");
    }

    #[test]
    fn many_inserts_fill_the_page() {
        let mut page = SlottedPage::init();
        let mut count = 0u16;
        while let Some(slot) = page.insert(&[0x5A]) {
            assert_eq!(slot, count);
            count += 1;
        }
        // Each 1-byte record costs 5 bytes (slot + payload) out of 4088.
        assert_eq!(count, 817);
        assert_eq!(page.free_space(), 3);
        assert!(page.insert(&[0x5A]).is_none());
        assert_eq!(page.slot_count(), count);
        for slot in 0..count {
            assert_eq!(page.get(slot).unwrap(), &[0x5A]);
        }
        let page = reload(page);
        assert_eq!(page.slot_count(), count);
    }

    #[test]
    fn tombstone_reuse_picks_the_lowest_slot() {
        let mut page = SlottedPage::init();
        assert_eq!(page.insert(b"a").unwrap(), 0);
        assert_eq!(page.insert(b"b").unwrap(), 1);
        assert_eq!(page.insert(b"c").unwrap(), 2);
        page.delete(2).unwrap();
        page.delete(0).unwrap();
        assert_eq!(page.slot_count(), 3);
        assert_eq!(&page.page().data()[8..12], &[0, 0, 0, 0]);

        let reused = page.insert(b"d").unwrap();
        assert_eq!(reused, 0);
        assert_eq!(page.get(0).unwrap(), b"d");
        assert_eq!(page.get(1).unwrap(), b"b");
        assert!(page.get(2).is_none());

        let next = page.insert(b"e").unwrap();
        assert_eq!(next, 2);
        assert_eq!(page.get(2).unwrap(), b"e");
        assert_eq!(page.slot_count(), 3);
    }

    #[test]
    fn compaction_preserves_slot_ids_and_data() {
        let mut page = SlottedPage::init();
        page.insert(b"alpha").unwrap();
        page.insert(b"beta").unwrap();
        page.insert(b"gamma").unwrap();
        page.delete(1).unwrap();
        assert!(page.free_space() > page.contiguous_free());

        page.compact();
        assert_eq!(page.slot_count(), 3);
        assert_eq!(page.get(0).unwrap(), b"alpha");
        assert!(page.get(1).is_none());
        assert_eq!(page.read_slot(1), (0, 0));
        assert_eq!(page.get(2).unwrap(), b"gamma");
        assert_eq!(page.free_space(), page.contiguous_free());

        let live: Vec<_> = page
            .iter_live()
            .map(|(slot, bytes)| (slot, bytes.to_vec()))
            .collect();
        assert_eq!(live, vec![(0, b"alpha".to_vec()), (2, b"gamma".to_vec())]);
        let page = reload(page);
        assert_eq!(page.get(0).unwrap(), b"alpha");
        assert_eq!(page.get(2).unwrap(), b"gamma");
    }

    #[test]
    fn insert_succeeds_only_after_compaction() {
        let mut page = SlottedPage::init();
        page.insert(&[1u8; 1000]).unwrap();
        page.insert(&[2u8; 1000]).unwrap();
        page.insert(&[3u8; 1000]).unwrap();
        page.update(0, &[1u8; 100]).unwrap();
        page.update(1, &[2u8; 100]).unwrap();
        page.update(2, &[3u8; 100]).unwrap();

        let record = vec![9u8; 2000];
        let needed = record.len() + 4;
        assert!(page.contiguous_free() < needed);
        assert!(page.free_space() >= needed);
        assert!(page.lowest_tombstone().is_none());

        let slot = page.insert(&record).unwrap();
        assert_eq!(slot, 3);
        assert_eq!(page.get(0).unwrap(), &[1u8; 100]);
        assert_eq!(page.get(1).unwrap(), &[2u8; 100]);
        assert_eq!(page.get(2).unwrap(), &[3u8; 100]);
        assert_eq!(page.get(3).unwrap(), record.as_slice());
        assert_eq!(page.free_space(), page.contiguous_free());
        let page = reload(page);
        assert_eq!(page.get(3).unwrap(), record.as_slice());
    }

    #[test]
    fn update_shrink_grow_and_reject() {
        let mut page = SlottedPage::init();
        page.insert(b"hello").unwrap();
        let (offset, len) = page.read_slot(0);
        assert_eq!((offset, len), (4091, 5));

        assert!(page.update(0, b"hi").unwrap());
        assert_eq!(page.get(0).unwrap(), b"hi");
        assert_eq!(page.read_slot(0), (4091, 2));
        assert!(page.update(0, b"yo").unwrap());
        assert_eq!(page.get(0).unwrap(), b"yo");
        assert_eq!(page.read_slot(0), (4091, 2));

        assert!(page.update(0, b"hello!").unwrap());
        assert_eq!(page.get(0).unwrap(), b"hello!");
        assert_ne!(page.read_slot(0).0, 4091);

        page.insert(&[1u8; 1000]).unwrap();
        page.insert(&[2u8; 1000]).unwrap();
        page.insert(&[3u8; 1000]).unwrap();
        page.delete(2).unwrap();
        assert!(page.contiguous_free() < 2500);
        assert!(page.update(1, &[9u8; 2500]).unwrap());
        assert_eq!(page.get(0).unwrap(), b"hello!");
        assert_eq!(page.get(1).unwrap(), &[9u8; 2500]);
        assert!(page.get(2).is_none());
        assert_eq!(page.get(3).unwrap(), &[3u8; 1000]);

        let mut packed = SlottedPage::init();
        packed.insert(&[1u8; 2000]).unwrap();
        packed.insert(&[2u8; 2000]).unwrap();
        let before = packed.page().data().to_vec();
        assert!(!packed.update(0, &[3u8; 2081]).unwrap());
        assert_eq!(packed.page().data(), before.as_slice());
        assert_eq!(packed.get(0).unwrap(), &[1u8; 2000]);
        assert_eq!(packed.get(1).unwrap(), &[2u8; 2000]);

        assert!(packed.update(0, &[3u8; 2080]).unwrap());
        assert_eq!(packed.get(0).unwrap(), &[3u8; 2080]);
        assert_eq!(packed.get(1).unwrap(), &[2u8; 2000]);
        let packed = reload(packed);
        assert_eq!(packed.get(0).unwrap(), &[3u8; 2080]);
    }

    #[test]
    fn zero_length_record_survives_compaction() {
        let mut page = SlottedPage::init();
        assert_eq!(page.insert(b"").unwrap(), 0);
        assert_eq!(page.get(0).unwrap(), b"");
        let (offset, len) = page.read_slot(0);
        assert_eq!((offset, len), (4096, 0));

        page.insert(b"x").unwrap();
        page.insert(b"").unwrap();
        page.delete(1).unwrap();
        page.compact();

        assert_eq!(page.get(0).unwrap(), b"");
        assert!(page.get(1).is_none());
        assert_eq!(page.get(2).unwrap(), b"");
        assert_ne!(page.read_slot(0).0, 0);
        assert_ne!(page.read_slot(2).0, 0);
        assert_eq!(page.read_slot(0).1, 0);
        assert_eq!(page.read_slot(2).1, 0);

        page.delete(0).unwrap();
        assert!(page.get(0).is_none());
        assert_eq!(page.insert(b"z").unwrap(), 0);
        assert_eq!(page.get(0).unwrap(), b"z");
        let page = reload(page);
        assert_eq!(page.get(0).unwrap(), b"z");
        assert_eq!(page.get(2).unwrap(), b"");
    }

    #[test]
    fn max_record_fills_an_empty_page() {
        let mut page = SlottedPage::init();
        let record = vec![0xABu8; MAX_RECORD_SIZE];
        assert_eq!(MAX_RECORD_SIZE, 4084);
        let slot = page.insert(&record).unwrap();
        assert_eq!(slot, 0);
        assert_eq!(page.get(0).unwrap(), record.as_slice());
        assert_eq!(page.free_space(), 0);
        assert_eq!(page.contiguous_free(), 0);
        assert_eq!(page.read_slot(0), (12, 4084));
        assert_eq!(&page.page().data()[12..], record.as_slice());
        assert!(page.insert(&[1]).is_none());
        let page = reload(page);
        assert_eq!(page.get(0).unwrap(), record.as_slice());
    }

    #[test]
    fn oversized_record_is_rejected() {
        let mut page = SlottedPage::init();
        let before = page.page().data().to_vec();
        assert!(page.insert(&[0u8; MAX_RECORD_SIZE + 1]).is_none());
        assert_eq!(page.page().data(), before.as_slice());

        page.insert(b"ok").unwrap();
        let before = page.page().data().to_vec();
        let err = page.update(0, &[0u8; MAX_RECORD_SIZE + 1]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            err.to_string(),
            format!(
                "record too large: {} bytes (max {MAX_RECORD_SIZE})",
                MAX_RECORD_SIZE + 1
            )
        );
        assert_eq!(page.page().data(), before.as_slice());
    }

    #[test]
    fn missing_slot_is_not_found() {
        let mut page = SlottedPage::init();
        page.insert(b"a").unwrap();
        page.delete(0).unwrap();

        let err = page.update(0, b"b").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert_eq!(err.to_string(), "record not found: slot 0");

        let err = page.delete(0).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert_eq!(err.to_string(), "record not found: slot 0");

        let err = page.delete(3).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert!(page.get(3).is_none());
    }

    #[test]
    fn non_record_page_is_rejected() {
        let err = error_of(SlottedPage::from_page(PageId(3), Page::zeroed()));
        assert_eq!(err.kind(), ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "not a record page: 3");

        let mut page = Page::zeroed();
        page.data_mut()[0] = 2;
        let err = error_of(SlottedPage::from_page(PageId(4), page));
        assert_eq!(err.kind(), ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "not a record page: 4");

        let mut page = Page::zeroed();
        page.data_mut()[0] = 1;
        page.data_mut()[2..4].copy_from_slice(&5000u16.to_le_bytes());
        let err = error_of(SlottedPage::from_page(PageId(5), page));
        assert_eq!(err.kind(), ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "invalid record page: 5");
    }

    #[test]
    fn random_operations_match_a_model() {
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move |bound: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % bound as u64) as usize
        };

        let mut page = SlottedPage::init();
        let mut model: Vec<Option<Vec<u8>>> = Vec::new();
        for step in 0..20_000 {
            let len = next(600);
            let record = vec![(step % 251) as u8; len];
            match next(3) {
                0 => {
                    let model_fits = {
                        let reuse = model.iter().any(Option::is_none);
                        page.free_space() >= len + if reuse { 0 } else { 4 }
                    };
                    match page.insert(&record) {
                        Some(slot) => {
                            assert!(model_fits);
                            let slot = slot as usize;
                            if slot == model.len() {
                                model.push(Some(record));
                            } else {
                                assert!(model[slot].is_none());
                                assert!(model[..slot].iter().all(Option::is_some));
                                model[slot] = Some(record);
                            }
                        }
                        None => assert!(!model_fits),
                    }
                }
                1 if !model.is_empty() => {
                    let slot = next(model.len());
                    let free_before = page.free_space();
                    let result = page.update(slot as u16, &record);
                    match &model[slot] {
                        None => assert_eq!(result.unwrap_err().kind(), ErrorKind::NotFound),
                        Some(old) => {
                            let fits = free_before + old.len() >= len;
                            assert_eq!(result.unwrap(), fits);
                            if fits {
                                model[slot] = Some(record);
                            }
                        }
                    }
                }
                _ if !model.is_empty() => {
                    let slot = next(model.len());
                    let result = page.delete(slot as u16);
                    assert_eq!(result.is_ok(), model[slot].is_some());
                    model[slot] = None;
                }
                _ => {}
            }

            page = reload(page);
            assert_eq!(page.slot_count() as usize, model.len());
            for (slot, expected) in model.iter().enumerate() {
                assert_eq!(page.get(slot as u16), expected.as_deref());
            }
        }
    }

    fn error_of<T>(result: std::io::Result<T>) -> std::io::Error {
        match result {
            Ok(_) => panic!("expected an error"),
            Err(err) => err,
        }
    }
}
