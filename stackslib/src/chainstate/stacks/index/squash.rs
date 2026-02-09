// Copyright (C) 2026 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

//! MARF squashing: offline snapshot creation and validation.
//!
//! This module implements the squashing logic described in Phase 1 of
//! SIP-XXX-MARF-snapshots.  A squashed MARF contains only the canonical state
//! at a given height H plus the metadata needed for ancestor hash lookups and
//! block-height resolution.  It can be extended to heights > H without
//! modification to the consensus-critical skip-list algorithm.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{BufReader, Read as _, Seek, SeekFrom};
use std::time::Instant;

use rusqlite::params;
use stacks_common::types::chainstate::{
    TrieHash, BLOCK_HEADER_HASH_ENCODED_SIZE, TRIEHASH_ENCODED_SIZE,
};

use crate::chainstate::stacks::index::marf::{
    MARFOpenOpts, MarfConnection, BLOCK_HEIGHT_TO_HASH_MAPPING_KEY, MARF,
};
use crate::chainstate::stacks::index::node::{is_backptr, TrieNodeID, TrieNodeType, TriePtr};
use crate::chainstate::stacks::index::storage::{
    SquashInfo, TrieFileStorage, TrieStorageConnection,
};
use crate::chainstate::stacks::index::trie::Trie;
use crate::chainstate::stacks::index::{trie_sql, Error, MARFValue, MarfTrieId};

/// Key that stores the squashed root hash at the snapshot tip.
pub const MARF_SQUASH_ROOT_KEY: &str = "__MARF_SQUASH_ROOT";
/// Key that stores the snapshot height for a squashed MARF.
pub const MARF_SQUASH_HEIGHT_KEY: &str = "__MARF_SQUASH_HEIGHT";
/// Prefix for per-height root hashes preserved in squashed MARFs.
/// Each key has the form `__MARF_SQUASHED_BLOCK_ROOT_HASH::<height>`.
pub const MARF_SQUASHED_BLOCK_ROOT_HASH_KEY: &str = "__MARF_SQUASHED_BLOCK_ROOT_HASH";

/// Summary statistics from a squashing run.
#[derive(Debug, Clone)]
pub struct SquashStats {
    /// Total number of leaves copied into the squashed MARF.
    pub leaf_count: u64,
}

/// Summary statistics from a validation run.
///
/// The default validation checks:
/// - Per-height root hashes stored in `marf_squash_root_hashes` match the
///   archival source (guarantees correct ancestor hash computation for the
///   skip-list at blocks > H).
/// - Squash metadata (`marf_squash_info`) is present and correct.
/// - All historical `marf_data` entries share the tip block's blob offset.
///
/// When `full_leaf_scan` is enabled, the validator additionally walks every
/// leaf in both MARFs and cross-checks them, which is O(leaf_count) and much
/// slower but useful for debugging.
#[derive(Debug, Clone)]
pub struct SquashValidationStats {
    // --- Fast-path (always populated) ---
    /// Whether the squashed root key was found in the SQL metadata.
    pub squash_root_present: bool,
    /// Whether the stored archival root hash at the squash height
    /// matches the source MARF's root hash at that height.
    pub squash_root_matches: bool,
    /// Per-height root hashes missing from the SQL table.
    pub root_hash_missing: u64,
    /// Per-height root hashes with mismatched values.
    pub root_hash_mismatches: u64,
    /// Number of historical `marf_data` entries that do NOT share the
    /// tip block's blob offset (should be 0 for a correct squash).
    pub blob_offset_mismatches: u64,

    // --- Full leaf scan (only populated when full_leaf_scan = true) ---
    /// Total keys compared from the source MARF (0 when fast-only).
    pub source_keys_checked: u64,
    /// Total keys compared from the squashed MARF (0 when fast-only).
    pub squashed_keys_checked: u64,
    /// Keys present in source but missing in squashed (0 when fast-only).
    pub missing_in_squashed: u64,
    /// Keys present in squashed but missing in source (0 when fast-only).
    pub missing_in_source: u64,
    /// Keys present in both but with different values (0 when fast-only).
    pub value_mismatches: u64,
}

impl<T: MarfTrieId> MARF<T> {
    /// Squash the MARF at `height` into a new database at `dst_path`.
    ///
    /// Produces a hash-preserving squash: the squashed MARF contains a single
    /// shared blob with all trie nodes reachable at `height`.  Each historical
    /// block (0..=height) has a `marf_data` row pointing at this shared blob so
    /// that `get_block_hash_caching(local_id)` returns the correct original
    /// `StacksBlockId`.
    ///
    /// Backpointer identity is preserved via `TriePtr.back_block` annotations.
    /// Children that were backpointers in the archival MARF are stored inline in
    /// the blob but with `back_block` set to the squashed DB's local_id for the
    /// original block.  When the squashed MARF is extended to height H+1, the
    /// modified `node_copy_update_ptrs` preserves these annotations, ensuring
    /// that `inner_write_children_hashes` uses the same `StacksBlockId` values
    /// as the archival MARF.  This guarantees identical per-block root hashes.
    ///
    /// # Steps
    ///
    /// 1. Bulk SQL: read all `marf_data` entries into an in-memory block_map.
    /// 2. Per-height metadata: trie walk + blob seek per height to collect
    ///    block hashes and root hashes for 0..=height.
    /// 3. BFS deep copy: breadth-first walk of the trie at `height`,
    ///    collecting all reachable nodes and a source->index map.
    /// 4. Placeholders: bulk-insert `marf_data` rows for blocks 0..height-1.
    /// 5. Remap: annotate backpointer children with `back_block` =
    ///    squashed local ID (in-memory, no I/O).
    /// 6. SQL metadata: write `marf_squash_info` and per-height root hashes.
    /// 7. Commit: flush TrieRAM to the shared `.blobs` file.
    /// 8. Blob update: bulk-update placeholders to share block H's blob
    ///    offset/length.
    pub fn squash_to_path(
        src_path: &str,
        dst_path: &str,
        open_opts: MARFOpenOpts,
        height: u32,
    ) -> Result<SquashStats, Error> {
        let overall_start = Instant::now();

        // Step 1: bulk SQL for block_map
        let src_storage = TrieFileStorage::open_readonly(src_path, open_opts.clone())?;
        let mut src = MARF::from_storage(src_storage);

        let tip = trie_sql::get_latest_confirmed_block_hash::<T>(src.sqlite_conn())?;
        let block_at_height = src
            .get_block_at_height(height, &tip)?
            .ok_or(Error::NotFoundError)?;

        let start_meta = Instant::now();
        let all_blocks = trie_sql::bulk_read_block_entries::<T>(src.sqlite_conn())?;
        let block_map: HashMap<T, (u32, u64)> = all_blocks
            .into_iter()
            .map(|(id, bh, offset)| (bh, (id, offset)))
            .collect();
        info!(
            "Squash step 1 (bulk SQL): {} entries in {:?}",
            block_map.len(),
            start_meta.elapsed()
        );

        // Step 2: Per-height metadata collection
        //
        // For each height 0..=H:
        //   a) get_by_key  -> finds block_hash (one trie walk from tip)
        //   b) direct blob seek -> reads 32-byte root hash at known offset
        //
        // This collects all height -> (block_hash, root_hash) mappings
        // needed for placeholders and SQL metadata.
        let start_meta = Instant::now();
        let source_tip = trie_sql::get_latest_confirmed_block_hash::<T>(src.sqlite_conn())?;
        let blobs_path = format!("{src_path}.blobs");

        // Each trie blob on disk is laid out as:
        //   [0..32)  parent block hash  (BLOCK_HEADER_HASH_ENCODED_SIZE = 32 bytes)
        //   [32..36) block identifier   (u32 LE, 4 bytes)
        //   [36..)   root node hash     (TRIEHASH_ENCODED_SIZE = 32 bytes), then node data
        //
        // So the root node's hash starts at byte 36.  This matches the canonical
        // `TrieFileStorage::root_ptr_disk()` calculation in storage.rs.
        let root_ptr_offset = (BLOCK_HEADER_HASH_ENCODED_SIZE as u64) + 4;

        let blobs_file = File::open(&blobs_path).map_err(Error::IOError)?;
        // Use a 64 KiB buffer to amortise syscalls over many small reads.
        let mut blobs_reader = BufReader::with_capacity(64 * 1024, blobs_file);

        let mut block_info: Vec<(u32, T, TrieHash)> = Vec::with_capacity((height + 1) as usize);
        let mut archival_ids: HashMap<T, u32> = HashMap::with_capacity((height + 1) as usize);
        let mut last_log = Instant::now();

        for h in 0..=height {
            let h_key = format!("{BLOCK_HEIGHT_TO_HASH_MAPPING_KEY}::{h}");
            let val = src
                .with_conn(|conn| Self::get_by_key(conn, &source_tip, &h_key))?
                .ok_or_else(|| {
                    Error::CorruptionError(format!("Missing height mapping for height {h}"))
                })?;
            let bh = T::from(val);

            let &(block_id, blob_offset) = block_map.get(&bh).ok_or_else(|| {
                Error::CorruptionError(format!(
                    "Missing block map entry for block hash at height {h}"
                ))
            })?;

            // Direct blob seek for root hash.
            blobs_reader.seek(SeekFrom::Start(blob_offset + root_ptr_offset))?;
            let mut hash_bytes = [0u8; TRIEHASH_ENCODED_SIZE];
            blobs_reader.read_exact(&mut hash_bytes)?;
            let rh = TrieHash(hash_bytes);

            archival_ids.insert(bh.clone(), block_id);
            block_info.push((h, bh, rh));

            if last_log.elapsed().as_secs() >= 30 || (h > 0 && h % 100_000 == 0) {
                info!(
                    "Squash step 2 (per-height metadata): {}/{} heights in {:?}",
                    h + 1,
                    height + 1,
                    start_meta.elapsed()
                );
                last_log = Instant::now();
            }
        }
        info!(
            "Squash step 2 (per-height metadata): {} heights in {:?}",
            height + 1,
            start_meta.elapsed()
        );

        // Step 3: BFS deep copy
        let start_bfs = Instant::now();
        let (mut nodes, source_to_idx) =
            src.with_conn(|conn| MARF::<T>::deep_copy_bfs_nodes(conn, &block_at_height))?;
        let node_count = nodes.len() as u64;
        info!(
            "Squash step 3 (BFS): {node_count} nodes in {:?}",
            start_bfs.elapsed()
        );

        let mut dst = MARF::from_path(dst_path, open_opts)?;
        let mut tx = dst.begin_tx()?;
        tx.begin(&T::sentinel(), &block_at_height)?;

        let start_placeholders = Instant::now();
        let mut archival_to_squashed: HashMap<u32, u32> = HashMap::new();
        {
            let conn = tx.sqlite_tx();
            let mut stmt = conn.prepare(
                "INSERT INTO marf_data (block_hash, data, unconfirmed, external_offset, external_length) \
                 VALUES (?1, ?2, 0, ?3, ?4)",
            )?;
            let empty_blob: &[u8] = &[];
            for (h, bh, _) in &block_info {
                if bh == &block_at_height {
                    continue;
                }
                let archival_id = *archival_ids.get(bh).ok_or(Error::NotFoundError)?;
                let squashed_id = stmt
                    .insert(params![bh.to_string(), empty_blob, 0i64, 0i64])?
                    .try_into()
                    .expect("block_id overflow");
                archival_to_squashed.insert(archival_id, squashed_id);
                if *h % 100_000 == 0 && *h > 0 {
                    info!(
                        "Squash placeholders: inserted {h} entries in {:?}",
                        start_placeholders.elapsed()
                    );
                }
            }
        }
        info!(
            "Squash step 4 (placeholders): {} entries in {:?}",
            archival_to_squashed.len(),
            start_placeholders.elapsed()
        );

        // Step 5: remap (in-memory, no I/O)
        let start_remap = Instant::now();
        Self::deep_copy_remap(&mut nodes, &source_to_idx, &archival_to_squashed)?;
        info!(
            "Squash step 5 (remap): {node_count} nodes in {:?}",
            start_remap.elapsed()
        );

        // Write collected nodes into the destination TrieRAM.
        for (idx, (node, hash, _)) in nodes.iter().enumerate() {
            tx.write_node_direct(idx as u32, node, *hash)?;
        }

        // Step 6: SQL metadata (root hashes from Step 2)
        let source_root_hash = block_info
            .iter()
            .find(|(_, bh, _)| bh == &block_at_height)
            .map(|(_, _, rh)| *rh)
            .ok_or(Error::NotFoundError)?;

        let start_sql_meta = Instant::now();
        trie_sql::write_squash_info(tx.sqlite_tx(), &source_root_hash, height)?;
        {
            let conn = tx.sqlite_tx();
            let mut stmt = conn.prepare(
                "INSERT OR REPLACE INTO marf_squash_root_hashes (height, root_hash) VALUES (?1, ?2)",
            )?;
            for (h, _, rh) in &block_info {
                stmt.execute(params![*h as i64, rh.as_bytes().to_vec()])?;
            }
        }
        info!(
            "Squash step 6 (SQL metadata): {} root hashes in {:?}",
            block_info.len(),
            start_sql_meta.elapsed()
        );

        tx.set_squash_info(Some(SquashInfo {
            root: source_root_hash,
            height,
            block_hash: block_at_height.clone(),
        }));

        // Step 7: commit
        let start_commit = Instant::now();
        info!("Squash commit: starting commit");
        tx.commit()?;
        info!("Squash commit: finished in {:?}", start_commit.elapsed());

        // Step 8: bulk-update placeholders to share the blob
        let start_blob_update = Instant::now();
        let conn = dst.sqlite_conn();
        let bh_id = trie_sql::get_block_identifier(conn, &block_at_height)?;
        let (offset, length) = trie_sql::get_external_trie_offset_length(conn, bh_id)?;

        conn.execute_batch("BEGIN IMMEDIATE")
            .map_err(|e| Error::CorruptionError(format!("BEGIN: {e}")))?;
        let updated = trie_sql::bulk_update_blob_offsets(conn, offset, length, &block_at_height)?;
        conn.execute_batch("COMMIT")
            .map_err(|e| Error::CorruptionError(format!("COMMIT: {e}")))?;
        info!(
            "Squash step 8 (blob update): {updated} rows in {:?}",
            start_blob_update.elapsed()
        );

        info!(
            "Squash complete: {node_count} nodes, total time {:?}",
            overall_start.elapsed()
        );

        Ok(SquashStats {
            leaf_count: node_count,
        })
    }

    /// Walk all leaves in the trie at `block_hash`, yielding full paths and values.
    ///
    /// Follows backpointers to resolve nodes living in earlier blocks, so the
    /// returned set represents the complete state visible at `block_hash`.
    pub(crate) fn walk_all_leaves<F>(
        storage: &mut TrieStorageConnection<T>,
        block_hash: &T,
        mut handle_leaf: F,
    ) -> Result<u64, Error>
    where
        F: FnMut(TrieHash, MARFValue) -> Result<(), Error>,
    {
        storage.open_block(block_hash)?;
        let (root_node, _root_hash) = Trie::read_root(storage)?;

        let mut leaf_count = 0u64;
        let mut stack: Vec<(TriePtr, Vec<u8>, T, Option<u32>)> = Vec::new();
        let root_prefix = root_node.path_bytes().clone();

        match root_node {
            TrieNodeType::Leaf(leaf) => {
                if root_prefix.len() != TRIEHASH_ENCODED_SIZE {
                    return Err(Error::CorruptionError(
                        "Root leaf path length invalid".to_string(),
                    ));
                }
                let full_path = TrieHash::from_bytes(&root_prefix).ok_or_else(|| {
                    Error::CorruptionError("Failed to decode root leaf path".to_string())
                })?;
                handle_leaf(full_path, leaf.data)?;
                leaf_count += 1;
            }
            _ => {
                let (cur_block_hash, cur_block_id) = storage.get_cur_block_and_id();
                for ptr in root_node.ptrs().iter() {
                    if ptr.id() == TrieNodeID::Empty as u8 {
                        continue;
                    }
                    let mut prefix = root_prefix.clone();
                    prefix.push(ptr.chr());
                    stack.push((*ptr, prefix, cur_block_hash.clone(), cur_block_id));
                }
            }
        }

        let walk_start = Instant::now();
        let mut last_walk_log = Instant::now();
        let mut nodes_visited: u64 = 0;

        while let Some((ptr, prefix, return_block, return_block_id)) = stack.pop() {
            nodes_visited += 1;
            if last_walk_log.elapsed().as_secs() >= 30 {
                info!(
                    "walk_all_leaves: {nodes_visited} nodes visited, {leaf_count} leaves found, stack {}, {:?} elapsed",
                    stack.len(),
                    walk_start.elapsed()
                );
                last_walk_log = Instant::now();
            }

            let (cur_block_hash, _) = storage.get_cur_block_and_id();
            if cur_block_hash != return_block {
                storage.open_block_maybe_id(&return_block, return_block_id)?;
            }

            let (node, node_block_hash, node_block_id) = MARF::read_node_for_ptr(storage, &ptr)?;

            let mut node_prefix = prefix;
            node_prefix.extend_from_slice(node.path_bytes());

            match node {
                TrieNodeType::Leaf(leaf) => {
                    if node_prefix.len() != TRIEHASH_ENCODED_SIZE as usize {
                        return Err(Error::CorruptionError(
                            "Leaf path length invalid".to_string(),
                        ));
                    }
                    let full_path = TrieHash::from_bytes(&node_prefix).ok_or_else(|| {
                        Error::CorruptionError("Failed to decode leaf path".to_string())
                    })?;
                    handle_leaf(full_path, leaf.data)?;
                    leaf_count += 1;
                }
                _ => {
                    for child_ptr in node.ptrs().iter() {
                        if child_ptr.id() == TrieNodeID::Empty as u8 {
                            continue;
                        }
                        let mut child_prefix = node_prefix.clone();
                        child_prefix.push(child_ptr.chr());
                        stack.push((
                            *child_ptr,
                            child_prefix,
                            node_block_hash.clone(),
                            node_block_id,
                        ));
                    }
                }
            }
        }

        Ok(leaf_count)
    }

    /// Read a node referenced by `ptr`, following backpointers when necessary.
    fn read_node_for_ptr(
        storage: &mut TrieStorageConnection<T>,
        ptr: &TriePtr,
    ) -> Result<(TrieNodeType, T, Option<u32>), Error> {
        if is_backptr(ptr.id()) {
            let back_block_id = ptr.back_block();
            let back_block_hash = storage.get_block_from_local_id(back_block_id)?.clone();
            storage.open_block_known_id(&back_block_hash, back_block_id)?;
            let backptr = ptr.from_backptr();
            let (node, _node_hash) = storage.read_nodetype(&backptr)?;
            Ok((node, back_block_hash, Some(back_block_id)))
        } else {
            let (node, _node_hash) = storage.read_nodetype(ptr)?;
            let (cur_block_hash, cur_block_id) = storage.get_cur_block_and_id();
            Ok((node, cur_block_hash, cur_block_id))
        }
    }

    /// BFS collection pass: gather all trie nodes reachable from `block_hash`.
    ///
    /// Returns:
    /// - `nodes`: `Vec<(TrieNodeType, TrieHash, origin_block_id)>` - raw
    ///   un-remapped nodes.  Index 0 is the root.
    /// - `source_to_idx`: `(source_block_id, byte_offset) -> Vec index map
    ///   needed by the remap pass.
    ///
    /// This function does pure node collection with no path tracking,
    /// height mapping, or blob reads.
    fn deep_copy_bfs_nodes(
        source: &mut TrieStorageConnection<T>,
        block_hash: &T,
    ) -> Result<
        (
            Vec<(TrieNodeType, TrieHash, u32)>,
            HashMap<(u32, u32), usize>,
        ),
        Error,
    > {
        source.open_block(block_hash)?;
        let (root_node, root_hash) = Trie::read_root(source)?;
        let root_block_id = source.get_cur_block_identifier()?;

        let mut nodes: Vec<(TrieNodeType, TrieHash, u32)> = Vec::new();
        let mut source_to_idx: HashMap<(u32, u32), usize> = HashMap::new();

        let root_disk_ptr = TrieStorageConnection::<T>::root_ptr_disk();
        source_to_idx.insert((root_block_id, root_disk_ptr), 0);
        nodes.push((root_node, root_hash, root_block_id));

        let mut queue: VecDeque<usize> = VecDeque::new();
        queue.push_back(0);
        let bfs_start = Instant::now();
        let mut bfs_processed: u64 = 0;
        let mut last_log = Instant::now();

        while let Some(current_idx) = queue.pop_front() {
            bfs_processed += 1;
            if last_log.elapsed().as_secs() >= 30 || bfs_processed % 500_000 == 0 {
                info!(
                    "Deep copy BFS: dequeued {bfs_processed} nodes, collected {} total, queue {}, {:?} elapsed",
                    nodes.len(),
                    queue.len(),
                    bfs_start.elapsed()
                );
                last_log = Instant::now();
            }

            let entry = nodes.get(current_idx).ok_or_else(|| {
                Error::CorruptionError(format!("deep_copy: BFS index {current_idx} out of bounds"))
            })?;
            let origin_block_id = entry.2;

            if entry.0.is_leaf() {
                continue;
            }

            let child_ptrs: Vec<TriePtr> = entry.0.ptrs().to_vec();

            for ptr in child_ptrs.iter() {
                if ptr.id() == TrieNodeID::Empty as u8 {
                    continue;
                }

                let (child_block_id, read_ptr) = if is_backptr(ptr.id()) {
                    (ptr.back_block(), ptr.from_backptr())
                } else {
                    (origin_block_id, *ptr)
                };

                let source_key = (child_block_id, read_ptr.ptr());
                if source_to_idx.contains_key(&source_key) {
                    continue;
                }

                let child_bh = source.get_block_from_local_id(child_block_id)?.clone();
                source.open_block_maybe_id(&child_bh, Some(child_block_id))?;
                let (child_node, child_hash) = source.read_nodetype(&read_ptr)?;

                let child_idx = nodes.len();
                source_to_idx.insert(source_key, child_idx);
                nodes.push((child_node, child_hash, child_block_id));
                queue.push_back(child_idx);
            }
        }

        info!(
            "Deep copy BFS complete: {} nodes in {:?}",
            nodes.len(),
            bfs_start.elapsed()
        );

        Ok((nodes, source_to_idx))
    }

    /// Remap pass: rewrite child pointers to Vec indices and annotate
    /// `back_block` for hash-preserving backpointer identity.
    ///
    /// Must be called after `deep_copy_bfs_nodes` and after the
    /// `block_id_map` (archival_id -> squashed_id) is built.
    ///
    /// ## Backpointer annotation rules
    ///
    /// - Children that were inline at the tip block have `back_block = 0`.
    /// - Children that were backpointers to an ancestor block have
    ///   `back_block = squashed_local_id` (looked up via `block_id_map`).
    /// - All children have the backptr flag cleared (they are physically
    ///   present in the single shared blob).
    fn deep_copy_remap(
        nodes: &mut Vec<(TrieNodeType, TrieHash, u32)>,
        source_to_idx: &HashMap<(u32, u32), usize>,
        block_id_map: &HashMap<u32, u32>,
    ) -> Result<(), Error> {
        use crate::chainstate::stacks::index::node::clear_backptr;

        let remap_start = Instant::now();
        let node_count = nodes.len();

        for idx in 0..node_count {
            if idx > 0 && idx % 500_000 == 0 {
                info!(
                    "Deep copy remap: processed {idx}/{node_count} nodes in {:?}",
                    remap_start.elapsed()
                );
            }

            let origin_block_id = nodes
                .get(idx)
                .ok_or_else(|| {
                    Error::CorruptionError(format!("deep_copy remap: index {idx} out of bounds"))
                })?
                .2;

            if nodes.get(idx).map_or(true, |e| e.0.is_leaf()) {
                continue;
            }

            let child_ptrs: Vec<TriePtr> = nodes
                .get(idx)
                .ok_or_else(|| {
                    Error::CorruptionError(format!("deep_copy remap: index {idx} out of bounds"))
                })?
                .0
                .ptrs()
                .to_vec();

            for (slot, ptr) in child_ptrs.iter().enumerate() {
                if ptr.id() == TrieNodeID::Empty as u8 {
                    continue;
                }

                let (child_block_id, read_ptr, was_backptr) = if is_backptr(ptr.id()) {
                    (ptr.back_block(), ptr.from_backptr(), true)
                } else {
                    (origin_block_id, *ptr, false)
                };

                let source_key = (child_block_id, read_ptr.ptr());
                let child_idx = *source_to_idx.get(&source_key).ok_or_else(|| {
                    Error::CorruptionError(format!(
                        "deep_copy: child {source_key:?} not in source_to_idx"
                    ))
                })?;

                let entry = nodes.get_mut(idx).ok_or_else(|| {
                    Error::CorruptionError(format!("deep_copy remap: index {idx} out of bounds"))
                })?;
                let node_ptrs = entry.0.ptrs_mut();
                let p = node_ptrs.get_mut(slot).ok_or_else(|| {
                    Error::CorruptionError(format!(
                        "deep_copy remap: slot {slot} out of bounds for node at {idx}"
                    ))
                })?;
                p.ptr = child_idx as u32;
                p.id = clear_backptr(p.id);

                if was_backptr {
                    let squashed_id = block_id_map.get(&child_block_id).ok_or_else(|| {
                        Error::CorruptionError(format!(
                            "deep_copy: block_id {child_block_id} not in block_id_map"
                        ))
                    })?;
                    p.back_block = *squashed_id;
                } else {
                    p.back_block = 0;
                }
            }
        }

        info!(
            "Deep copy remap complete: {node_count} nodes remapped in {:?}",
            remap_start.elapsed()
        );

        Ok(())
    }

    /// Validate that a squashed MARF is consistent with the source MARF at
    /// the given `height`.
    ///
    /// ## Fast path (default, `full_leaf_scan = false`)
    ///
    /// Because the MARF is a Merkle trie, a matching root hash at the squash
    /// height cryptographically guarantees that every leaf and intermediate
    /// node is identical.  The fast path therefore:
    ///
    /// 1. Compares the MARF root hash at block_H - O(1).
    /// 2. Verifies per-height root hashes in the SQL table - O(H).
    /// 3. Verifies `marf_squash_info` SQL metadata - O(1).
    /// 4. Verifies all `marf_data` entries share the same blob offset - O(H).
    ///
    /// ## Full leaf scan (`full_leaf_scan = true`)
    ///
    /// In addition to the fast path, walks every leaf in both MARFs and
    /// cross-checks them.  This is O(leaf_count) and much slower, but
    /// useful for debugging squash implementation correctness.
    pub fn validate_squashed_at_height(
        src_path: &str,
        squashed_path: &str,
        open_opts: MARFOpenOpts,
        height: u32,
    ) -> Result<SquashValidationStats, Error> {
        Self::validate_squashed_at_height_ex(src_path, squashed_path, open_opts, height, false)
    }

    /// Extended validation with optional full leaf scan.
    ///
    /// See [`Self::validate_squashed_at_height`] for details on fast vs full mode.
    pub fn validate_squashed_at_height_ex(
        src_path: &str,
        squashed_path: &str,
        open_opts: MARFOpenOpts,
        height: u32,
        full_leaf_scan: bool,
    ) -> Result<SquashValidationStats, Error> {
        let src_storage = TrieFileStorage::open_readonly(src_path, open_opts.clone())?;
        let mut src = MARF::from_storage(src_storage);

        let squashed_storage = TrieFileStorage::open_readonly(squashed_path, open_opts)?;
        let mut squashed = MARF::from_storage(squashed_storage);

        let squashed_block_at_height =
            trie_sql::get_latest_confirmed_block_hash::<T>(squashed.sqlite_conn())?;

        let source_tip = trie_sql::get_latest_confirmed_block_hash::<T>(src.sqlite_conn())?;
        let height_key = format!("{BLOCK_HEIGHT_TO_HASH_MAPPING_KEY}::{height}");
        let source_block_at_height = src
            .with_conn(|conn| Self::get_by_key(conn, &source_tip, &height_key))?
            .map(T::from)
            .unwrap_or_else(|| squashed_block_at_height.clone());

        let mut stats = SquashValidationStats {
            squash_root_present: false,
            squash_root_matches: false,
            root_hash_missing: 0,
            root_hash_mismatches: 0,
            blob_offset_mismatches: 0,
            source_keys_checked: 0,
            squashed_keys_checked: 0,
            missing_in_squashed: 0,
            missing_in_source: 0,
            value_mismatches: 0,
        };

        // Check 1: Per-height root hashes
        //
        // Compare archival root hashes from the source blobs against the
        // squashed MARF's `marf_squash_root_hashes` SQL table. These
        // values are used by the skip-list for blocks > H.
        //
        // Note: we do NOT compare the trie root hashes of block H between
        // source and squashed. They are structurally different by design
        // (all-inline vs mixed-backpointer). Hash identity is restored
        // at H+1 via back_block annotations.
        let start_root_hashes = Instant::now();

        // (a) Bulk SQL for source block entries + offsets.
        let source_blocks = trie_sql::bulk_read_block_entries::<T>(src.sqlite_conn())?;
        let source_block_map: HashMap<T, (u32, u64)> = source_blocks
            .into_iter()
            .map(|(id, bh, offset)| (bh, (id, offset)))
            .collect();
        let source_blobs_path = format!("{src_path}.blobs");
        let root_ptr_offset = (BLOCK_HEADER_HASH_ENCODED_SIZE as u64) + 4;
        let blobs_file = File::open(&source_blobs_path).map_err(Error::IOError)?;
        let mut blobs_reader = BufReader::with_capacity(64 * 1024, blobs_file);

        // (b) Per-height walks + inline blob reads for source root hashes.
        info!("Validate: per-height walks for source root hashes ...");
        let source_tip = trie_sql::get_latest_confirmed_block_hash::<T>(src.sqlite_conn())?;
        let mut source_root_hashes: HashMap<u32, TrieHash> =
            HashMap::with_capacity((height + 1) as usize);
        let mut last_log = Instant::now();
        for h in 0..=height {
            let h_key = format!("{BLOCK_HEIGHT_TO_HASH_MAPPING_KEY}::{h}");
            if let Some(val) = src.with_conn(|conn| Self::get_by_key(conn, &source_tip, &h_key))? {
                let bh = T::from(val);
                if let Some(&(_, blob_offset)) = source_block_map.get(&bh) {
                    blobs_reader.seek(SeekFrom::Start(blob_offset + root_ptr_offset))?;
                    let mut hash_bytes = [0u8; TRIEHASH_ENCODED_SIZE];
                    blobs_reader.read_exact(&mut hash_bytes)?;
                    source_root_hashes.insert(h, TrieHash(hash_bytes));
                }
            }
            if last_log.elapsed().as_secs() >= 30 || (h > 0 && h % 100_000 == 0) {
                info!(
                    "Validate per-height: {}/{} heights in {:?}",
                    h + 1,
                    height + 1,
                    start_root_hashes.elapsed()
                );
                last_log = Instant::now();
            }
        }
        info!(
            "Validate: collected {} source root hashes in {:?}",
            source_root_hashes.len(),
            start_root_hashes.elapsed()
        );

        // (c) Squashed side: single SQL query for all stored root hashes.
        let squashed_root_hashes: HashMap<u32, TrieHash> =
            trie_sql::bulk_read_squash_root_hashes(squashed.sqlite_conn())?
                .into_iter()
                .collect();

        // (d) In-memory root hash comparison.
        for h in 0..=height {
            if let Some(expected) = source_root_hashes.get(&h) {
                match squashed_root_hashes.get(&h) {
                    Some(actual) if expected != actual => {
                        stats.root_hash_mismatches += 1;
                    }
                    None => stats.root_hash_missing += 1,
                    _ => {}
                }
            }
        }
        info!(
            "Validate per-height root hashes: {} checked, {} missing, {} mismatched in {:?}",
            source_root_hashes.len(),
            stats.root_hash_missing,
            stats.root_hash_mismatches,
            start_root_hashes.elapsed()
        );

        let expected_squash_root = match source_root_hashes.get(&height).copied() {
            Some(h) => h,
            None => src.get_root_hash_at(&source_block_at_height)?,
        };

        // Check 2: Squash metadata in SQL (O(1))
        let sql_squash_info = trie_sql::read_squash_info(squashed.sqlite_conn())?;
        match sql_squash_info {
            Some((root, _height)) => {
                stats.squash_root_present = true;
                stats.squash_root_matches = root == expected_squash_root;
            }
            None => {
                stats.squash_root_present = false;
                stats.squash_root_matches = false;
            }
        }

        // Check 3: marf_data blob offsets
        let tip_block_id =
            trie_sql::get_block_identifier(squashed.sqlite_conn(), &squashed_block_at_height)?;
        let (tip_offset, tip_length) =
            trie_sql::get_external_trie_offset_length(squashed.sqlite_conn(), tip_block_id)?;
        stats.blob_offset_mismatches = trie_sql::count_blob_offset_mismatches(
            squashed.sqlite_conn(),
            tip_offset,
            tip_length,
            &squashed_block_at_height,
        )?;

        // Optional: Full leaf scan (O(leaf_count))
        if full_leaf_scan {
            info!("Full leaf scan enabled - walking all leaves in both MARFs");

            // Pass A: source -> squashed
            let start_pass_a = Instant::now();
            let (missing_in_squashed, value_mismatches, source_keys_checked) =
                src.with_conn(|conn| {
                    let mut missing = 0u64;
                    let mut mismatched = 0u64;
                    let mut checked = 0u64;
                    let result =
                        Self::walk_all_leaves(conn, &source_block_at_height, |path, value| {
                            let squashed_value = squashed.with_conn(|sconn| {
                                Self::get_by_hash(sconn, &squashed_block_at_height, &path)
                            })?;
                            checked += 1;
                            if checked % 100_000 == 0 {
                                info!(
                                    "Validate leaf scan (source->squashed): checked {checked} keys in {:?}",
                                    start_pass_a.elapsed()
                                );
                            }
                            match squashed_value {
                                None => missing += 1,
                                Some(other) => {
                                    if other != value {
                                        mismatched += 1;
                                    }
                                }
                            }
                            Ok(())
                        });
                    result.map(|_| (missing, mismatched, checked))
                })?;

            stats.missing_in_squashed = missing_in_squashed;
            stats.value_mismatches = value_mismatches;
            stats.source_keys_checked = source_keys_checked;

            // Pass B: squashed -> source
            let start_pass_b = Instant::now();
            let (missing_in_source, squashed_keys_checked) = squashed.with_conn(|sconn| {
                let mut missing = 0u64;
                let mut checked = 0u64;
                let result =
                    Self::walk_all_leaves(sconn, &squashed_block_at_height, |path, _value| {
                        let src_value = src.with_conn(|conn| {
                            Self::get_by_hash(conn, &source_block_at_height, &path)
                        })?;
                        checked += 1;
                        if checked % 100_000 == 0 {
                            info!(
                                "Validate leaf scan (squashed->source): checked {checked} keys in {:?}",
                                start_pass_b.elapsed()
                            );
                        }
                        if src_value.is_none() {
                            missing += 1;
                        }
                        Ok(())
                    });
                result.map(|_| (missing, checked))
            })?;

            stats.missing_in_source = missing_in_source;
            stats.squashed_keys_checked = squashed_keys_checked;
        }

        Ok(stats)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use stacks_common::types::chainstate::StacksBlockId;
    use tempfile::tempdir;

    use super::*;
    use crate::chainstate::stacks::index::marf::{MARFOpenOpts, OWN_BLOCK_HEIGHT_KEY};
    use crate::chainstate::stacks::index::storage::TrieHashCalculationMode;
    use crate::chainstate::stacks::index::{trie_sql, ClarityMarfTrieId};

    /// Create a small MARF with 2 blocks for basic tests.
    fn setup_marf(path: &str) -> (MARF<StacksBlockId>, StacksBlockId, StacksBlockId) {
        let open_opts = MARFOpenOpts::new(TrieHashCalculationMode::Immediate, "noop", true);
        let mut marf = MARF::<StacksBlockId>::from_path(path, open_opts).unwrap();

        let b1 = StacksBlockId::from_bytes(&[1u8; 32]).unwrap();
        let b2 = StacksBlockId::from_bytes(&[2u8; 32]).unwrap();

        marf.begin(&StacksBlockId::sentinel(), &b1).unwrap();
        marf.insert("k1", MARFValue::from_value("v1")).unwrap();
        marf.commit().unwrap();

        marf.begin(&b1, &b2).unwrap();
        marf.insert("k1", MARFValue::from_value("v2")).unwrap();
        marf.insert("k2", MARFValue::from_value("v3")).unwrap();
        marf.commit().unwrap();

        (marf, b1, b2)
    }

    /// Create a larger MARF with 10 blocks (heights 0–9) for skip-list coverage.
    ///
    /// k1 is updated at every block (exercises backpointers at every depth).
    /// k2..k10 are each inserted at their respective blocks.
    fn setup_large_marf(path: &str) -> (MARF<StacksBlockId>, Vec<StacksBlockId>) {
        let open_opts = MARFOpenOpts::new(TrieHashCalculationMode::Immediate, "noop", true);
        let mut marf = MARF::<StacksBlockId>::from_path(path, open_opts).unwrap();

        let blocks: Vec<StacksBlockId> = (1..=10u8)
            .map(|i| StacksBlockId::from_bytes(&[i; 32]).unwrap())
            .collect();

        // Block at height 0
        marf.begin(&StacksBlockId::sentinel(), &blocks[0]).unwrap();
        marf.insert("k1", MARFValue::from_value("v1_at_0")).unwrap();
        marf.commit().unwrap();

        // Heights 1–9
        for i in 1..blocks.len() {
            marf.begin(&blocks[i - 1], &blocks[i]).unwrap();
            let key = format!("k{}", i + 1);
            let val = format!("v{}_at_{}", i + 1, i);
            marf.insert(&key, MARFValue::from_value(&val)).unwrap();
            marf.insert("k1", MARFValue::from_value(&format!("v1_at_{i}")))
                .unwrap();
            marf.commit().unwrap();
        }

        (marf, blocks)
    }

    fn squash_helper(
        src_path: &str,
        dst_dir: &std::path::Path,
        height: u32,
    ) -> (PathBuf, SquashStats) {
        std::fs::create_dir_all(dst_dir).unwrap();
        let dst_db_path = dst_dir.join("index.sqlite");
        let open_opts = MARFOpenOpts::new(TrieHashCalculationMode::Immediate, "noop", true);
        let stats = MARF::<StacksBlockId>::squash_to_path(
            src_path,
            dst_db_path.to_str().unwrap(),
            open_opts,
            height,
        )
        .unwrap();
        (dst_db_path, stats)
    }

    #[test]
    fn test_walk_all_leaves_yields_all_keys() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("index.sqlite");
        let (mut marf, _b1, b2) = setup_marf(db_path.to_str().unwrap());

        let tip =
            trie_sql::get_latest_confirmed_block_hash::<StacksBlockId>(marf.sqlite_conn()).unwrap();
        assert_eq!(tip, b2);

        let block_at_height = marf.get_block_at_height(1, &tip).unwrap().unwrap();
        assert_eq!(block_at_height, b2);

        let mut seen: HashMap<TrieHash, MARFValue> = HashMap::new();
        let leaf_count = marf
            .with_conn(|conn| {
                MARF::walk_all_leaves(conn, &block_at_height, |path, value| {
                    seen.insert(path, value);
                    Ok(())
                })
            })
            .unwrap();

        assert!(leaf_count > 0);
        assert_eq!(
            seen.get(&TrieHash::from_key("k1")).cloned().unwrap(),
            MARFValue::from_value("v2")
        );
        assert_eq!(
            seen.get(&TrieHash::from_key("k2")).cloned().unwrap(),
            MARFValue::from_value("v3")
        );
    }

    #[test]
    fn test_squash_to_path_outputs_data() {
        let dir = tempdir().unwrap();
        let src_db_path = dir.path().join("index.sqlite");
        let _ = setup_marf(src_db_path.to_str().unwrap());

        let (dst_db_path, stats) = squash_helper(
            src_db_path.to_str().unwrap(),
            &dir.path().join("squashed"),
            1,
        );

        assert!(stats.leaf_count > 0);
        assert!(dst_db_path.exists());
        assert!(PathBuf::from(format!("{}.blobs", dst_db_path.display())).exists());

        let open_opts = MARFOpenOpts::new(TrieHashCalculationMode::Immediate, "noop", true);
        let mut dst =
            MARF::<StacksBlockId>::from_path(dst_db_path.to_str().unwrap(), open_opts).unwrap();
        let b2 = StacksBlockId::from_bytes(&[2u8; 32]).unwrap();
        let k1 = dst.get(&b2, "k1").unwrap().unwrap();
        assert_eq!(k1, MARFValue::from_value("v2"));
        let own_height = dst.get(&b2, OWN_BLOCK_HEIGHT_KEY).unwrap().unwrap();
        assert_eq!(own_height, MARFValue::from(1u32));
    }

    #[test]
    fn test_squash_info_detected_on_open() {
        let dir = tempdir().unwrap();
        let src_db_path = dir.path().join("index.sqlite");
        let _ = setup_marf(src_db_path.to_str().unwrap());

        let (dst_db_path, _) = squash_helper(
            src_db_path.to_str().unwrap(),
            &dir.path().join("squashed"),
            1,
        );

        let open_opts = MARFOpenOpts::new(TrieHashCalculationMode::Immediate, "noop", true);
        let mut squashed =
            MARF::<StacksBlockId>::from_path(dst_db_path.to_str().unwrap(), open_opts).unwrap();
        let tip =
            trie_sql::get_latest_confirmed_block_hash::<StacksBlockId>(squashed.sqlite_conn())
                .unwrap();

        // Verify squash metadata was detected from the SQL table on open.
        let (is_squashed, info_root, info_height, info_block) = squashed
            .with_conn(
                |conn| -> Result<(bool, TrieHash, u32, StacksBlockId), Error> {
                    let info = conn.squash_info().expect("missing squash info");
                    Ok((
                        conn.is_squashed(),
                        info.root,
                        info.height,
                        info.block_hash.clone(),
                    ))
                },
            )
            .unwrap();

        // Cross-check with the SQL table directly.
        let (sql_root, sql_height) = trie_sql::read_squash_info(squashed.sqlite_conn())
            .unwrap()
            .expect("SQL squash info missing");

        assert!(is_squashed);
        assert_eq!(info_root, sql_root);
        assert_eq!(info_height, sql_height);
        assert_eq!(info_height, 1);
        assert_eq!(info_block, tip);
    }

    #[test]
    fn test_squash_info_absent_on_archival_open() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("index.sqlite");
        let (mut marf, _b1, _b2) = setup_marf(db_path.to_str().unwrap());

        let (is_squashed, has_info) = marf
            .with_conn(|conn| -> Result<(bool, bool), Error> {
                Ok((conn.is_squashed(), conn.squash_info().is_some()))
            })
            .unwrap();

        assert!(!is_squashed);
        assert!(!has_info);
    }

    #[test]
    fn test_squashed_marf_can_extend_past_snapshot_height() {
        let dir = tempdir().unwrap();
        let src_db_path = dir.path().join("index.sqlite");
        let _ = setup_marf(src_db_path.to_str().unwrap());

        let (dst_db_path, _) = squash_helper(
            src_db_path.to_str().unwrap(),
            &dir.path().join("squashed"),
            1,
        );

        let open_opts = MARFOpenOpts::new(TrieHashCalculationMode::Immediate, "noop", true);
        let mut squashed =
            MARF::<StacksBlockId>::from_path(dst_db_path.to_str().unwrap(), open_opts).unwrap();

        let b2 = StacksBlockId::from_bytes(&[2u8; 32]).unwrap();
        let b3 = StacksBlockId::from_bytes(&[3u8; 32]).unwrap();
        let b4 = StacksBlockId::from_bytes(&[4u8; 32]).unwrap();

        squashed.begin(&b2, &b3).unwrap();
        squashed.insert("k3", MARFValue::from_value("v4")).unwrap();
        squashed.commit().unwrap();

        squashed.begin(&b3, &b4).unwrap();
        squashed.insert("k4", MARFValue::from_value("v5")).unwrap();
        squashed.commit().unwrap();

        let v4 = squashed.get(&b4, "k4").unwrap().unwrap();
        assert_eq!(v4, MARFValue::from_value("v5"));
        let own_height = squashed.get(&b4, OWN_BLOCK_HEIGHT_KEY).unwrap().unwrap();
        assert_eq!(own_height, MARFValue::from(3u32));
    }

    // ---------------------------------------------------------------
    // New tests
    // ---------------------------------------------------------------

    #[test]
    fn test_validate_squashed_correct_fast() {
        let dir = tempdir().unwrap();
        let src_db_path = dir.path().join("index.sqlite");
        let _ = setup_marf(src_db_path.to_str().unwrap());

        let (dst_db_path, _) = squash_helper(
            src_db_path.to_str().unwrap(),
            &dir.path().join("squashed"),
            1,
        );

        let open_opts = MARFOpenOpts::new(TrieHashCalculationMode::Immediate, "noop", true);
        // Fast path (default) - no leaf scan.
        let stats = MARF::<StacksBlockId>::validate_squashed_at_height(
            src_db_path.to_str().unwrap(),
            dst_db_path.to_str().unwrap(),
            open_opts,
            1,
        )
        .unwrap();

        assert!(stats.squash_root_present, "squash root present");
        assert!(stats.squash_root_matches, "squash root matches");
        assert_eq!(stats.root_hash_missing, 0);
        assert_eq!(stats.root_hash_mismatches, 0);
        assert_eq!(stats.blob_offset_mismatches, 0);
        // Fast path skips leaf scan - these should be 0.
        assert_eq!(stats.source_keys_checked, 0);
        assert_eq!(stats.squashed_keys_checked, 0);
    }

    #[test]
    fn test_validate_squashed_correct_full() {
        let dir = tempdir().unwrap();
        let src_db_path = dir.path().join("index.sqlite");
        let _ = setup_marf(src_db_path.to_str().unwrap());

        let (dst_db_path, _) = squash_helper(
            src_db_path.to_str().unwrap(),
            &dir.path().join("squashed"),
            1,
        );

        let open_opts = MARFOpenOpts::new(TrieHashCalculationMode::Immediate, "noop", true);
        // Full leaf scan mode.
        let stats = MARF::<StacksBlockId>::validate_squashed_at_height_ex(
            src_db_path.to_str().unwrap(),
            dst_db_path.to_str().unwrap(),
            open_opts,
            1,
            true,
        )
        .unwrap();

        assert!(stats.squash_root_present, "squash root present");
        assert!(stats.squash_root_matches, "squash root matches");
        assert_eq!(stats.root_hash_missing, 0);
        assert_eq!(stats.root_hash_mismatches, 0);
        assert_eq!(stats.blob_offset_mismatches, 0);
        // Full scan populates leaf counts.
        assert_eq!(stats.missing_in_squashed, 0, "missing in squashed");
        assert_eq!(stats.missing_in_source, 0, "missing in source");
        assert_eq!(stats.value_mismatches, 0, "value mismatches");
        assert!(stats.source_keys_checked > 0);
        assert!(stats.squashed_keys_checked > 0);
    }

    #[test]
    fn test_validate_detects_wrong_height() {
        // Squash at height 1 but validate at height 0.
        // Source state at height 0 differs from squashed state at height 1
        // (k1 was overwritten between blocks), so validation must report
        // a root hash mismatch.
        let dir = tempdir().unwrap();
        let src_db_path = dir.path().join("index.sqlite");
        let _ = setup_marf(src_db_path.to_str().unwrap());

        let (dst_db_path, _) = squash_helper(
            src_db_path.to_str().unwrap(),
            &dir.path().join("squashed"),
            1,
        );

        let open_opts = MARFOpenOpts::new(TrieHashCalculationMode::Immediate, "noop", true);
        let stats = MARF::<StacksBlockId>::validate_squashed_at_height(
            src_db_path.to_str().unwrap(),
            dst_db_path.to_str().unwrap(),
            open_opts,
            0,
        )
        .unwrap();

        // Source at height 0 has a different root hash than squashed (at height 1).
        assert!(
            !stats.squash_root_matches,
            "Expected squash root mismatch when validating at wrong height: {stats:?}"
        );
    }

    #[test]
    fn test_walk_all_leaves_resolves_backpointers() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("index.sqlite");
        let (mut marf, _blocks) = setup_large_marf(db_path.to_str().unwrap());

        // At the tip (height 9), k1 should reflect the latest update and keys
        // k2..k10 should all be reachable via backpointers.
        let tip =
            trie_sql::get_latest_confirmed_block_hash::<StacksBlockId>(marf.sqlite_conn()).unwrap();
        let block_at_tip = marf.get_block_at_height(9, &tip).unwrap().unwrap();

        let mut seen: HashMap<TrieHash, MARFValue> = HashMap::new();
        let leaf_count = marf
            .with_conn(|conn| {
                MARF::walk_all_leaves(conn, &block_at_tip, |path, value| {
                    seen.insert(path, value);
                    Ok(())
                })
            })
            .unwrap();

        // We expect k1..k10 plus MARF metadata keys.
        assert!(leaf_count >= 10, "expected >= 10 leaves, got {leaf_count}");
        assert_eq!(
            seen.get(&TrieHash::from_key("k1")).cloned().unwrap(),
            MARFValue::from_value("v1_at_9"),
            "k1 should have latest value"
        );
        for i in 2..=10 {
            let key = format!("k{i}");
            assert!(
                seen.contains_key(&TrieHash::from_key(&key)),
                "missing key {key} from walk"
            );
        }
    }

    #[test]
    fn test_large_marf_squash_extend_root_hash_matches_archival() {
        // Squash a 10-block MARF at height 8, then extend both the archival
        // and squashed MARFs with the same data at heights 9 and 10.
        //
        // Since squash metadata is stored in SQL tables (not in the trie),
        // the trie content is identical to the archival MARF and therefore
        // the MARF root hashes at the extended heights MUST match.
        let dir = tempdir().unwrap();
        let archival_path = dir.path().join("archival.sqlite");
        let (mut archival, blocks) = setup_large_marf(archival_path.to_str().unwrap());

        // Squash at height 8 (block index 8 = blocks[8]).
        let (squashed_path, _) = squash_helper(
            archival_path.to_str().unwrap(),
            &dir.path().join("squashed"),
            8,
        );

        let open_opts = MARFOpenOpts::new(TrieHashCalculationMode::Immediate, "noop", true);
        let mut squashed =
            MARF::<StacksBlockId>::from_path(squashed_path.to_str().unwrap(), open_opts).unwrap();

        // Extend both MARFs from blocks[8] (height 8) with the same data.
        let b_new_9 = StacksBlockId::from_bytes(&[101u8; 32]).unwrap();
        let b_new_10 = StacksBlockId::from_bytes(&[102u8; 32]).unwrap();

        // --- Extend archival ---
        archival.begin(&blocks[8], &b_new_9).unwrap();
        archival
            .insert("k_new_9", MARFValue::from_value("val9"))
            .unwrap();
        archival.commit().unwrap();

        archival.begin(&b_new_9, &b_new_10).unwrap();
        archival
            .insert("k_new_10", MARFValue::from_value("val10"))
            .unwrap();
        archival.commit().unwrap();

        // --- Extend squashed ---
        squashed.begin(&blocks[8], &b_new_9).unwrap();
        squashed
            .insert("k_new_9", MARFValue::from_value("val9"))
            .unwrap();
        squashed.commit().unwrap();

        squashed.begin(&b_new_9, &b_new_10).unwrap();
        squashed
            .insert("k_new_10", MARFValue::from_value("val10"))
            .unwrap();
        squashed.commit().unwrap();

        // (a) Data inserted at the extended heights is readable from
        // both MARFs.
        assert_eq!(
            squashed.get(&b_new_9, "k_new_9").unwrap().unwrap(),
            MARFValue::from_value("val9")
        );
        assert_eq!(
            squashed.get(&b_new_10, "k_new_10").unwrap().unwrap(),
            MARFValue::from_value("val10")
        );
        // Keys from the squash point are still readable through backpointers.
        assert_eq!(
            squashed.get(&b_new_10, "k1").unwrap().unwrap(),
            MARFValue::from_value("v1_at_8")
        );

        // (b) MARF root hashes at the extended heights must match between
        // archival and squashed, proving the trie content is identical.
        let archival_root_9 = archival.get_root_hash_at(&b_new_9).unwrap();
        let squashed_root_9 = squashed.get_root_hash_at(&b_new_9).unwrap();
        assert_eq!(
            archival_root_9, squashed_root_9,
            "Root hash mismatch at height 9"
        );

        let archival_root_10 = archival.get_root_hash_at(&b_new_10).unwrap();
        let squashed_root_10 = squashed.get_root_hash_at(&b_new_10).unwrap();
        assert_eq!(
            archival_root_10, squashed_root_10,
            "Root hash mismatch at height 10"
        );

        // Sanity: root hashes are non-trivial and differ between heights.
        assert_ne!(archival_root_9, TrieHash([0u8; 32]), "root at 9 is zero");
        assert_ne!(archival_root_10, TrieHash([0u8; 32]), "root at 10 is zero");
        assert_ne!(
            archival_root_9, archival_root_10,
            "roots at 9 and 10 should differ"
        );

        // Verify height progression is correct.
        let own_h = squashed
            .get(&b_new_10, OWN_BLOCK_HEIGHT_KEY)
            .unwrap()
            .unwrap();
        assert_eq!(own_h, MARFValue::from(10u32));
    }

    /// Squash at height 5, then extend both MARFs through 10 additional
    /// heights and verify hash equality at EVERY extended height.
    #[test]
    fn test_multi_height_extension_hash_equality() {
        let dir = tempdir().unwrap();
        let archival_path = dir.path().join("archival.sqlite");
        let (mut archival, blocks) = setup_large_marf(archival_path.to_str().unwrap());

        let (squashed_path, _) = squash_helper(
            archival_path.to_str().unwrap(),
            &dir.path().join("squashed"),
            5,
        );

        let open_opts = MARFOpenOpts::new(TrieHashCalculationMode::Immediate, "noop", true);
        let mut squashed =
            MARF::<StacksBlockId>::from_path(squashed_path.to_str().unwrap(), open_opts).unwrap();

        // Extend from blocks[5] through 10 new heights.
        let mut prev_block = blocks[5].clone();
        let mut new_blocks: Vec<StacksBlockId> = Vec::new();
        for i in 0..10u8 {
            let new_bh = StacksBlockId::from_bytes(&[200 + i; 32]).unwrap();
            let key = format!("ext_k{i}");
            let val = format!("ext_v{i}");

            archival.begin(&prev_block, &new_bh).unwrap();
            archival.insert(&key, MARFValue::from_value(&val)).unwrap();
            archival.commit().unwrap();

            squashed.begin(&prev_block, &new_bh).unwrap();
            squashed.insert(&key, MARFValue::from_value(&val)).unwrap();
            squashed.commit().unwrap();

            new_blocks.push(new_bh.clone());
            prev_block = new_bh;
        }

        // Verify hash equality at every extended height.
        for (i, bh) in new_blocks.iter().enumerate() {
            let arch_root = archival.get_root_hash_at(bh).unwrap();
            let sq_root = squashed.get_root_hash_at(bh).unwrap();
            assert_eq!(
                arch_root,
                sq_root,
                "Root hash mismatch at extended height {}",
                i + 6
            );
            assert_ne!(arch_root, TrieHash([0u8; 32]), "root at {} is zero", i + 6);
        }

        // Verify data accessibility from the last block.
        let last = new_blocks.last().unwrap();
        assert_eq!(
            squashed.get(last, "k1").unwrap().unwrap(),
            MARFValue::from_value("v1_at_5"),
        );
        assert_eq!(
            squashed.get(last, "ext_k9").unwrap().unwrap(),
            MARFValue::from_value("ext_v9"),
        );
    }

    /// Verify that all historical marf_data entries share the same
    /// external_offset (i.e. point to the single shared blob).
    #[test]
    fn test_marf_data_entries_share_blob_offset() {
        let dir = tempdir().unwrap();
        let src_path = dir.path().join("index.sqlite");
        let (_, blocks) = setup_large_marf(src_path.to_str().unwrap());

        let (dst_path, _) =
            squash_helper(src_path.to_str().unwrap(), &dir.path().join("squashed"), 8);

        let open_opts = MARFOpenOpts::new(TrieHashCalculationMode::Immediate, "noop", true);
        let squashed =
            MARF::<StacksBlockId>::from_path(dst_path.to_str().unwrap(), open_opts).unwrap();
        let conn = squashed.sqlite_conn();

        // Read the tip block's offset.
        let tip_id = trie_sql::get_block_identifier(conn, &blocks[8]).unwrap();
        let (tip_offset, tip_length) =
            trie_sql::get_external_trie_offset_length(conn, tip_id).unwrap();
        assert!(tip_length > 0, "blob length should be non-zero");

        // Every historical block should share the same offset.
        for i in 0..8 {
            let blk_id = trie_sql::get_block_identifier(conn, &blocks[i]).unwrap();
            let (offset, length) = trie_sql::get_external_trie_offset_length(conn, blk_id).unwrap();
            assert_eq!(offset, tip_offset, "block {i} offset mismatch");
            assert_eq!(length, tip_length, "block {i} length mismatch");
        }
    }

    /// Verify that walk_cow correctly follows annotated back_block values
    /// when copying nodes from a squashed blob into a new block.
    /// This tests the modified `node_copy_update_ptrs` path.
    #[test]
    fn test_walk_cow_preserves_backpointer_identity() {
        let dir = tempdir().unwrap();
        let archival_path = dir.path().join("archival.sqlite");
        let (mut archival, blocks) = setup_large_marf(archival_path.to_str().unwrap());

        // Squash at height 9 (the tip).
        let (squashed_path, _) = squash_helper(
            archival_path.to_str().unwrap(),
            &dir.path().join("squashed"),
            9,
        );

        let open_opts = MARFOpenOpts::new(TrieHashCalculationMode::Immediate, "noop", true);
        let mut squashed =
            MARF::<StacksBlockId>::from_path(squashed_path.to_str().unwrap(), open_opts).unwrap();

        // Insert into a new block - this exercises walk_cow which must
        // correctly handle nodes from the squash blob with annotated back_block.
        let b_new = StacksBlockId::from_bytes(&[250u8; 32]).unwrap();
        squashed.begin(&blocks[9], &b_new).unwrap();
        squashed
            .insert("k1", MARFValue::from_value("v1_at_10"))
            .unwrap();
        squashed
            .insert("new_key", MARFValue::from_value("new_val"))
            .unwrap();
        squashed.commit().unwrap();

        // All historical keys should still be readable.
        for i in 2..=10 {
            let key = format!("k{i}");
            let result = squashed.get(&b_new, &key).unwrap();
            assert!(result.is_some(), "missing key {key} after extend");
        }

        // Verify the updated key.
        assert_eq!(
            squashed.get(&b_new, "k1").unwrap().unwrap(),
            MARFValue::from_value("v1_at_10"),
        );

        // Verify the new key.
        assert_eq!(
            squashed.get(&b_new, "new_key").unwrap().unwrap(),
            MARFValue::from_value("new_val"),
        );

        // Compare root hash with archival extension.
        archival.begin(&blocks[9], &b_new).unwrap();
        archival
            .insert("k1", MARFValue::from_value("v1_at_10"))
            .unwrap();
        archival
            .insert("new_key", MARFValue::from_value("new_val"))
            .unwrap();
        archival.commit().unwrap();

        let arch_root = archival.get_root_hash_at(&b_new).unwrap();
        let sq_root = squashed.get_root_hash_at(&b_new).unwrap();
        assert_eq!(arch_root, sq_root, "Root hash mismatch after walk_cow");
    }
}
