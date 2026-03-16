// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020-2026 Stacks Open Internet Foundation
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

use rusqlite::{params, Connection, OptionalExtension};
use stacks_common::util::hash::to_hex;

use crate::chainstate::stacks::index::{Error, MARFValue};
use crate::types::chainstate::{TrieHash, TRIEHASH_ENCODED_SIZE};

pub static SQL_MARF_SQUASHED_STATE_TABLE: &str = "
CREATE TABLE IF NOT EXISTS marf_squashed_state (
    path_hex TEXT PRIMARY KEY,
    path BLOB NOT NULL,
    value BLOB NOT NULL,
    source_block_id INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS index_marf_squashed_state_source_block_id
    ON marf_squashed_state(source_block_id);
";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SquashedStateRow {
    pub path: TrieHash,
    pub value: MARFValue,
    pub source_block_id: u32,
}

fn decode_state_row(
    path_bytes: Vec<u8>,
    value_bytes: Vec<u8>,
    source_block_id: i64,
) -> Result<SquashedStateRow, Error> {
    let path_slice = path_bytes
        .get(..TRIEHASH_ENCODED_SIZE)
        .ok_or_else(|| Error::CorruptionError("Invalid squashed-state path length".into()))?;
    let path = TrieHash::from_bytes(path_slice)
        .ok_or_else(|| Error::CorruptionError("Invalid squashed-state path bytes".into()))?;

    let value_slice = value_bytes
        .get(..40)
        .ok_or_else(|| Error::CorruptionError("Invalid squashed-state value length".into()))?;
    let mut value_arr = [0u8; 40];
    value_arr.copy_from_slice(value_slice);

    Ok(SquashedStateRow {
        path,
        value: MARFValue(value_arr),
        source_block_id: source_block_id as u32,
    })
}

pub fn put_squashed_state_row(
    conn: &Connection,
    path: &TrieHash,
    value: &MARFValue,
    source_block_id: u32,
) -> Result<(), Error> {
    conn.execute(
        "INSERT OR REPLACE INTO marf_squashed_state (path_hex, path, value, source_block_id)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            to_hex(path.as_bytes()),
            path.as_bytes().to_vec(),
            value.as_bytes().to_vec(),
            source_block_id as i64
        ],
    )?;
    Ok(())
}

pub fn get_squashed_state_row(
    conn: &Connection,
    path: &TrieHash,
) -> Result<Option<SquashedStateRow>, Error> {
    let result: Option<(Vec<u8>, Vec<u8>, i64)> = conn
        .query_row(
            "SELECT path, value, source_block_id
             FROM marf_squashed_state
             WHERE path_hex = ?1",
            params![to_hex(path.as_bytes())],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;

    result
        .map(|(path_bytes, value_bytes, source_block_id)| {
            decode_state_row(path_bytes, value_bytes, source_block_id)
        })
        .transpose()
}

pub fn read_squashed_state_prefix(
    conn: &Connection,
    prefix: &[u8],
) -> Result<Vec<SquashedStateRow>, Error> {
    let mut stmt = conn.prepare(
        "SELECT path, value, source_block_id
         FROM marf_squashed_state
         WHERE path_hex LIKE (?1 || '%')
         ORDER BY path_hex ASC",
    )?;
    let rows = stmt.query_map(params![to_hex(prefix)], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, Vec<u8>>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;

    let mut out = vec![];
    for row in rows {
        let (path_bytes, value_bytes, source_block_id) = row?;
        out.push(decode_state_row(path_bytes, value_bytes, source_block_id)?);
    }
    Ok(out)
}

pub fn clear_squashed_state(conn: &Connection) -> Result<(), Error> {
    conn.execute("DELETE FROM marf_squashed_state", [])?;
    Ok(())
}
