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

pub(crate) mod blocks;
pub(crate) mod burnchain;
pub(crate) mod common;
pub(crate) mod fork_storage;
pub(crate) mod index;
pub(crate) mod sortition;
pub(crate) mod spv;

#[cfg(test)]
mod tests;

// Re-export public API so existing `use snapshot::*` imports continue to work.
pub use blocks::{
    copy_confirmed_epoch2_microblocks, copy_epoch2_block_files, copy_nakamoto_staging_blocks,
    validate_epoch2_block_files, validate_microblock_streams, validate_nakamoto_staging_blocks,
    Epoch2BlockFileCopyStats, Epoch2BlockFileValidation, Epoch2MicroblockCopyStats,
    MicroblockValidation, NakamotoBlockCopyStats, NakamotoBlockValidation,
};
pub use burnchain::{
    copy_burnchain_db, validate_burnchain_db, BurnchainDbCopyStats, BurnchainDbValidation,
};
pub use index::{
    copy_index_side_tables, validate_index_side_tables, IndexSideTableStats,
    IndexSideTableValidation,
};
pub use sortition::{
    copy_sortition_side_tables, copy_sortition_side_tables_with_boundary,
    validate_sortition_side_tables, validate_sortition_side_tables_with_boundary,
    SortitionSideTableStats, SortitionSideTableValidation, SortitionTipCopyBoundary,
};
pub use spv::{copy_spv_headers, validate_spv_headers, SpvHeadersCopyStats, SpvHeadersValidation};
