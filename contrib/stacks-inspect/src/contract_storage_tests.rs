// Copyright (C) 2025-2026 Stacks Open Internet Foundation
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
use std::time::{SystemTime, UNIX_EPOCH};

use clarity::vm::ClarityVersion;
use clarity::vm::contexts::ContractContext;
use clarity::vm::database::contract_storage::{CONTRACT_HEADER_KEY, LEGACY_CONTRACT_KEY};
use clarity::vm::database::{ClaritySerializable, SqliteConnection};
use clarity::vm::types::QualifiedContractIdentifier;
use rusqlite::{Connection, params};

use super::migrate_standalone_contract_storage;

struct DatabaseFile(PathBuf);

impl DatabaseFile {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "stacks-inspect-migration-{}-{}.sqlite",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(),
        )))
    }

    fn path(&self) -> &str {
        self.0.to_str().unwrap()
    }
}

impl Drop for DatabaseFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn explicit_migration_converts_existing_records_and_is_idempotent() {
    let file = DatabaseFile::new();
    let conn = Connection::open(file.path()).unwrap();
    SqliteConnection::initialize_conn(&conn).unwrap();
    let id = QualifiedContractIdentifier::local("legacy").unwrap();
    let context = ContractContext::new(id.clone(), ClarityVersion::Clarity2);
    conn.execute(
        "INSERT INTO metadata_table(key,blockhash,value) VALUES(?1,'block',?2)",
        params![
            format!("clr-meta::{id}::{LEGACY_CONTRACT_KEY}"),
            context.serialize()
        ],
    )
    .unwrap();
    drop(conn);

    assert_eq!(migrate_standalone_contract_storage(file.path()).unwrap(), 1);
    assert_eq!(migrate_standalone_contract_storage(file.path()).unwrap(), 0);
    let conn = Connection::open(file.path()).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT typeof(value) FROM metadata_table WHERE key=?1 AND blockhash='block'",
            params![format!("clr-meta::{id}::{CONTRACT_HEADER_KEY}")],
            |r| r.get::<_, String>(0),
        )
        .unwrap(),
        "blob"
    );
    clarity::vm::database::contract_migration::check_contract_storage(&conn).unwrap();
}

#[test]
fn explicit_migration_refuses_missing_files_and_unrelated_databases() {
    let file = DatabaseFile::new();
    assert!(migrate_standalone_contract_storage(file.path()).is_err());
    assert!(!file.0.exists());
    let conn = Connection::open(file.path()).unwrap();
    conn.execute_batch("CREATE TABLE unrelated(value TEXT)")
        .unwrap();
    drop(conn);
    let before = std::fs::read(&file.0).unwrap();
    assert!(migrate_standalone_contract_storage(file.path()).is_err());
    assert_eq!(std::fs::read(&file.0).unwrap(), before);
}
