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

//! Consumer-side offline verification of a standalone PCS directory. Does not
//! require access to the source chainstate. Levels 0-2 (directory cleanliness,
//! checksums, squash-root recomputation) always run; Level 3 compares the
//! recomputed squash roots against a trusted WSCP checkpoint file.

use std::fs;
use std::path::Path;

use serde::Deserialize;
use stacks_common::types::chainstate::{SortitionId, StacksBlockId};
use stackslib::chainstate::stacks::index::marf::{MARF, MARFOpenOpts, MarfConnection};
use stackslib::chainstate::stacks::index::storage::TrieFileStorage;
use stackslib::chainstate::stacks::index::{MarfTrieId, trie_sql};
use stackslib::util_lib::db::sqlite_open;

use crate::checksums::{collect_files_recursive, compute_aggregate_checksum, sha256_file};
use crate::db::{derive_expected_epoch2_block_rel_paths, read_marf_open_opts};
use crate::layout::{
    BURNCHAIN_DB_REL, CLARITY_MARF_REL, HEADERS_DB_REL, INDEX_DB_REL, NAKAMOTO_DB_REL,
    PCS_MANIFEST, SORTITION_MARF_REL, canonical_rel_path,
};
use crate::manifest::{ChecksumsSection, SquashManifest};

/// Trusted WSCP checkpoint file format.
#[derive(Deserialize)]
pub struct CheckpointFile {
    pub stacks_height: u32,
    pub bitcoin_height: u32,
    pub clarity_squash_root_node_hash: String,
    pub index_squash_root_node_hash: String,
    pub sortition_squash_root_node_hash: String,
}

/// The files a full PCS must carry checksum entries for: the three MARF DBs
/// with their `.blobs` sidecars, plus the Bitcoin aux files and
/// `nakamoto.sqlite`. Derived from the layout consts.
fn required_pcs_files() -> Vec<String> {
    let mut files = Vec::new();
    for marf in [CLARITY_MARF_REL, INDEX_DB_REL, SORTITION_MARF_REL] {
        files.push(marf.to_string());
        files.push(format!("{marf}.blobs"));
    }
    files.push(NAKAMOTO_DB_REL.to_string());
    files.push(BURNCHAIN_DB_REL.to_string());
    files.push(HEADERS_DB_REL.to_string());
    files
}

/// Validate a `0x`-prefixed 64-hex-char hash string.  Returns `Ok(())` or an
/// error message describing what is wrong.
pub fn validate_checkpoint_hash(field_name: &str, value: &str) -> Result<(), String> {
    if !value.starts_with("0x") {
        return Err(format!("{field_name}: must start with 0x"));
    }
    if value.len() != 66 {
        return Err(format!(
            "{field_name}: expected 66 chars (0x + 64 hex), got {}",
            value.len()
        ));
    }
    if !value[2..].chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("{field_name}: contains non-hex characters"));
    }
    Ok(())
}

fn validate_full_pcs_manifest(
    manifest: &SquashManifest,
    checksums: &ChecksumsSection,
) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();

    if manifest.blocks.is_none() {
        errors.push(format!(
            "{PCS_MANIFEST} is missing the [blocks] section; \
             verify requires a full PCS produced by `squash --all`"
        ));
    }

    for (label, hash) in [
        (
            "clarity",
            manifest
                .squash_roots
                .clarity_squash_root_node_hash
                .as_deref(),
        ),
        (
            "index",
            manifest.squash_roots.index_squash_root_node_hash.as_deref(),
        ),
        (
            "sortition",
            manifest
                .squash_roots
                .sortition_squash_root_node_hash
                .as_deref(),
        ),
    ] {
        if hash.is_none() {
            errors.push(format!(
                "{PCS_MANIFEST} is missing the {label} squash root; \
                 verify requires a full PCS produced by `squash --all`"
            ));
        }
    }

    for required_file in required_pcs_files() {
        if !checksums.files.contains_key(&required_file) {
            errors.push(format!(
                "{PCS_MANIFEST} is missing the checksum entry for required PCS file \
                 `{required_file}`"
            ));
        }
    }
    if checksums.epoch2_block_archive_hash.is_none() {
        errors.push(format!(
            "{PCS_MANIFEST} is missing `checksums.epoch2_block_archive_hash`; \
             verify requires a full PCS produced by `squash --all`"
        ));
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// The expected relative paths of the epoch-2.x block files, read from the
/// PCS's own index DB (read-only).
fn read_expected_epoch2_files(pcs_dir: &Path) -> Result<Vec<String>, Vec<String>> {
    let index_db_path = pcs_dir.join(INDEX_DB_REL);
    let conn = sqlite_open(
        &index_db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        false,
    )
    .map_err(|e| {
        vec![format!(
            "Failed to open PCS index DB '{}': {e}",
            index_db_path.display()
        )]
    })?;
    derive_expected_epoch2_block_rel_paths(&conn).map_err(|e| {
        vec![format!(
            "Failed to derive expected epoch-2 block files from index.sqlite: {e}"
        )]
    })
}

/// Core verification logic for a PCS directory.  Returns `Ok(())` when all
/// requested levels pass, or `Err(errors)` with accumulated failure messages.
/// Rejects partial outputs; the directory must be a full PCS produced by
/// `squash --all`. Levels 0-2 always run; Level 3 runs when
/// `checkpoint_file` is provided.
pub fn verify_pcs(pcs_dir: &Path, checkpoint_file: Option<&Path>) -> Result<(), Vec<String>> {
    let mut errors: Vec<String> = Vec::new();

    // Require PCS_manifest.toml (full PCS only).
    let manifest_path = pcs_dir.join(PCS_MANIFEST);
    if !manifest_path.exists() {
        return Err(vec![format!(
            "{PCS_MANIFEST} not found in {}",
            pcs_dir.display()
        )]);
    }

    let manifest_str = fs::read_to_string(&manifest_path)
        .map_err(|e| vec![format!("Failed to read {}: {e}", manifest_path.display())])?;
    let manifest: SquashManifest = toml::from_str(&manifest_str)
        .map_err(|e| vec![format!("Failed to parse {PCS_MANIFEST}: {e}")])?;

    // Require [checksums] section.
    let checksums = manifest
        .checksums
        .as_ref()
        .ok_or_else(|| vec![format!("{PCS_MANIFEST} is missing the [checksums] section")])?;
    validate_full_pcs_manifest(&manifest, checksums)?;
    let expected_epoch2_files = read_expected_epoch2_files(pcs_dir)?;
    let expected_epoch2_count = expected_epoch2_files.len() as u64;
    let manifest_epoch2_count = manifest.blocks.as_ref().unwrap().epoch2x_files;
    if expected_epoch2_count != manifest_epoch2_count {
        errors.push(format!(
            "Level 0: manifest blocks.epoch2x_files {} != derived epoch-2 file count {}",
            manifest_epoch2_count, expected_epoch2_count
        ));
    }

    // Level 0: Directory cleanliness
    println!("Level 0: Checking directory cleanliness...");
    let mut disk_files: Vec<std::path::PathBuf> = Vec::new();
    if let Err(e) = collect_files_recursive(pcs_dir, pcs_dir, &mut disk_files) {
        errors.push(format!("Level 0: {e}"));
    } else {
        // Build set of expected relative paths.
        let mut expected: std::collections::HashSet<String> =
            checksums.files.keys().cloned().collect();
        expected.extend(expected_epoch2_files.iter().cloned());
        expected.insert(PCS_MANIFEST.to_string());

        for path in &disk_files {
            let rel = canonical_rel_path(path.strip_prefix(pcs_dir).unwrap());
            if !expected.contains(&rel) {
                errors.push(format!("Level 0: extra file not in manifest: {rel}"));
            }
        }
        // Also check that all manifest files exist on disk.
        let disk_set: std::collections::HashSet<String> = disk_files
            .iter()
            .map(|p| canonical_rel_path(p.strip_prefix(pcs_dir).unwrap()))
            .collect();
        for expected_file in checksums.files.keys() {
            if !disk_set.contains(expected_file.as_str()) {
                errors.push(format!(
                    "Level 0: manifest file missing from disk: {expected_file}"
                ));
            }
        }
        if errors.is_empty() {
            println!("  PASS: {} files, no extras, no symlinks", disk_files.len());
        }
    }

    // Level 1: Checksum verification
    println!("Level 1: Verifying SHA-256 checksums...");
    let mut checksum_failures = 0;
    for (rel_path, expected_hash) in &checksums.files {
        let file_path = pcs_dir.join(rel_path);
        match sha256_file(&file_path) {
            Ok(actual_hash) => {
                if actual_hash != *expected_hash {
                    errors.push(format!(
                        "Level 1: {rel_path}: expected {expected_hash}, got {actual_hash}"
                    ));
                    checksum_failures += 1;
                }
            }
            Err(e) => {
                errors.push(format!("Level 1: {rel_path}: {e}"));
                checksum_failures += 1;
            }
        }
    }
    let expected_epoch2_hash = checksums.epoch2_block_archive_hash.as_ref().unwrap();
    match compute_aggregate_checksum(pcs_dir, &expected_epoch2_files) {
        Ok(actual_hash) => {
            if actual_hash != *expected_epoch2_hash {
                errors.push(format!(
                    "Level 1: epoch-2 block archive: expected {}, got {}",
                    expected_epoch2_hash, actual_hash
                ));
                checksum_failures += 1;
            }
        }
        Err(e) => {
            errors.push(format!("Level 1: epoch-2 block archive: {e}"));
            checksum_failures += 1;
        }
    }
    if checksum_failures == 0 {
        println!(
            "  PASS: {} fixed files verified and epoch-2 block archive hash matched",
            checksums.files.len()
        );
    }

    // Level 2: Squash root recomputation
    println!("Level 2: Recomputing squash root node hashes from MARF contents...");

    let recomputed_clarity = recompute_marf_root::<StacksBlockId>(
        pcs_dir,
        CLARITY_MARF_REL,
        "clarity",
        manifest
            .squash_roots
            .clarity_squash_root_node_hash
            .as_deref(),
        &mut errors,
    );

    let recomputed_index = recompute_marf_root::<StacksBlockId>(
        pcs_dir,
        INDEX_DB_REL,
        "index",
        manifest.squash_roots.index_squash_root_node_hash.as_deref(),
        &mut errors,
    );

    let recomputed_sortition = recompute_marf_root::<SortitionId>(
        pcs_dir,
        SORTITION_MARF_REL,
        "sortition",
        manifest
            .squash_roots
            .sortition_squash_root_node_hash
            .as_deref(),
        &mut errors,
    );

    // Level 3: WSCP checkpoint comparison
    if let Some(cp_path) = checkpoint_file {
        println!("Level 3: Comparing against WSCP checkpoint...");
        let cp_str = fs::read_to_string(cp_path)
            .map_err(|e| vec![format!("Failed to read checkpoint file: {e}")])?;
        let cp: CheckpointFile = toml::from_str(&cp_str)
            .map_err(|e| vec![format!("Failed to parse checkpoint file: {e}")])?;

        // Validate checkpoint hash fields (prefix + length + hex characters).
        validate_checkpoint_hash(
            "clarity_squash_root_node_hash",
            &cp.clarity_squash_root_node_hash,
        )
        .map_err(|e| vec![e])?;
        validate_checkpoint_hash(
            "index_squash_root_node_hash",
            &cp.index_squash_root_node_hash,
        )
        .map_err(|e| vec![e])?;
        validate_checkpoint_hash(
            "sortition_squash_root_node_hash",
            &cp.sortition_squash_root_node_hash,
        )
        .map_err(|e| vec![e])?;

        // Heights must match.
        if cp.stacks_height != manifest.snapshot.stacks_height {
            errors.push(format!(
                "Level 3: checkpoint stacks_height {} != manifest stacks_height {}",
                cp.stacks_height, manifest.snapshot.stacks_height
            ));
        }
        if cp.bitcoin_height != manifest.snapshot.bitcoin_height {
            errors.push(format!(
                "Level 3: checkpoint bitcoin_height {} != manifest bitcoin_height {}",
                cp.bitcoin_height, manifest.snapshot.bitcoin_height
            ));
        }

        // Compare recomputed hashes against checkpoint (not stored metadata).
        // Normalize checkpoint values to lowercase so that valid uppercase hex
        // does not spuriously fail (recomputed hashes are always lowercase).
        let check = |name: &str, recomputed: &Option<String>, checkpoint: &str| -> Option<String> {
            let checkpoint_lower = checkpoint.to_ascii_lowercase();
            match recomputed {
                Some(hash) if *hash == checkpoint_lower => {
                    println!("  PASS: {name} matches checkpoint");
                    None
                }
                Some(hash) => Some(format!(
                    "Level 3: {name} recomputed={hash}, checkpoint={checkpoint_lower}"
                )),
                None => Some(format!(
                    "Level 3: {name} not present in PCS (cannot compare)"
                )),
            }
        };

        if let Some(e) = check(
            "clarity",
            &recomputed_clarity,
            &cp.clarity_squash_root_node_hash,
        ) {
            errors.push(e);
        }
        if let Some(e) = check("index", &recomputed_index, &cp.index_squash_root_node_hash) {
            errors.push(e);
        }
        if let Some(e) = check(
            "sortition",
            &recomputed_sortition,
            &cp.sortition_squash_root_node_hash,
        ) {
            errors.push(e);
        }
    } else {
        println!("Level 3: Skipped (no --checkpoint-file provided)");
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Recompute the squash root node hash for a single MARF and compare against
/// the manifest value.  Returns the `0x`-prefixed hex hash if successful.
/// Pushes error strings into `errors` on failure.
fn recompute_marf_root<T: MarfTrieId>(
    pcs_dir: &Path,
    rel_path: &str,
    name: &str,
    manifest_hash: Option<&str>,
    errors: &mut Vec<String>,
) -> Option<String> {
    let db_path = pcs_dir.join(rel_path);
    if !db_path.exists() {
        if manifest_hash.is_some() {
            errors.push(format!(
                "Level 2: {name} MARF not found at {}",
                db_path.display()
            ));
        } else {
            println!("  SKIP: {name} (not in PCS)");
        }
        return None;
    }

    let open_opts: MARFOpenOpts = read_marf_open_opts(&db_path);
    let storage = match TrieFileStorage::open_readonly(db_path.to_str().unwrap(), open_opts) {
        Ok(s) => s,
        Err(e) => {
            errors.push(format!("Level 2: {name}: failed to open MARF: {e:?}"));
            return None;
        }
    };
    let mut marf = MARF::<T>::from_storage(storage);

    let tip = match trie_sql::get_latest_confirmed_block_hash(marf.sqlite_conn()) {
        Ok(t) => t,
        Err(e) => {
            errors.push(format!("Level 2: {name}: failed to read tip: {e:?}"));
            return None;
        }
    };

    let recomputed = match marf.recompute_squash_root_node_hash(&tip) {
        Ok(h) => h,
        Err(e) => {
            errors.push(format!("Level 2: {name}: recomputation error: {e:?}"));
            return None;
        }
    };

    let recomputed_hex = format!("0x{recomputed}");

    match manifest_hash {
        Some(expected) => {
            if recomputed_hex == expected {
                println!("  PASS: {name} recomputed hash matches manifest");
            } else {
                errors.push(format!(
                    "Level 2: {name} recomputed={recomputed_hex}, manifest={expected}"
                ));
            }
        }
        None => {
            // MARF exists on disk but manifest has no squash root hash - this is
            // a malformed manifest for a full PCS.
            errors.push(format!(
                "Level 2: {name} MARF exists but manifest has no squash root hash"
            ));
        }
    }

    Some(recomputed_hex)
}
