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

use std::path::PathBuf;

use clap::{Parser, Subcommand};

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
    /// Create squashed MARFs from a source chainstate and validate them against it.
    Squash(SquashArgs),
    /// Validate squashed MARFs against a source chainstate.
    Validate(ValidateArgs),
    /// Verify a standalone PCS directory's integrity and optionally check a WSCP checkpoint.
    Verify(VerifyArgs),
}

/// Arguments for generating squashed MARFs.
#[derive(Parser, Debug)]
pub struct SquashArgs {
    /// Path to the chainstate folder (the parent of chainstate/ and burnchain/).
    #[arg(long, value_name = "DIR")]
    pub chainstate: PathBuf,
    /// Output directory -- the node's `working_dir`. The squash writes a
    /// directly bootable `<out-dir>/<network>/` tree. `<network>` is `mainnet`
    /// for a mainnet chainstate, otherwise the source chainstate's own
    /// subdirectory name (e.g. `krypton`).
    #[arg(long = "out-dir", value_name = "DIR")]
    pub out_dir: PathBuf,
    /// Bitcoin block height where a Nakamoto tenure started (sortition=true).
    /// The snapshot includes the complete tenure: all Stacks blocks produced
    /// by the miner who won this sortition. Epoch 3.x (Nakamoto) only.
    #[arg(long, value_name = "HEIGHT")]
    pub tenure_start_bitcoin_height: u32,
    /// Squash the Clarity MARF (chainstate/vm/clarity/marf.sqlite).
    #[arg(long)]
    pub clarity: bool,
    /// Squash the Index MARF (chainstate/vm/index.sqlite).
    #[arg(long)]
    pub index: bool,
    /// Squash the Sortition MARF (burnchain/sortition/marf.sqlite).
    #[arg(long)]
    pub sortition: bool,
    /// Squash all three MARFs and copy all auxiliary data (blocks + bitcoin).
    #[arg(long)]
    pub all: bool,
    /// Copy canonical block data (epoch 2.x files, confirmed microblocks, nakamoto.sqlite).
    /// Requires --index (is implied by --all).
    #[arg(long)]
    pub blocks: bool,
    /// Copy Bitcoin auxiliary files (burnchain.sqlite + headers.sqlite).
    /// Requires --sortition (is implied by --all).
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
    pub tenure_start_bitcoin_height: u32,
    /// Validate the Clarity MARF.
    #[arg(long)]
    pub clarity: bool,
    /// Validate the Index MARF.
    #[arg(long)]
    pub index: bool,
    /// Validate the Sortition MARF.
    #[arg(long)]
    pub sortition: bool,
    /// Validate all three MARFs and auxiliary data (blocks + bitcoin).
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

/// Arguments for standalone PCS verification.
#[derive(Parser, Debug)]
pub struct VerifyArgs {
    /// Path to a PCS directory (must contain PCS_manifest.toml).
    #[arg(long, value_name = "DIR")]
    pub pcs_dir: PathBuf,
    /// Path to a TOML file with trusted WSCP checkpoint hashes.
    #[arg(long, value_name = "FILE")]
    pub checkpoint_file: Option<PathBuf>,
}
