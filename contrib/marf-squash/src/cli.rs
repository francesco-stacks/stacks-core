use std::collections::BTreeMap;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

/// Offline squashing CLI for Index, Clarity, and Sortition MARF snapshots.
#[derive(Parser, Debug)]
#[command(
    name = "marf-squash",
    about = "Offline squashing tool for Index, Clarity, and Sortition MARFs"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Create squashed MARFs and validate against the source.
    Squash(SquashArgs),
    /// Validate squashed MARFs against a source chainstate.
    Validate(ValidateArgs),
    /// Verify a standalone GSS directory's integrity and optionally check WSCP checkpoint.
    Verify(VerifyArgs),
    /// Print the latest confirmed block height in a MARF.
    LatestHeight(LatestHeightArgs),
    /// Diagnose state root divergence by comparing ancestor hashes between
    /// squashed and archival clarity MARFs.
    Diagnose(DiagnoseArgs),
}

/// Arguments for diagnosing state root divergence.
#[derive(Parser, Debug)]
pub struct DiagnoseArgs {
    /// Path to the archival chainstate folder.
    #[arg(long, value_name = "DIR")]
    pub archival_chainstate: PathBuf,
    /// Path to the squashed chainstate folder.
    #[arg(long, value_name = "DIR")]
    pub squashed_chainstate: PathBuf,
    /// Stacks block height to diagnose (the failing block).
    #[arg(long, value_name = "HEIGHT")]
    pub stacks_height: u32,
}

/// Arguments for generating squashed MARFs.
#[derive(Parser, Debug)]
pub struct SquashArgs {
    /// Path to the chainstate folder (the parent of chainstate/ and burnchain/).
    #[arg(long, value_name = "DIR")]
    pub chainstate: PathBuf,
    /// Output directory for the squashed MARF files.
    #[arg(long = "out-dir", value_name = "DIR")]
    pub out_dir: PathBuf,
    /// Bitcoin block height where a Nakamoto tenure started (sortition=true).
    /// The snapshot includes the complete tenure: all Stacks blocks produced
    /// by the miner who won this sortition. Epoch 3.x (Nakamoto) only.
    #[arg(long, value_name = "HEIGHT")]
    pub tenure_start_bitcoin_height: u64,
    /// Squash the Clarity MARF (chainstate/vm/clarity/marf.sqlite).
    #[arg(long)]
    pub clarity: bool,
    /// Squash the Index MARF (chainstate/vm/index.sqlite).
    #[arg(long)]
    pub index: bool,
    /// Squash the Sortition MARF (burnchain/sortition/marf.sqlite).
    #[arg(long)]
    pub sortition: bool,
    /// Squash all three MARFs (Clarity, Index, Sortition).
    #[arg(long)]
    pub all: bool,
    /// Copy canonical block data (epoch 2.x files, confirmed microblocks, nakamoto.sqlite).
    /// Requires --index (or --all).
    #[arg(long)]
    pub blocks: bool,
    /// Copy Bitcoin auxiliary files (burnchain.sqlite + headers.sqlite).
    /// Requires --sortition (or --all).
    #[arg(long)]
    pub bitcoin: bool,
    /// Skip validation to speed up size measurements.
    #[arg(long = "skip-validate")]
    pub skip_validate: bool,
    /// Path to the node config TOML file. Used to extract PoX constants
    /// Required for testnet.
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,
    /// Run full leaf-by-leaf comparison (slow, O(leaf_count)).
    /// By default, validation uses the fast hash-based check.
    #[arg(long)]
    pub full: bool,
}

/// Arguments for validating squashed MARFs against a source.
#[derive(Parser, Debug)]
pub struct ValidateArgs {
    /// Path to the source chainstate folder.
    #[arg(long = "source-chainstate", value_name = "DIR")]
    pub source_chainstate: PathBuf,
    /// Path to the squashed chainstate folder.
    #[arg(long = "squashed-chainstate", value_name = "DIR")]
    pub squashed_chainstate: PathBuf,
    /// Bitcoin block height where the Nakamoto tenure started.
    #[arg(long, value_name = "HEIGHT")]
    pub tenure_start_bitcoin_height: u64,
    /// Validate the Clarity MARF.
    #[arg(long)]
    pub clarity: bool,
    /// Validate the Index MARF.
    #[arg(long)]
    pub index: bool,
    /// Validate the Sortition MARF.
    #[arg(long)]
    pub sortition: bool,
    /// Validate all three MARFs.
    #[arg(long)]
    pub all: bool,
    /// Validate block data (epoch 2.x files, confirmed microblocks, nakamoto.sqlite).
    #[arg(long)]
    pub blocks: bool,
    /// Validate Bitcoin auxiliary files (burnchain.sqlite + headers.sqlite).
    #[arg(long)]
    pub bitcoin: bool,
    /// Path to the node config TOML file. Used to extract PoX constants
    /// Required for testnet.
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,
    /// Run full leaf-by-leaf comparison (slow, O(leaf_count)).
    /// By default, validation uses the fast hash-based check.
    #[arg(long)]
    pub full: bool,
}

/// Arguments for reporting the latest confirmed height.
#[derive(Parser, Debug)]
pub struct LatestHeightArgs {
    /// Path to the chainstate folder.
    #[arg(long, value_name = "DIR")]
    pub chainstate: PathBuf,
    /// Read the latest height from the Clarity MARF.
    #[arg(long)]
    pub clarity: bool,
    /// Read the latest height from the Index MARF.
    #[arg(long)]
    pub index: bool,
    /// Read the latest height from the Sortition MARF (prints burn block height).
    #[arg(long)]
    pub sortition: bool,
}

/// Arguments for standalone GSS verification.
#[derive(Parser, Debug)]
pub struct VerifyArgs {
    /// Path to a GSS directory (must contain GSS_manifest.toml).
    #[arg(long, value_name = "DIR")]
    pub gss_dir: PathBuf,
    /// Path to a TOML file with trusted WSCP checkpoint hashes.
    #[arg(long, value_name = "FILE")]
    pub checkpoint_file: Option<PathBuf>,
}

/// Trusted WSCP checkpoint file format.
#[derive(Deserialize)]
pub struct CheckpointFile {
    pub stacks_height: u32,
    pub bitcoin_height: u64,
    pub clarity_squash_root_node_hash: String,
    pub index_squash_root_node_hash: String,
    pub sortition_squash_root_node_hash: String,
}

#[derive(Debug, Clone)]
pub struct TargetPaths {
    pub db: PathBuf,
    pub blobs: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct ChainstatePaths {
    pub clarity: TargetPaths,
    pub index: TargetPaths,
    pub sortition: TargetPaths,
}

#[derive(Serialize, Deserialize)]
pub struct SquashManifest {
    pub snapshot: SnapshotSection,
    pub roots: RootsSection,
    pub squash_roots: SquashRootsSection,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocks: Option<BlocksSection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksums: Option<ChecksumsSection>,
}

#[derive(Serialize, Deserialize)]
pub struct SnapshotSection {
    pub version: u32,
    pub stacks_height: u32,
    pub bitcoin_height: u64,
    pub block_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bitcoin_block_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    pub chain_id: u32,
    pub mainnet: bool,
}

#[derive(Serialize, Deserialize)]
pub struct RootsSection {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clarity_archival_marf_root_hash: Option<String>,
    pub index_archival_marf_root_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sortition_archival_marf_root_hash: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct SquashRootsSection {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clarity_squash_root_node_hash: Option<String>,
    pub index_squash_root_node_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sortition_squash_root_node_hash: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct BlocksSection {
    pub epoch2x_files: u64,
    pub epoch2x_bytes: u64,
    pub epoch2x_microblock_rows: u64,
    pub epoch2x_microblock_bytes: u64,
    pub nakamoto_rows: u64,
    pub nakamoto_bytes: u64,
}

#[derive(Serialize, Deserialize)]
pub struct ChecksumsSection {
    pub files: BTreeMap<String, String>,
}

/// Manifest file names.
pub const GSS_MANIFEST: &str = "GSS_manifest.toml";

/// File extensions that indicate SQLite sidecars (WAL, SHM, journal).
pub const SQLITE_SIDECAR_EXTENSIONS: &[&str] = &["sqlite-wal", "sqlite-shm", "sqlite-journal"];
