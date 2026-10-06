//! Primary-key B+Tree index.
//!
//! Each node is one page. The root page id never changes: splitting the root
//! allocates two pages, moves the old contents there, and rewrites the root
//! as an internal node with one separator. Deletes remove a leaf entry and
//! do not merge or redistribute. Node writes stay in the buffer pool until
//! the caller flushes. Insert and delete do not sync the file.

use std::io::{self, ErrorKind};

use crate::buffer::BufferPool;
use crate::page::{Page, PageId, PAGE_SIZE};
use crate::record::{RecordId, TableId};

/// Page type byte of a B+Tree node.
pub(crate) const PAGE_TYPE_BTREE: u8 = 2;

const NODE_LEAF: u8 = 1;
const NODE_INTERNAL: u8 = 2;

const HEADER_LEN: usize = 16;
const LEAF_ENTRY_LEN: usize = 16;
const INTERNAL_ENTRY_LEN: usize = 12;

/// Maximum keys in a leaf: `(4096 - 16) / 16`.
pub const LEAF_CAPACITY: usize = (PAGE_SIZE - HEADER_LEN) / LEAF_ENTRY_LEN;

/// Maximum separators in an internal node: `(4096 - 16) / 12`.
pub const INTERNAL_CAPACITY: usize = (PAGE_SIZE - HEADER_LEN) / INTERNAL_ENTRY_LEN;

/// A primary-key index rooted at a fixed page.
pub struct BTree {
    root: PageId,
    owner: TableId,
    leaf_capacity: usize,
    internal_capacity: usize,
}

#[derive(Clone)]
struct LeafEntry {
    key: i64,
    rid: RecordId,
}

#[derive(Clone)]
struct InternalEntry {
    key: i64,
    child: PageId,
}

enum Node {
    Leaf {
        next: PageId,
        entries: Vec<LeafEntry>,
    },
    Internal {
        leftmost: PageId,
        entries: Vec<InternalEntry>,
    },
}

struct Split {
    key: i64,
    right: PageId,
}

impl BTree {
    /// Allocates an empty leaf and returns its page id.
    ///
    /// That id stays the root for the life of the index. The new page is dirty
    /// until the caller flushes the buffer pool.
    pub fn create(pages: &mut BufferPool, owner: TableId) -> io::Result<PageId> {
        let id = pages.allocate_page()?;
        write_leaf(pages, id, owner, PageId(0), &[])?;
        Ok(id)
    }

    /// Opens the index rooted at `root` and owned by `owner`.
    ///
    /// The page is not read until an operation needs it.
    pub fn open(root: PageId, owner: TableId) -> BTree {
        BTree {
            root,
            owner,
            leaf_capacity: LEAF_CAPACITY,
            internal_capacity: INTERNAL_CAPACITY,
        }
    }

    /// Opens `root` with smaller capacities so tests can split cheaply.
    ///
    /// Capacities must be at least 1 and no larger than the on-disk maximum.
    #[cfg(test)]
    pub(crate) fn with_capacities(
        root: PageId,
        owner: TableId,
        leaf_capacity: usize,
        internal_capacity: usize,
    ) -> BTree {
        assert!((1..=LEAF_CAPACITY).contains(&leaf_capacity));
        assert!((1..=INTERNAL_CAPACITY).contains(&internal_capacity));
        BTree {
            root,
            owner,
            leaf_capacity,
            internal_capacity,
        }
    }

    /// Page id of the root. It does not change when the root splits.
    pub fn root(&self) -> PageId {
        self.root
    }

    /// Looks up `key`.
    ///
    /// `Ok(None)` when the key is absent. A page that is not a node of this
    /// index is [`ErrorKind::InvalidData`] (`invalid index page: {page_id}`).
    pub fn get(&self, pages: &mut BufferPool, key: i64) -> io::Result<Option<RecordId>> {
        let mut page_id = self.root;
        let limit = pages.page_count()?;
        for _ in 0..limit {
            match read_node(pages, page_id, self.owner)? {
                Node::Leaf { entries, .. } => {
                    return Ok(find_leaf(&entries, key).map(|index| entries[index].rid));
                }
                Node::Internal { leftmost, entries } => {
                    page_id = child_for(leftmost, &entries, key);
                }
            }
        }
        Err(invalid_index_page(page_id))
    }

    /// Inserts `key` pointing at `rid`.
    ///
    /// A duplicate key is [`ErrorKind::InvalidInput`]
    /// (`duplicate primary key: {key}`) and the tree is not modified.
    /// The new entry stays dirty until the caller flushes.
    pub fn insert(&self, pages: &mut BufferPool, key: i64, rid: RecordId) -> io::Result<()> {
        self.insert_node(pages, self.root, key, rid)?;
        Ok(())
    }

    /// Removes `key` from its leaf.
    ///
    /// Returns `Ok(true)` when an entry was removed and `Ok(false)` when the
    /// key was absent. Absent keys do not write. A removal stays dirty until
    /// the caller flushes. Empty leaves stay in the tree.
    pub fn delete(&self, pages: &mut BufferPool, key: i64) -> io::Result<bool> {
        let leaf_id = self.find_leaf_page(pages, key)?;
        let Node::Leaf { next, mut entries } = read_node(pages, leaf_id, self.owner)? else {
            return Err(invalid_index_page(leaf_id));
        };
        let Some(index) = find_leaf(&entries, key) else {
            return Ok(false);
        };
        entries.remove(index);
        write_leaf(pages, leaf_id, self.owner, next, &entries)?;
        Ok(true)
    }

    /// Every live key, in ascending order, by walking the leaf chain.
    pub fn scan_all(&self, pages: &mut BufferPool) -> io::Result<Vec<(i64, RecordId)>> {
        let mut page_id = self.leftmost_leaf(pages)?;
        let limit = pages.page_count()?;
        let mut rows = Vec::new();
        for _ in 0..limit {
            if page_id == PageId(0) {
                return Ok(rows);
            }
            match read_node(pages, page_id, self.owner)? {
                Node::Leaf { next, entries } => {
                    for entry in entries {
                        rows.push((entry.key, entry.rid));
                    }
                    page_id = next;
                }
                Node::Internal { .. } => return Err(invalid_index_page(page_id)),
            }
        }
        Err(invalid_index_page(page_id))
    }

    /// Number of levels. A root that is a leaf has height 1.
    pub fn height(&self, pages: &mut BufferPool) -> io::Result<u32> {
        let mut page_id = self.root;
        let limit = pages.page_count()?;
        for level in 1..=limit {
            match read_node(pages, page_id, self.owner)? {
                Node::Leaf { .. } => return Ok(level),
                Node::Internal { leftmost, .. } => page_id = leftmost,
            }
        }
        Err(invalid_index_page(page_id))
    }

    fn insert_node(
        &self,
        pages: &mut BufferPool,
        page_id: PageId,
        key: i64,
        rid: RecordId,
    ) -> io::Result<Option<Split>> {
        let is_root = page_id == self.root;
        match read_node(pages, page_id, self.owner)? {
            Node::Leaf { next, mut entries } => {
                match entries.binary_search_by_key(&key, |entry| entry.key) {
                    Ok(_) => return Err(duplicate_key(key)),
                    Err(index) => entries.insert(index, LeafEntry { key, rid }),
                }
                if entries.len() <= self.leaf_capacity {
                    write_leaf(pages, page_id, self.owner, next, &entries)?;
                    return Ok(None);
                }
                let (left, right, separator) = split_leaf(entries);
                if is_root {
                    self.split_root_leaf(pages, next, &left, &right, separator)?;
                    Ok(None)
                } else {
                    let right_id = pages.allocate_page()?;
                    write_leaf(pages, right_id, self.owner, next, &right)?;
                    write_leaf(pages, page_id, self.owner, right_id, &left)?;
                    Ok(Some(Split {
                        key: separator,
                        right: right_id,
                    }))
                }
            }
            Node::Internal {
                leftmost,
                mut entries,
            } => {
                let child = child_for(leftmost, &entries, key);
                let Some(split) = self.insert_node(pages, child, key, rid)? else {
                    return Ok(None);
                };
                let index = entries
                    .binary_search_by_key(&split.key, |entry| entry.key)
                    .unwrap_or_else(|index| index);
                entries.insert(
                    index,
                    InternalEntry {
                        key: split.key,
                        child: split.right,
                    },
                );
                if entries.len() <= self.internal_capacity {
                    write_internal(pages, page_id, self.owner, leftmost, &entries)?;
                    return Ok(None);
                }
                let (promoted, right_leftmost, right_entries) = split_internal(&mut entries);
                if is_root {
                    self.split_root_internal(
                        pages,
                        leftmost,
                        &entries,
                        promoted,
                        right_leftmost,
                        &right_entries,
                    )?;
                    Ok(None)
                } else {
                    let right_id = pages.allocate_page()?;
                    write_internal(pages, right_id, self.owner, right_leftmost, &right_entries)?;
                    write_internal(pages, page_id, self.owner, leftmost, &entries)?;
                    Ok(Some(Split {
                        key: promoted,
                        right: right_id,
                    }))
                }
            }
        }
    }

    fn split_root_leaf(
        &self,
        pages: &mut BufferPool,
        old_next: PageId,
        left: &[LeafEntry],
        right: &[LeafEntry],
        separator: i64,
    ) -> io::Result<()> {
        let left_id = pages.allocate_page()?;
        let right_id = pages.allocate_page()?;
        write_leaf(pages, left_id, self.owner, right_id, left)?;
        write_leaf(pages, right_id, self.owner, old_next, right)?;
        let entries = [InternalEntry {
            key: separator,
            child: right_id,
        }];
        write_internal(pages, self.root, self.owner, left_id, &entries)
    }

    fn split_root_internal(
        &self,
        pages: &mut BufferPool,
        left_leftmost: PageId,
        left_entries: &[InternalEntry],
        promoted: i64,
        right_leftmost: PageId,
        right_entries: &[InternalEntry],
    ) -> io::Result<()> {
        let left_id = pages.allocate_page()?;
        let right_id = pages.allocate_page()?;
        write_internal(pages, left_id, self.owner, left_leftmost, left_entries)?;
        write_internal(pages, right_id, self.owner, right_leftmost, right_entries)?;
        let entries = [InternalEntry {
            key: promoted,
            child: right_id,
        }];
        write_internal(pages, self.root, self.owner, left_id, &entries)
    }

    fn find_leaf_page(&self, pages: &mut BufferPool, key: i64) -> io::Result<PageId> {
        let mut page_id = self.root;
        let limit = pages.page_count()?;
        for _ in 0..limit {
            match read_node(pages, page_id, self.owner)? {
                Node::Leaf { .. } => return Ok(page_id),
                Node::Internal { leftmost, entries } => {
                    page_id = child_for(leftmost, &entries, key);
                }
            }
        }
        Err(invalid_index_page(page_id))
    }

    fn leftmost_leaf(&self, pages: &mut BufferPool) -> io::Result<PageId> {
        let mut page_id = self.root;
        let limit = pages.page_count()?;
        for _ in 0..limit {
            match read_node(pages, page_id, self.owner)? {
                Node::Leaf { .. } => return Ok(page_id),
                Node::Internal { leftmost, .. } => page_id = leftmost,
            }
        }
        Err(invalid_index_page(page_id))
    }
}

/// Left takes `ceil(len / 2)` entries. The separator is the right leaf's first key.
fn split_leaf(mut entries: Vec<LeafEntry>) -> (Vec<LeafEntry>, Vec<LeafEntry>, i64) {
    let mid = entries.len().div_ceil(2);
    let right = entries.split_off(mid);
    let separator = right[0].key;
    (entries, right, separator)
}

/// Promotes the middle key. `entries` is left as the left node's keys.
fn split_internal(entries: &mut Vec<InternalEntry>) -> (i64, PageId, Vec<InternalEntry>) {
    let mid = entries.len() / 2;
    let promoted = entries[mid].key;
    let right_leftmost = entries[mid].child;
    let right_entries = entries.split_off(mid + 1);
    entries.pop();
    (promoted, right_leftmost, right_entries)
}

fn find_leaf(entries: &[LeafEntry], key: i64) -> Option<usize> {
    entries.binary_search_by_key(&key, |entry| entry.key).ok()
}

fn child_for(leftmost: PageId, entries: &[InternalEntry], key: i64) -> PageId {
    match entries.binary_search_by_key(&key, |entry| entry.key) {
        Ok(index) => entries[index].child,
        Err(0) => leftmost,
        Err(index) => entries[index - 1].child,
    }
}

fn read_node(pages: &mut BufferPool, page_id: PageId, owner: TableId) -> io::Result<Node> {
    let page = pages.read_page(page_id)?;
    parse_node(page_id, owner, page.data())
}

fn parse_node(page_id: PageId, owner: TableId, data: &[u8]) -> io::Result<Node> {
    if data.len() < HEADER_LEN || data[0] != PAGE_TYPE_BTREE {
        return Err(invalid_index_page(page_id));
    }
    let kind = data[1];
    let key_count = u16::from_le_bytes([data[2], data[3]]) as usize;
    let page_owner = u16::from_le_bytes([data[4], data[5]]);
    if page_owner != owner.0 {
        return Err(invalid_index_page(page_id));
    }
    let link = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
    match kind {
        NODE_LEAF => {
            if key_count > LEAF_CAPACITY {
                return Err(invalid_index_page(page_id));
            }
            let mut entries = Vec::with_capacity(key_count);
            for index in 0..key_count {
                let start = HEADER_LEN + index * LEAF_ENTRY_LEN;
                let end = start + LEAF_ENTRY_LEN;
                let entry = data
                    .get(start..end)
                    .ok_or_else(|| invalid_index_page(page_id))?;
                entries.push(LeafEntry {
                    key: read_i64(&entry[0..8]),
                    rid: RecordId {
                        page_id: PageId(read_u32(&entry[8..12])),
                        slot_id: read_u16(&entry[12..14]),
                    },
                });
            }
            Ok(Node::Leaf {
                next: PageId(link),
                entries,
            })
        }
        NODE_INTERNAL => {
            if key_count > INTERNAL_CAPACITY || link == 0 {
                return Err(invalid_index_page(page_id));
            }
            let mut entries = Vec::with_capacity(key_count);
            for index in 0..key_count {
                let start = HEADER_LEN + index * INTERNAL_ENTRY_LEN;
                let end = start + INTERNAL_ENTRY_LEN;
                let entry = data
                    .get(start..end)
                    .ok_or_else(|| invalid_index_page(page_id))?;
                let child = read_u32(&entry[8..12]);
                if child == 0 {
                    return Err(invalid_index_page(page_id));
                }
                entries.push(InternalEntry {
                    key: read_i64(&entry[0..8]),
                    child: PageId(child),
                });
            }
            Ok(Node::Internal {
                leftmost: PageId(link),
                entries,
            })
        }
        _ => Err(invalid_index_page(page_id)),
    }
}

fn write_leaf(
    pages: &mut BufferPool,
    page_id: PageId,
    owner: TableId,
    next: PageId,
    entries: &[LeafEntry],
) -> io::Result<()> {
    debug_assert!(entries.windows(2).all(|pair| pair[0].key < pair[1].key));
    let mut page = Page::zeroed();
    write_header(page.data_mut(), NODE_LEAF, entries.len(), owner, next.0)?;
    for (index, entry) in entries.iter().enumerate() {
        let start = HEADER_LEN + index * LEAF_ENTRY_LEN;
        let slot = &mut page.data_mut()[start..start + LEAF_ENTRY_LEN];
        slot[0..8].copy_from_slice(&entry.key.to_le_bytes());
        slot[8..12].copy_from_slice(&entry.rid.page_id.0.to_le_bytes());
        slot[12..14].copy_from_slice(&entry.rid.slot_id.to_le_bytes());
    }
    pages.write_page(page_id, &page)
}

fn write_internal(
    pages: &mut BufferPool,
    page_id: PageId,
    owner: TableId,
    leftmost: PageId,
    entries: &[InternalEntry],
) -> io::Result<()> {
    debug_assert!(leftmost != PageId(0));
    debug_assert!(entries.windows(2).all(|pair| pair[0].key < pair[1].key));
    let mut page = Page::zeroed();
    write_header(
        page.data_mut(),
        NODE_INTERNAL,
        entries.len(),
        owner,
        leftmost.0,
    )?;
    for (index, entry) in entries.iter().enumerate() {
        let start = HEADER_LEN + index * INTERNAL_ENTRY_LEN;
        let slot = &mut page.data_mut()[start..start + INTERNAL_ENTRY_LEN];
        slot[0..8].copy_from_slice(&entry.key.to_le_bytes());
        slot[8..12].copy_from_slice(&entry.child.0.to_le_bytes());
    }
    pages.write_page(page_id, &page)
}

fn write_header(
    data: &mut [u8],
    kind: u8,
    key_count: usize,
    owner: TableId,
    link: u32,
) -> io::Result<()> {
    let key_count = u16::try_from(key_count)
        .map_err(|_| io::Error::new(ErrorKind::InvalidData, "invalid index page: 0"))?;
    data[0] = PAGE_TYPE_BTREE;
    data[1] = kind;
    data[2..4].copy_from_slice(&key_count.to_le_bytes());
    data[4..6].copy_from_slice(&owner.0.to_le_bytes());
    data[8..12].copy_from_slice(&link.to_le_bytes());
    Ok(())
}

fn read_i64(bytes: &[u8]) -> i64 {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(bytes);
    i64::from_le_bytes(buf)
}

fn read_u32(bytes: &[u8]) -> u32 {
    let mut buf = [0u8; 4];
    buf.copy_from_slice(bytes);
    u32::from_le_bytes(buf)
}

fn read_u16(bytes: &[u8]) -> u16 {
    let mut buf = [0u8; 2];
    buf.copy_from_slice(bytes);
    u16::from_le_bytes(buf)
}

fn duplicate_key(key: i64) -> io::Error {
    io::Error::new(
        ErrorKind::InvalidInput,
        format!("duplicate primary key: {key}"),
    )
}

fn invalid_index_page(page_id: PageId) -> io::Error {
    io::Error::new(
        ErrorKind::InvalidData,
        format!("invalid index page: {page_id}"),
    )
}

#[cfg(test)]
mod tests {
    use super::{BTree, INTERNAL_CAPACITY, LEAF_CAPACITY};
    use crate::buffer::BufferPool;
    use crate::page::PageId;
    use crate::record::{RecordId, TableId};
    use std::collections::BTreeMap;
    use std::env::temp_dir;
    use std::fs;
    use std::io::ErrorKind;
    use std::path::PathBuf;
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDb {
        path: PathBuf,
    }

    impl TempDb {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let mut path = temp_dir();
            path.push(format!("sqltoy-btree-{label}-{}-{nanos}", process::id()));
            let _ = fs::remove_file(&path);
            let _ = fs::remove_file(crate::wal::wal_path(&path));
            TempDb { path }
        }

        fn path(&self) -> &PathBuf {
            &self.path
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
            let _ = fs::remove_file(crate::wal::wal_path(&self.path));
        }
    }

    const OWNER: TableId = TableId(2);

    fn rid(page: u32, slot: u16) -> RecordId {
        RecordId {
            page_id: PageId(page),
            slot_id: slot,
        }
    }

    fn fresh(label: &str) -> (TempDb, BufferPool, PageId) {
        let db = TempDb::new(label);
        let mut pages = BufferPool::open(db.path(), 8).unwrap();
        let root = BTree::create(&mut pages, OWNER).unwrap();
        (db, pages, root)
    }

    #[test]
    fn capacities_match_the_page_layout() {
        assert_eq!(LEAF_CAPACITY, 255);
        assert_eq!(INTERNAL_CAPACITY, 340);
    }

    #[test]
    fn empty_leaf_root_layout() {
        let (_db, mut pages, root) = fresh("empty");
        let page = pages.read_page(root).unwrap();
        let data = page.data();
        assert_eq!(data[0], 2);
        assert_eq!(data[1], 1);
        assert_eq!(&data[2..4], &0u16.to_le_bytes());
        assert_eq!(&data[4..6], &OWNER.0.to_le_bytes());
        assert_eq!(&data[6..8], &[0, 0]);
        assert_eq!(&data[8..12], &0u32.to_le_bytes());
        assert_eq!(&data[12..16], &[0, 0, 0, 0]);
        assert!(data[16..].iter().all(|byte| *byte == 0));

        let tree = BTree::open(root, OWNER);
        assert!(tree.get(&mut pages, 0).unwrap().is_none());
        assert!(tree.scan_all(&mut pages).unwrap().is_empty());
        assert_eq!(tree.height(&mut pages).unwrap(), 1);
        assert!(!tree.delete(&mut pages, 0).unwrap());
    }

    #[test]
    fn leaf_entry_is_little_endian() {
        let (_db, mut pages, root) = fresh("layout");
        let tree = BTree::open(root, OWNER);
        let id = rid(4, 5);
        tree.insert(&mut pages, -1, id).unwrap();
        let page = pages.read_page(root).unwrap();
        let data = page.data();
        assert_eq!(&data[2..4], &1u16.to_le_bytes());
        assert_eq!(&data[16..24], &(-1i64).to_le_bytes());
        assert_eq!(&data[24..28], &4u32.to_le_bytes());
        assert_eq!(&data[28..30], &5u16.to_le_bytes());
        assert_eq!(&data[30..32], &0u16.to_le_bytes());
        assert_eq!(tree.get(&mut pages, -1).unwrap(), Some(id));
    }

    #[test]
    fn leaf_split_gives_the_left_the_ceiling_half() {
        let (_db, mut pages, root) = fresh("split");
        let tree = BTree::with_capacities(root, OWNER, 2, 2);
        for key in 1..=3 {
            tree.insert(&mut pages, key, rid(1, key as u16)).unwrap();
        }
        assert_eq!(tree.root(), root);
        assert_eq!(tree.height(&mut pages).unwrap(), 2);
        let page = pages.read_page(root).unwrap();
        assert_eq!(page.data()[1], 2);
        let separator = i64::from_le_bytes(page.data()[16..24].try_into().unwrap());
        assert_eq!(separator, 3);
        assert_eq!(
            tree.scan_all(&mut pages).unwrap(),
            vec![(1, rid(1, 1)), (2, rid(1, 2)), (3, rid(1, 3))]
        );
    }

    #[test]
    fn ascending_descending_and_extreme_keys() {
        let (_db, mut pages, root) = fresh("order");
        let tree = BTree::with_capacities(root, OWNER, 4, 4);
        for key in (0..80).rev() {
            tree.insert(&mut pages, key, rid(2, key as u16)).unwrap();
        }
        tree.insert(&mut pages, i64::MIN, rid(9, 1)).unwrap();
        tree.insert(&mut pages, i64::MAX, rid(9, 2)).unwrap();
        tree.insert(&mut pages, -7, rid(9, 3)).unwrap();
        let scanned = tree.scan_all(&mut pages).unwrap();
        let mut keys: Vec<i64> = scanned.iter().map(|(key, _)| *key).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
        assert_eq!(keys[0], i64::MIN);
        assert_eq!(*keys.last().unwrap(), i64::MAX);
        assert_eq!(tree.get(&mut pages, i64::MIN).unwrap(), Some(rid(9, 1)));
        assert_eq!(tree.get(&mut pages, i64::MAX).unwrap(), Some(rid(9, 2)));
        assert_eq!(tree.get(&mut pages, -7).unwrap(), Some(rid(9, 3)));
        assert!(tree.get(&mut pages, -8).unwrap().is_none());
        assert!(tree.height(&mut pages).unwrap() >= 2);
        assert_eq!(tree.root(), root);
        keys = tree
            .scan_all(&mut pages)
            .unwrap()
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert!(keys.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn duplicate_key_leaves_the_tree_unchanged() {
        let (_db, mut pages, root) = fresh("dup");
        let tree = BTree::with_capacities(root, OWNER, 3, 3);
        for key in [1, 5, 3, 9, 7] {
            tree.insert(&mut pages, key, rid(1, key as u16)).unwrap();
        }
        let before = tree.scan_all(&mut pages).unwrap();
        let pages_before = pages.page_count().unwrap();
        let err = tree.insert(&mut pages, 5, rid(8, 8)).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "duplicate primary key: 5");
        assert_eq!(tree.scan_all(&mut pages).unwrap(), before);
        assert_eq!(pages.page_count().unwrap(), pages_before);
        assert_eq!(tree.get(&mut pages, 5).unwrap(), Some(rid(1, 5)));
    }

    #[test]
    fn delete_removes_the_key_and_keeps_empty_leaves() {
        let (_db, mut pages, root) = fresh("del");
        let tree = BTree::with_capacities(root, OWNER, 3, 3);
        for key in 0..30 {
            tree.insert(&mut pages, key, rid(1, key as u16)).unwrap();
        }
        let height = tree.height(&mut pages).unwrap();
        assert!(height >= 2);
        let pages_before = pages.page_count().unwrap();
        assert!(tree.delete(&mut pages, 10).unwrap());
        assert!(tree.get(&mut pages, 10).unwrap().is_none());
        assert!(!tree.delete(&mut pages, 10).unwrap());
        assert_eq!(pages.page_count().unwrap(), pages_before);
        for key in 0..30 {
            if key != 10 {
                tree.delete(&mut pages, key).unwrap();
            }
        }
        assert!(tree.scan_all(&mut pages).unwrap().is_empty());
        assert_eq!(tree.height(&mut pages).unwrap(), height);
        assert_eq!(pages.page_count().unwrap(), pages_before);
        tree.insert(&mut pages, 10, rid(3, 1)).unwrap();
        assert_eq!(tree.get(&mut pages, 10).unwrap(), Some(rid(3, 1)));
    }

    #[test]
    fn height_grows_and_root_page_stays() {
        let (_db, mut pages, root) = fresh("height");
        let tree = BTree::with_capacities(root, OWNER, 4, 4);
        assert_eq!(tree.height(&mut pages).unwrap(), 1);
        let mut height = 1;
        for key in 0..120 {
            tree.insert(&mut pages, key, rid(1, 0)).unwrap();
            let next = tree.height(&mut pages).unwrap();
            assert!(next >= height);
            height = next;
            assert_eq!(tree.root(), root);
        }
        assert!(height >= 3);
        let page = pages.read_page(root).unwrap();
        assert_eq!(page.data()[0], 2);
        assert_eq!(page.data()[1], 2);
    }

    #[test]
    fn random_ops_match_a_btreemap() {
        let (_db, mut pages, root) = fresh("random");
        let tree = BTree::with_capacities(root, OWNER, 4, 4);
        let mut model: BTreeMap<i64, RecordId> = BTreeMap::new();
        let mut state = 0x1234_5678_9abc_def0u64;
        let mut next = || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            state
        };
        for _ in 0..3000 {
            let key = (next() % 400) as i64 - 80;
            if next() % 5 == 0 {
                let removed = tree.delete(&mut pages, key).unwrap();
                assert_eq!(removed, model.remove(&key).is_some());
            } else {
                let id = rid((next() % 50) as u32 + 1, (next() % 20) as u16);
                match tree.insert(&mut pages, key, id) {
                    Ok(()) => {
                        assert!(model.insert(key, id).is_none());
                    }
                    Err(err) => {
                        assert_eq!(err.kind(), ErrorKind::InvalidInput);
                        assert_eq!(err.to_string(), format!("duplicate primary key: {key}"));
                        assert!(model.contains_key(&key));
                    }
                }
            }
        }
        let scanned = tree.scan_all(&mut pages).unwrap();
        let expected: Vec<(i64, RecordId)> = model.into_iter().collect();
        assert_eq!(scanned, expected);
        assert!(scanned.windows(2).all(|pair| pair[0].0 < pair[1].0));
        assert_eq!(tree.root(), root);
        assert!(tree.height(&mut pages).unwrap() >= 2);
    }

    #[test]
    fn real_capacity_persists_across_reopen() {
        let db = TempDb::new("persist");
        let root;
        {
            let mut pages = BufferPool::open(db.path(), 8).unwrap();
            root = BTree::create(&mut pages, OWNER).unwrap();
            let tree = BTree::open(root, OWNER);
            for key in 0..300 {
                tree.insert(&mut pages, key, rid(1, (key % 100) as u16))
                    .unwrap();
            }
            tree.insert(&mut pages, -5, rid(7, 1)).unwrap();
            assert!(tree.height(&mut pages).unwrap() >= 2);
            assert_eq!(tree.root(), root);
            pages.flush().unwrap();
        }
        let mut pages = BufferPool::open(db.path(), 8).unwrap();
        let tree = BTree::open(root, OWNER);
        assert_eq!(tree.root(), root);
        assert!(tree.height(&mut pages).unwrap() >= 2);
        assert_eq!(tree.get(&mut pages, 0).unwrap(), Some(rid(1, 0)));
        assert_eq!(tree.get(&mut pages, 299).unwrap(), Some(rid(1, 99)));
        assert_eq!(tree.get(&mut pages, -5).unwrap(), Some(rid(7, 1)));
        assert!(tree.get(&mut pages, 300).unwrap().is_none());
        let scanned = tree.scan_all(&mut pages).unwrap();
        assert_eq!(scanned.len(), 301);
        assert!(scanned.windows(2).all(|pair| pair[0].0 < pair[1].0));
        assert_eq!(scanned[0].0, -5);
        let page = pages.read_page(root).unwrap();
        assert_eq!(page.data()[1], 2);
    }

    #[test]
    fn invalid_pages_and_wrong_owner_are_rejected() {
        let (_db, mut pages, root) = fresh("bad");
        let zero = pages.allocate_page().unwrap();
        let tree = BTree::open(zero, OWNER);
        let err = tree.get(&mut pages, 1).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData);
        assert_eq!(err.to_string(), format!("invalid index page: {zero}"));

        let tree = BTree::open(root, TableId(9));
        let err = tree.get(&mut pages, 1).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData);
        assert_eq!(err.to_string(), format!("invalid index page: {root}"));

        let mut record = pages.read_page(zero).unwrap();
        record.data_mut()[0] = 1;
        pages.write_page(zero, &record).unwrap();
        let tree = BTree::open(zero, OWNER);
        let err = tree.scan_all(&mut pages).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData);
        assert_eq!(err.to_string(), format!("invalid index page: {zero}"));
    }
}
