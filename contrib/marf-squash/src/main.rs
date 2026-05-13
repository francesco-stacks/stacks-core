mod cli;
mod commands;
mod manifest;
mod ops;
mod util;
mod verify;

use clap::Parser;
use cli::{Cli, Command};
use commands::{run_squash, run_validate, run_verify};

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Squash(args) => run_squash(args),
        Command::Validate(args) => run_validate(args),
        Command::Verify(args) => run_verify(args),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use clap::Parser;
    use stacks_common::types::chainstate::{SortitionId, StacksBlockId};
    use stackslib::chainstate::stacks::index::marf::{MARF, MarfConnection};
    use stackslib::chainstate::stacks::index::{MARFValue, MarfTrieId, trie_sql};

    use crate::cli::{
        BlocksSection, ChecksumsSection, Cli, Command, GSS_MANIFEST, RootsSection, SnapshotSection,
        SquashManifest, SquashRootsSection, ValidateArgs,
    };
    use crate::util::{
        compute_aggregate_checksum, compute_checksums, epoch2_block_rel_path, sha256_file,
        squash_marf_open_opts,
    };
    use crate::verify::{validate_checkpoint_hash, verify_gss};

    //  Helpers

    fn create_test_gss_dir(dir: &std::path::Path, files: &[&str]) {
        for f in files {
            let path = dir.join(f);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&path, format!("content of {f}")).unwrap();
        }
    }

    fn no_squash_roots() -> SquashRootsSection {
        SquashRootsSection {
            clarity_squash_root_node_hash: None,
            index_squash_root_node_hash: None,
            sortition_squash_root_node_hash: None,
        }
    }

    fn write_manifest_toml(
        dir: &std::path::Path,
        checksums: Option<ChecksumsSection>,
        squash_roots: SquashRootsSection,
    ) {
        let manifest = SquashManifest {
            snapshot: SnapshotSection {
                version: 1,
                stacks_height: 100,
                bitcoin_height: 869704,
                block_hash: "0xdeadbeef".to_string(),
                bitcoin_block_hash: None,
                timestamp: None,
                chain_id: 1,
                mainnet: true,
            },
            roots: RootsSection {
                clarity_archival_marf_root_hash: None,
                index_archival_marf_root_hash: "0xaaa".to_string(),
                sortition_archival_marf_root_hash: None,
            },
            squash_roots,
            blocks: None,
            checksums,
        };
        let toml_str = toml::to_string(&manifest).unwrap();
        std::fs::write(dir.join(GSS_MANIFEST), toml_str).unwrap();
    }

    fn write_test_marf<T: MarfTrieId>(
        db_path: &std::path::Path,
        block_byte: u8,
        key: &str,
        value: &str,
    ) -> String {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }

        let open_opts = squash_marf_open_opts();
        let mut marf = MARF::from_path(db_path.to_str().unwrap(), open_opts.clone()).unwrap();
        let tip = T::from_bytes([block_byte; 32]);
        let mut tx = marf.begin_tx().unwrap();
        tx.begin(&T::sentinel(), &tip).unwrap();
        tx.insert_batch(&[key.to_string()], vec![MARFValue::from_value(value)])
            .unwrap();
        tx.commit().unwrap();
        drop(marf);

        let mut marf = MARF::from_path(db_path.to_str().unwrap(), open_opts).unwrap();
        let tip = trie_sql::get_latest_confirmed_block_hash::<T>(marf.sqlite_conn()).unwrap();
        let root = marf.recompute_squash_root_node_hash(&tip).unwrap();
        format!("0x{root}")
    }

    fn create_full_gss_fixture(dir: &std::path::Path) -> (String, String, String) {
        let clarity_root = write_test_marf::<StacksBlockId>(
            &dir.join("chainstate/vm/clarity/marf.sqlite"),
            0x11,
            "clarity-key",
            "clarity-value",
        );
        let index_root = write_test_marf::<StacksBlockId>(
            &dir.join("chainstate/vm/index.sqlite"),
            0x22,
            "index-key",
            "index-value",
        );
        let sortition_root = write_test_marf::<SortitionId>(
            &dir.join("burnchain/sortition/marf.sqlite"),
            0x33,
            "sortition-key",
            "sortition-value",
        );

        create_test_gss_dir(
            dir,
            &[
                "burnchain/burnchain.sqlite",
                "headers.sqlite",
                "chainstate/blocks/nakamoto.sqlite",
            ],
        );
        let epoch2_hash = StacksBlockId([0x44; 32]);
        let epoch2_rel_path = epoch2_block_rel_path(&epoch2_hash);
        let epoch2_bytes = b"epoch2-block-data";
        let epoch2_path = dir.join(&epoch2_rel_path);
        std::fs::create_dir_all(epoch2_path.parent().unwrap()).unwrap();
        std::fs::write(&epoch2_path, epoch2_bytes).unwrap();

        let index_conn =
            rusqlite::Connection::open(dir.join("chainstate/vm/index.sqlite")).unwrap();
        index_conn
            .execute(
                "CREATE TABLE block_headers (index_block_hash TEXT NOT NULL, block_height INTEGER NOT NULL)",
                [],
            )
            .unwrap();
        index_conn
            .execute(
                "INSERT INTO block_headers (index_block_hash, block_height) VALUES (?1, ?2)",
                rusqlite::params![epoch2_hash.to_string(), 1i64],
            )
            .unwrap();
        drop(index_conn);

        let mut files = compute_checksums(dir, None, None).unwrap();
        files.remove(&epoch2_rel_path);
        let epoch2_block_archive_hash =
            compute_aggregate_checksum(dir, std::slice::from_ref(&epoch2_rel_path)).unwrap();

        let manifest = SquashManifest {
            snapshot: SnapshotSection {
                version: 1,
                stacks_height: 100,
                bitcoin_height: 869704,
                block_hash: "0xdeadbeef".to_string(),
                bitcoin_block_hash: Some("0xbeef".to_string()),
                timestamp: None,
                chain_id: 1,
                mainnet: true,
            },
            roots: RootsSection {
                clarity_archival_marf_root_hash: Some("0xaaa".to_string()),
                index_archival_marf_root_hash: "0xbbb".to_string(),
                sortition_archival_marf_root_hash: Some("0xccc".to_string()),
            },
            squash_roots: SquashRootsSection {
                clarity_squash_root_node_hash: Some(clarity_root.clone()),
                index_squash_root_node_hash: Some(index_root.clone()),
                sortition_squash_root_node_hash: Some(sortition_root.clone()),
            },
            blocks: Some(BlocksSection {
                epoch2x_files: 1,
                epoch2x_bytes: epoch2_bytes.len() as u64,
                epoch2x_microblock_rows: 0,
                epoch2x_microblock_bytes: 0,
                nakamoto_rows: 0,
                nakamoto_bytes: 0,
            }),
            checksums: Some(ChecksumsSection {
                files,
                epoch2_block_archive_hash: Some(epoch2_block_archive_hash),
            }),
        };
        let toml_str = toml::to_string(&manifest).unwrap();
        std::fs::write(dir.join(GSS_MANIFEST), toml_str).unwrap();

        (clarity_root, index_root, sortition_root)
    }

    //  CLI parsing

    #[test]
    fn test_parse_squash_args_ok() {
        let args = vec![
            "marf-squash",
            "squash",
            "--chainstate",
            "/tmp/chainstate",
            "--tenure-start-bitcoin-height",
            "869704",
            "--out-dir",
            "/tmp/out",
            "--index",
        ]
        .into_iter()
        .map(String::from);

        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Command::Squash(args) => {
                assert_eq!(args.chainstate, PathBuf::from("/tmp/chainstate"));
                assert_eq!(args.tenure_start_bitcoin_height, 869704);
                assert!(args.index);
            }
            _ => panic!("expected squash command"),
        }
    }

    #[test]
    fn test_parse_squash_args_sortition() {
        let args = vec![
            "marf-squash",
            "squash",
            "--chainstate",
            "/tmp/chainstate",
            "--tenure-start-bitcoin-height",
            "869704",
            "--out-dir",
            "/tmp/out",
            "--sortition",
        ]
        .into_iter()
        .map(String::from);

        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Command::Squash(args) => {
                assert!(args.sortition);
                assert!(!args.clarity);
                assert!(!args.index);
            }
            _ => panic!("expected squash command"),
        }
    }

    #[test]
    fn test_parse_validate_args_ok() {
        let args = vec![
            "marf-squash",
            "validate",
            "--source-chainstate",
            "/tmp/source",
            "--squashed-chainstate",
            "/tmp/squashed",
            "--tenure-start-bitcoin-height",
            "869704",
            "--clarity",
        ]
        .into_iter()
        .map(String::from);

        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Command::Validate(ValidateArgs {
                source_chainstate,
                squashed_chainstate,
                tenure_start_bitcoin_height,
                clarity,
                ..
            }) => {
                assert_eq!(source_chainstate, PathBuf::from("/tmp/source"));
                assert_eq!(squashed_chainstate, PathBuf::from("/tmp/squashed"));
                assert_eq!(tenure_start_bitcoin_height, 869704);
                assert!(clarity);
            }
            _ => panic!("expected validate command"),
        }
    }
    #[test]
    fn test_parse_verify_args_ok() {
        let args = vec![
            "marf-squash",
            "verify",
            "--gss-dir",
            "/tmp/gss",
            "--checkpoint-file",
            "/tmp/checkpoint.toml",
        ]
        .into_iter()
        .map(String::from);

        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Command::Verify(args) => {
                assert_eq!(args.gss_dir, PathBuf::from("/tmp/gss"));
                assert_eq!(
                    args.checkpoint_file,
                    Some(PathBuf::from("/tmp/checkpoint.toml"))
                );
            }
            _ => panic!("expected verify command"),
        }
    }

    #[test]
    fn test_parse_verify_args_no_checkpoint() {
        let args = vec!["marf-squash", "verify", "--gss-dir", "/tmp/gss"]
            .into_iter()
            .map(String::from);

        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Command::Verify(args) => {
                assert_eq!(args.gss_dir, PathBuf::from("/tmp/gss"));
                assert!(args.checkpoint_file.is_none());
            }
            _ => panic!("expected verify command"),
        }
    }

    //  compute_checksums / collect_files_recursive

    #[test]
    fn test_compute_checksums_clean_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        create_test_gss_dir(dir, &["a.sqlite", "sub/b.sqlite"]);
        std::fs::write(dir.join(GSS_MANIFEST), "dummy").unwrap();

        let checksums = compute_checksums(dir, None, None).unwrap();
        assert_eq!(checksums.len(), 2);
        assert!(checksums.contains_key("a.sqlite"));
        assert!(checksums.contains_key("sub/b.sqlite"));
        let expected = sha256_file(&dir.join("a.sqlite")).unwrap();
        assert_eq!(checksums["a.sqlite"], expected);
    }

    #[test]
    fn test_compute_checksums_ignores_sqlite_sidecars() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        create_test_gss_dir(dir, &["a.sqlite", "a.sqlite-wal"]);

        let checksums = compute_checksums(dir, None, None).unwrap();
        assert_eq!(checksums.len(), 1);
        assert!(checksums.contains_key("a.sqlite"));
    }

    #[test]
    fn test_manifest_rejects_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        create_test_gss_dir(dir, &["a.sqlite"]);
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.join("a.sqlite"), dir.join("link.sqlite")).unwrap();

        #[cfg(unix)]
        {
            let result = compute_checksums(dir, None, None);
            assert!(result.is_err());
            assert!(result.unwrap_err().contains("symlink"));
        }
    }

    #[test]
    fn test_manifest_rejects_extra_file_in_outdir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        create_test_gss_dir(dir, &["expected.sqlite", "stale.sqlite"]);

        let mut expected = std::collections::HashSet::new();
        expected.insert("expected.sqlite".to_string());

        let result = compute_checksums(dir, Some(&expected), None);
        let err = result.unwrap_err();
        assert!(err.contains("unexpected file"), "got: {err}");
        assert!(err.contains("stale.sqlite"), "got: {err}");
    }

    #[test]
    fn test_compute_checksums_rejects_stale_block_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let blocks_dir = dir.join("chainstate/blocks/ab/cd");
        std::fs::create_dir_all(&blocks_dir).unwrap();
        std::fs::write(blocks_dir.join("legit_block"), "data").unwrap();
        std::fs::write(blocks_dir.join("stale_block"), "old data").unwrap();

        let mut expected = std::collections::HashSet::new();
        expected.insert("chainstate/blocks/ab/cd/legit_block".to_string());

        let result = compute_checksums(dir, Some(&expected), None);
        let err = result.unwrap_err();
        assert!(err.contains("unexpected file"), "got: {err}");
        assert!(err.contains("stale_block"), "got: {err}");
    }

    //  validate_checkpoint_hash

    #[test]
    fn test_validate_checkpoint_hash_valid() {
        let hash = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        assert!(validate_checkpoint_hash("test_field", hash).is_ok());
    }

    #[test]
    fn test_validate_checkpoint_hash_no_prefix() {
        let hash = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa00";
        let err = validate_checkpoint_hash("test_field", hash).unwrap_err();
        assert!(err.contains("must start with 0x"), "got: {err}");
    }

    #[test]
    fn test_validate_checkpoint_hash_wrong_length() {
        let hash = "0xtooshort";
        let err = validate_checkpoint_hash("test_field", hash).unwrap_err();
        assert!(err.contains("expected 66 chars"), "got: {err}");
    }

    #[test]
    fn test_validate_checkpoint_hash_invalid_hex_chars() {
        let hash = "0xgggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggg";
        assert_eq!(hash.len(), 66);
        let err = validate_checkpoint_hash("test_field", hash).unwrap_err();
        assert!(err.contains("non-hex"), "got: {err}");
    }

    //  End-to-end verify_gss

    #[test]
    fn test_verify_gss_end_to_end_valid() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        create_full_gss_fixture(dir);

        let result = verify_gss(dir, None);
        assert!(result.is_ok(), "expected pass, got: {result:?}");
    }

    #[test]
    fn test_verify_gss_rejects_partial_gss() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        create_test_gss_dir(dir, &["data.sqlite"]);

        let hash = sha256_file(&dir.join("data.sqlite")).unwrap();
        let mut files = BTreeMap::new();
        files.insert("data.sqlite".to_string(), hash);
        write_manifest_toml(
            dir,
            Some(ChecksumsSection {
                files,
                epoch2_block_archive_hash: None,
            }),
            no_squash_roots(),
        );

        let result = verify_gss(dir, None);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e: &String| e.contains("[blocks] section")),
            "expected full-GSS error, got: {errors:?}"
        );
    }

    #[test]
    fn test_verify_gss_rejects_missing_checksums() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write_manifest_toml(dir, None, no_squash_roots());

        let result = verify_gss(dir, None);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(
            errors.iter().any(|e: &String| e.contains("[checksums]")),
            "expected checksums error, got: {errors:?}"
        );
    }

    #[test]
    fn test_verify_gss_checkpoint_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let (clarity_root, index_root, _sortition_root) = create_full_gss_fixture(dir);

        let cp_toml = format!(
            r#"
stacks_height = 100
bitcoin_height = 869704
clarity_squash_root_node_hash = "{clarity_root}"
index_squash_root_node_hash = "{index_root}"
sortition_squash_root_node_hash = "0xcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
"#
        );
        let cp_path = dir.join("checkpoint.toml");
        std::fs::write(&cp_path, cp_toml).unwrap();

        let result = verify_gss(dir, Some(&cp_path));
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e: &String| e.contains("Level 3") && e.contains("recomputed=")),
            "expected Level 3 failure, got: {errors:?}"
        );
    }
}
