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
//! A squashed MARF contains only the canonical state at a given
//! height H plus the metadata needed for ancestor hash lookups and
//! block-height resolution.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{BufReader, Read as _, Seek, SeekFrom};
use std::time::Instant;

use rusqlite::params;
use stacks_common::types::chainstate::{
    StacksBlockId, TrieHash, BLOCK_HEADER_HASH_ENCODED_SIZE, TRIEHASH_ENCODED_SIZE,
};

use crate::chainstate::stacks::index::bits::get_leaf_hash;
use crate::chainstate::stacks::index::marf::{
    MARFOpenOpts, MarfConnection, BLOCK_HEIGHT_TO_HASH_MAPPING_KEY, MARF,
};
use crate::chainstate::stacks::index::node::{is_backptr, TrieNodeID, TrieNodeType, TriePtr};
use crate::chainstate::stacks::index::storage::{
    SquashInfo, TrieFileStorage, TrieStorageConnection,
};
use crate::chainstate::stacks::index::trie::Trie;
use crate::chainstate::stacks::index::{trie_sql, BlockMap, Error, MarfTrieId, TrieHasher};

/// A collected trie node: `(node, hash, origin_block_id)`.
type CollectedNode = (TrieNodeType, TrieHash, u32);

/// Per-height block metadata: `(height, block_hash, root_hash)`.
type BlockInfo<T> = (u32, T, TrieHash);

/// A `BlockMap` adapter for trie nodes that have no backpointer children.
///
/// After the remap pass all pointers in the squash blob are inline.
/// `write_consensus_bytes` writes zeroed block hashes for non-backptr
/// children and never queries the `BlockMap`, so every method here is
/// unreachable.
struct InlineOnlyBlockMap;

impl BlockMap for InlineOnlyBlockMap {
    type TrieId = StacksBlockId;

    fn get_block_hash(&self, _id: u32) -> Result<Self::TrieId, Error> {
        unreachable!("InlineOnlyBlockMap: no backpointers in squash trie")
    }
    fn get_block_hash_caching(&mut self, _id: u32) -> Result<&Self::TrieId, Error> {
        unreachable!("InlineOnlyBlockMap: no backpointers in squash trie")
    }
    fn is_block_hash_cached(&self, _id: u32) -> bool {
        false
    }
    fn get_block_id(&self, _bhh: &Self::TrieId) -> Result<u32, Error> {
        unreachable!("InlineOnlyBlockMap: no backpointers in squash trie")
    }
    fn get_block_id_caching(&mut self, _bhh: &Self::TrieId) -> Result<u32, Error> {
        unreachable!("InlineOnlyBlockMap: no backpointers in squash trie")
    }
}

/// Compute the content hash of a `TrieNodeType` given pre-collected child hashes.
///
/// Equivalent to `bits::get_node_hash` but works on the `TrieNodeType` enum
/// directly (which does not implement `ConsensusSerializable<M>`).
fn compute_node_hash(node: &TrieNodeType, child_hashes: &[TrieHash]) -> TrieHash {
    use sha2::Digest;
    let mut hasher = TrieHasher::new();
    node.write_consensus_bytes(&mut InlineOnlyBlockMap, &mut hasher)
        .expect("IO failure pushing to hasher");
    for h in child_hashes {
        hasher.update(h.as_ref());
    }
    TrieHash(hasher.finalize().into())
}

/// Recompute all node hashes in `nodes` as **pure content hashes**.
///
/// The BFS collection pass stores archival hashes which may include skip-list
/// information (for nodes that were trie roots in their original blocks).
/// For proof consistency the blob must store deterministic content hashes —
/// `H(consensus_bytes(node) || children_content_hashes)` — matching what the
/// proof verifier computes bottom-up.
///
/// Processes in reverse BFS order (children before parents).
fn recompute_content_hashes(nodes: &mut [CollectedNode]) -> Result<(), Error> {
    let empty_hash = TrieHash::from_data(&[]);

    // Leaf hashes depend only on their own data; compute them first.
    for (node, hash, _) in nodes.iter_mut() {
        if let TrieNodeType::Leaf(ref leaf) = node {
            *hash = get_leaf_hash(leaf);
        }
    }

    // Internal nodes in reverse order so every child hash is already final.
    for idx in (0..nodes.len()).rev() {
        let entry = nodes.get(idx).ok_or_else(|| {
            Error::CorruptionError(format!(
                "recompute_content_hashes: index {idx} out of bounds"
            ))
        })?;
        if entry.0.is_leaf() {
            continue;
        }

        let child_hashes = collect_child_hashes(nodes, idx, empty_hash)?;
        let node_ref = &nodes.get(idx).expect("bounds already checked").0;
        let new_hash = compute_node_hash(node_ref, &child_hashes);
        nodes.get_mut(idx).expect("bounds already checked").1 = new_hash;
    }

    Ok(())
}

/// Gather the content hashes of every child of `nodes[idx]`.
///
/// Empty slots and backpointer children (which reference state outside the
/// blob) contribute `empty_hash`.  Inline children contribute their already-
/// computed hash from the `nodes` array.
fn collect_child_hashes(
    nodes: &[CollectedNode],
    idx: usize,
    empty_hash: TrieHash,
) -> Result<Vec<TrieHash>, Error> {
    let entry = nodes.get(idx).ok_or_else(|| {
        Error::CorruptionError(format!("collect_child_hashes: index {idx} out of bounds"))
    })?;
    let ptrs = entry.0.ptrs();
    let mut out = Vec::with_capacity(ptrs.len());
    for child_ptr in ptrs {
        if child_ptr.id() == TrieNodeID::Empty as u8 || is_backptr(child_ptr.id()) {
            out.push(empty_hash);
        } else {
            let child_idx = child_ptr.ptr() as usize;
            let (_, child_hash, _) = nodes.get(child_idx).ok_or_else(|| {
                Error::CorruptionError(format!(
                    "Invalid child index {child_idx} while recomputing squash hashes"
                ))
            })?;
            out.push(*child_hash);
        }
    }
    Ok(out)
}

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
/// - Per-height root hashes stored in `marf_squash_archival_marf_roots` match the
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
    pub archival_root_present: bool,
    /// Whether the stored archival root hash at the squash height
    /// matches the source MARF's root hash at that height.
    pub archival_root_matches: bool,
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

/// Step 1: Build an in-memory block_map from all `marf_data` entries.
fn collect_block_map<T: MarfTrieId>(src: &MARF<T>) -> Result<HashMap<T, (u32, u64)>, Error> {
    let all_blocks = trie_sql::bulk_read_block_entries::<T>(src.sqlite_conn())?;
    Ok(all_blocks
        .into_iter()
        .map(|(id, bh, offset)| (bh, (id, offset)))
        .collect())
}

/// Step 2: For each height 0..=H, resolve (block_hash, root_hash) via trie
/// walk + direct blob seek.  Returns the per-height info plus the archival
/// block_id mapping needed for placeholder insertion.
fn collect_per_height_metadata<T: MarfTrieId>(
    src: &mut MARF<T>,
    source_tip: &T,
    block_map: &HashMap<T, (u32, u64)>,
    blobs_path: &str,
    height: u32,
) -> Result<(Vec<BlockInfo<T>>, HashMap<T, u32>), Error> {
    let root_ptr_offset = (BLOCK_HEADER_HASH_ENCODED_SIZE as u64) + 4;
    let blobs_file = File::open(blobs_path).map_err(Error::IOError)?;
    let mut blobs_reader = BufReader::with_capacity(64 * 1024, blobs_file);

    let mut block_info: Vec<BlockInfo<T>> = Vec::with_capacity((height + 1) as usize);
    let mut archival_ids: HashMap<T, u32> = HashMap::with_capacity((height + 1) as usize);
    let mut last_log = Instant::now();
    let start = Instant::now();

    for h in 0..=height {
        let h_key = format!("{BLOCK_HEIGHT_TO_HASH_MAPPING_KEY}::{h}");
        let val = src
            .with_conn(|conn| MARF::<T>::get_by_key(conn, source_tip, &h_key))?
            .ok_or_else(|| {
                Error::CorruptionError(format!("Missing height mapping for height {h}"))
            })?;
        let bh = T::from(val);

        let &(block_id, blob_offset) = block_map.get(&bh).ok_or_else(|| {
            Error::CorruptionError(format!(
                "Missing block map entry for block hash at height {h}"
            ))
        })?;

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
                start.elapsed()
            );
            last_log = Instant::now();
        }
    }
    info!(
        "Squash step 2 (per-height metadata): {} heights in {:?}",
        height + 1,
        start.elapsed()
    );

    Ok((block_info, archival_ids))
}

/// Step 4: Bulk-insert `marf_data` placeholder rows for blocks 0..H-1.
///
/// Returns a mapping from archival block_id to squashed block_id.
fn insert_placeholder_blocks<T: MarfTrieId>(
    conn: &rusqlite::Connection,
    block_info: &[BlockInfo<T>],
    block_at_height: &T,
    archival_ids: &HashMap<T, u32>,
) -> Result<HashMap<u32, u32>, Error> {
    let start = Instant::now();
    let mut archival_to_squashed: HashMap<u32, u32> = HashMap::new();
    let mut stmt = conn.prepare(
        "INSERT INTO marf_data (block_hash, data, unconfirmed, external_offset, external_length) \
         VALUES (?1, ?2, 0, ?3, ?4)",
    )?;
    let empty_blob: &[u8] = &[];
    for (h, bh, _) in block_info {
        if bh == block_at_height {
            continue;
        }
        let archival_id = *archival_ids.get(bh).ok_or(Error::NotFoundError)?;
        let squashed_id: u32 = stmt
            .insert(params![bh.to_string(), empty_blob, 0i64, 0i64])?
            .try_into()
            .expect("block_id overflow");
        archival_to_squashed.insert(archival_id, squashed_id);
        if *h % 100_000 == 0 && *h > 0 {
            info!(
                "Squash placeholders: inserted {h} entries in {:?}",
                start.elapsed()
            );
        }
    }
    info!(
        "Squash step 4 (placeholders): {} entries in {:?}",
        archival_to_squashed.len(),
        start.elapsed()
    );
    Ok(archival_to_squashed)
}

/// Step 6: Write all squash SQL metadata in one transaction scope.
fn persist_squash_metadata<T: MarfTrieId>(
    conn: &rusqlite::Connection,
    block_info: &[BlockInfo<T>],
    source_root_hash: &TrieHash,
    height: u32,
) -> Result<(), Error> {
    let start = Instant::now();
    trie_sql::write_squash_info(conn, source_root_hash, height)?;
    let mut stmt = conn.prepare(
        "INSERT OR REPLACE INTO marf_squash_archival_marf_roots (height, marf_root_hash) VALUES (?1, ?2)",
    )?;
    let mut stmt_bh = conn.prepare(
        "INSERT OR REPLACE INTO marf_squash_block_heights (block_hash, height) VALUES (?1, ?2)",
    )?;
    for (h, bh, rh) in block_info {
        stmt.execute(params![*h as i64, rh.as_bytes().to_vec()])?;
        stmt_bh.execute(params![bh.to_string(), *h as i64])?;
    }
    info!(
        "Squash step 6 (SQL metadata): {} root hashes + block heights in {:?}",
        block_info.len(),
        start.elapsed()
    );
    Ok(())
}

/// Post-commit: persist `squash_root_node_hash` and share blob offsets.
fn finalize_shared_blob_offsets<T: MarfTrieId>(
    dst: &mut MARF<T>,
    block_at_height: &T,
    squash_root_node_hash: &TrieHash,
) -> Result<u64, Error> {
    // Persist squash_root_node_hash to SQL.
    {
        let conn = dst.sqlite_conn();
        conn.execute_batch("BEGIN IMMEDIATE")
            .map_err(|e| Error::CorruptionError(format!("BEGIN squash_root_node_hash: {e}")))?;
        trie_sql::update_squash_root_node_hash(conn, squash_root_node_hash)?;
        conn.execute_batch("COMMIT")
            .map_err(|e| Error::CorruptionError(format!("COMMIT squash_root_node_hash: {e}")))?;
    }

    // Bulk-update placeholders to share the tip block's blob offset.
    let start = Instant::now();
    let conn = dst.sqlite_conn();
    let bh_id = trie_sql::get_block_identifier(conn, block_at_height)?;
    let (offset, length) = trie_sql::get_external_trie_offset_length(conn, bh_id)?;

    conn.execute_batch("BEGIN IMMEDIATE")
        .map_err(|e| Error::CorruptionError(format!("BEGIN: {e}")))?;
    let updated = trie_sql::bulk_update_blob_offsets(conn, offset, length, block_at_height)?;
    conn.execute_batch("COMMIT")
        .map_err(|e| Error::CorruptionError(format!("COMMIT: {e}")))?;
    info!(
        "Squash step 8 (blob update): {updated} rows in {:?}",
        start.elapsed()
    );
    Ok(updated)
}

impl<T: MarfTrieId> MARF<T> {
    /// Squash the MARF at `height` into a new database at `dst_path`.
    ///
    /// Produces a hash-preserving squash: the squashed MARF contains a single
    /// shared trie storage with all trie nodes reachable at `height`. Each historical
    /// block (0..=height) has a `marf_data` row pointing at this shared trie storage so
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
    pub fn squash_to_path(
        src_path: &str,
        dst_path: &str,
        open_opts: MARFOpenOpts,
        height: u32,
    ) -> Result<SquashStats, Error> {
        let overall_start = Instant::now();

        // Step 1: bulk SQL block map
        let src_storage = TrieFileStorage::open_readonly(src_path, open_opts.clone())?;
        let mut src = MARF::from_storage(src_storage);

        let tip = trie_sql::get_latest_confirmed_block_hash::<T>(src.sqlite_conn())?;
        let block_at_height = src
            .get_block_at_height(height, &tip)?
            .ok_or(Error::NotFoundError)?;

        let start = Instant::now();
        let block_map = collect_block_map(&src)?;
        info!(
            "Squash step 1 (bulk SQL): {} entries in {:?}",
            block_map.len(),
            start.elapsed()
        );

        // Step 2: per-height metadata
        let blobs_path = format!("{src_path}.blobs");
        let (block_info, archival_ids) =
            collect_per_height_metadata(&mut src, &tip, &block_map, &blobs_path, height)?;

        // Step 3: BFS deep copy
        let start = Instant::now();
        let (mut nodes, source_to_idx) =
            src.with_conn(|conn| MARF::<T>::collect_all_nodes(conn, &block_at_height))?;
        let node_count = nodes.len() as u64;
        info!(
            "Squash step 3 (BFS): {node_count} nodes in {:?}",
            start.elapsed()
        );

        // Open destination MARF and begin transaction
        let mut dst = MARF::from_path(dst_path, open_opts)?;
        let mut tx = dst.begin_tx()?;
        tx.begin(&T::sentinel(), &block_at_height)?;

        // Step 4: placeholder marf_data rows
        let archival_to_squashed = insert_placeholder_blocks(
            tx.sqlite_tx(),
            &block_info,
            &block_at_height,
            &archival_ids,
        )?;

        // Step 5a: remap pointers (in-memory)
        let start = Instant::now();
        Self::deep_copy_remap(&mut nodes, &source_to_idx, &archival_to_squashed)?;
        info!(
            "Squash step 5a (remap): {node_count} nodes in {:?}",
            start.elapsed()
        );

        // Step 5b: recompute content hashes
        let start = Instant::now();
        recompute_content_hashes(&mut nodes)?;
        info!(
            "Squash step 5b (content hashes): {node_count} nodes in {:?}",
            start.elapsed()
        );

        // Write nodes to TrieRAM
        for (idx, (node, hash, _)) in nodes.iter().enumerate() {
            tx.write_node_direct(idx as u32, node, *hash)?;
        }

        // Step 6: SQL metadata
        let source_root_hash = block_info
            .iter()
            .find(|(_, bh, _)| bh == &block_at_height)
            .map(|(_, _, rh)| *rh)
            .ok_or(Error::NotFoundError)?;
        persist_squash_metadata(tx.sqlite_tx(), &block_info, &source_root_hash, height)?;

        let squash_root_node_hash = nodes
            .first()
            .map(|(_, hash, _)| *hash)
            .ok_or_else(|| Error::CorruptionError("No nodes in squash trie".to_string()))?;
        info!("Squash blob content root hash: {squash_root_node_hash}");

        tx.set_squash_info(Some(SquashInfo {
            archival_marf_root_hash: source_root_hash,
            squash_root_node_hash,
            height,
            block_hash: block_at_height.clone(),
        }));

        // Step 7: commit
        let start = Instant::now();
        info!("Squash commit: starting commit");
        tx.commit()?;
        info!("Squash commit: finished in {:?}", start.elapsed());

        // Step 8: post-commit finalization
        finalize_shared_blob_offsets(&mut dst, &block_at_height, &squash_root_node_hash)?;

        info!(
            "Squash complete: {node_count} nodes, total time {:?}",
            overall_start.elapsed()
        );

        Ok(SquashStats {
            leaf_count: node_count,
        })
    }

    /// BFS collection pass: gather all trie nodes reachable from `block_hash`.
    ///
    /// Returns:
    /// - `nodes`: raw un-remapped nodes.  Index 0 is the root.
    /// - `source_to_idx`: `(source_block_id, byte_offset) -> Vec index` map
    ///   needed by the remap pass.
    fn collect_all_nodes(
        source: &mut TrieStorageConnection<T>,
        block_hash: &T,
    ) -> Result<(Vec<CollectedNode>, HashMap<(u32, u32), usize>), Error> {
        source.open_block(block_hash)?;
        let (root_node, root_hash) = Trie::read_root(source)?;
        let root_block_id = source.get_cur_block_identifier()?;

        let mut nodes: Vec<CollectedNode> = Vec::new();
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
    /// Must be called after `collect_all_nodes` and after the
    /// `block_id_map` (archival_id -> squashed_id) is built.
    ///
    /// ## Backpointer annotation rules
    ///
    /// - Children that were inline at the tip block have `back_block = 0`.
    /// - Children that were backpointers to an ancestor block have
    ///   `back_block = squashed_local_id` (looked up via `block_id_map`).
    /// - All children have the backptr flag cleared (they are physically
    ///   present in the single shared trie storage).
    fn deep_copy_remap(
        nodes: &mut [CollectedNode],
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

            let entry = nodes.get(idx).ok_or_else(|| {
                Error::CorruptionError(format!("deep_copy remap: index {idx} out of bounds"))
            })?;
            let origin_block_id = entry.2;

            if entry.0.is_leaf() {
                continue;
            }

            let child_ptrs: Vec<TriePtr> = entry.0.ptrs().to_vec();

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

                debug_assert!(
                    child_idx < node_count,
                    "remap: child_idx {child_idx} >= node_count {node_count}"
                );

                let p = nodes
                    .get_mut(idx)
                    .ok_or_else(|| {
                        Error::CorruptionError(format!(
                            "deep_copy remap: index {idx} out of bounds"
                        ))
                    })?
                    .0
                    .ptrs_mut()
                    .get_mut(slot)
                    .ok_or_else(|| {
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
            archival_root_present: false,
            archival_root_matches: false,
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
        let start_root_hashes = Instant::now();

        let source_blocks = trie_sql::bulk_read_block_entries::<T>(src.sqlite_conn())?;
        let source_block_map: HashMap<T, (u32, u64)> = source_blocks
            .into_iter()
            .map(|(id, bh, offset)| (bh, (id, offset)))
            .collect();
        let source_blobs_path = format!("{src_path}.blobs");
        let root_ptr_offset = (BLOCK_HEADER_HASH_ENCODED_SIZE as u64) + 4;
        let blobs_file = File::open(&source_blobs_path).map_err(Error::IOError)?;
        let mut blobs_reader = BufReader::with_capacity(64 * 1024, blobs_file);

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

        let squashed_root_hashes: HashMap<u32, TrieHash> =
            trie_sql::bulk_read_squash_archival_marf_root_hashes(squashed.sqlite_conn())?
                .into_iter()
                .collect();

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
            Some((archival_marf_root_hash, _squash_root_node_hash, _height)) => {
                stats.archival_root_present = true;
                stats.archival_root_matches = archival_marf_root_hash == expected_squash_root;
            }
            None => {
                stats.archival_root_present = false;
                stats.archival_root_matches = false;
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

            let start_pass_a = Instant::now();
            let (missing_in_squashed, value_mismatches, source_keys_checked) =
                src.with_conn(|conn| {
                    let mut missing = 0u64;
                    let mut mismatched = 0u64;
                    let mut checked = 0u64;
                    let result =
                        Self::for_each_leaf(conn, &source_block_at_height, |path, value| {
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

            let start_pass_b = Instant::now();
            let (missing_in_source, squashed_keys_checked) = squashed.with_conn(|sconn| {
                let mut missing = 0u64;
                let mut checked = 0u64;
                let result =
                    Self::for_each_leaf(sconn, &squashed_block_at_height, |path, _value| {
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
