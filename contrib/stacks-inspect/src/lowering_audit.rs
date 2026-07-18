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

//! `clarity-lowering-audit`: walk every deployed contract in a Clarity
//! metadata database and run the typed-IR lowering pass (`clarity::vm::lowered`)
//! over it, reporting:
//!
//! - totality on real data: deserialization/canonicalization failures and
//!   panics (both must be zero for the lowered evaluator to be defaultable);
//! - typed coverage: what fraction of AST nodes execute on the typed path vs
//!   falling back to `Opaque` (legacy eval);
//! - an aggregate histogram of `Opaque` head names — the data-driven priority
//!   list for which form families to type next.
//!
//! Contracts are canonicalized at the latest epoch before lowering, modeling
//! what `read_contract` would produce at today's chain tip.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};

use clarity::vm::contexts::ContractContext;
use clarity::vm::database::{ClarityDeserializable, SqliteConnection};
use clarity::vm::lowered::{LExpr, lower_contract};
use clarity::vm::representations::{SymbolicExpression, SymbolicExpressionType};
use rusqlite::OpenFlags;
use stacks_common::types::StacksEpochId;
use stackslib::util_lib::db::sqlite_open;

/// The metadata key under which a contract's serialized `ContractContext` is
/// stored (`vm-metadata::{StoreType::Contract}::contract`).
const CONTRACT_METADATA_KEY: &str = "vm-metadata::9::contract";

#[derive(Debug)]
enum AuditError {
    Sqlite(rusqlite::Error),
}

impl From<rusqlite::Error> for AuditError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}

#[derive(Default)]
struct Totals {
    contracts: u64,
    duplicate_contract_rows: u64,
    functions: u64,
    total_ast_nodes: u64,
    typed_nodes: u64,
    opaque_nodes: u64,
    /// AST nodes inside Opaque subtrees (executed via legacy eval).
    opaque_ast_nodes: u64,
    fully_typed_contracts: u64,
    deser_failures: Vec<(String, String)>,
    panics: Vec<String>,
    opaque_heads: HashMap<String, u64>,
}

struct ContractStats {
    functions: u64,
    total_ast_nodes: u64,
    typed_nodes: u64,
    opaque_nodes: u64,
    opaque_ast_nodes: u64,
    opaque_heads: HashMap<String, u64>,
}

pub fn run(clarity_db_path: &str) -> Result<(), String> {
    let conn = sqlite_open(clarity_db_path, OpenFlags::SQLITE_OPEN_READ_ONLY, false)
        .map_err(|e| format!("Failed to open {clarity_db_path}: {e}"))?;

    let mut totals = Totals::default();
    let mut seen_contract_ids = std::collections::HashSet::new();
    let mut rows_scanned: u64 = 0;

    SqliteConnection::visit_metadata_rows::<AuditError, _>(&conn, |row| {
        rows_scanned += 1;
        let Some((contract_id, meta_key)) = SqliteConnection::parse_metadata_key(row.key) else {
            // Not this tool's job to police key formats; skip.
            return Ok(());
        };
        if meta_key != CONTRACT_METADATA_KEY {
            return Ok(());
        }
        totals.contracts += 1;
        if !seen_contract_ids.insert(contract_id.to_string()) {
            totals.duplicate_contract_rows += 1;
        }
        match audit_contract_blob(row.value) {
            Ok(stats) => {
                totals.functions += stats.functions;
                totals.total_ast_nodes += stats.total_ast_nodes;
                totals.typed_nodes += stats.typed_nodes;
                totals.opaque_nodes += stats.opaque_nodes;
                totals.opaque_ast_nodes += stats.opaque_ast_nodes;
                if stats.opaque_nodes == 0 {
                    totals.fully_typed_contracts += 1;
                }
                for (head, n) in stats.opaque_heads {
                    *totals.opaque_heads.entry(head).or_insert(0) += n;
                }
            }
            Err(AuditFailure::Deser(e)) => {
                totals.deser_failures.push((contract_id.to_string(), e));
            }
            Err(AuditFailure::Panic) => {
                totals.panics.push(contract_id.to_string());
            }
        }
        Ok(())
    })
    .map_err(|AuditError::Sqlite(e)| format!("sqlite error during scan: {e}"))?;

    report(&totals, rows_scanned);

    if totals.panics.is_empty() && totals.deser_failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} panics, {} deserialization failures",
            totals.panics.len(),
            totals.deser_failures.len()
        ))
    }
}

enum AuditFailure {
    Deser(String),
    Panic,
}

fn audit_contract_blob(value: &str) -> Result<ContractStats, AuditFailure> {
    catch_unwind(AssertUnwindSafe(|| {
        let mut contract_context = ContractContext::deserialize(value)
            .map_err(|e| AuditFailure::Deser(format!("deserialize: {e:?}")))?;
        contract_context
            .canonicalize_types(&StacksEpochId::latest())
            .map_err(|e| AuditFailure::Deser(format!("canonicalize: {e:?}")))?;

        let lowered = lower_contract(&contract_context);

        let mut stats = ContractStats {
            functions: lowered.functions.len() as u64,
            total_ast_nodes: contract_context
                .functions
                .values()
                .map(|f| ast_node_count(f.get_body()))
                .sum(),
            typed_nodes: 0,
            opaque_nodes: 0,
            opaque_ast_nodes: 0,
            opaque_heads: HashMap::new(),
        };
        for body in lowered.functions.values() {
            body.walk(&mut |node| match node {
                LExpr::Opaque(expr) => {
                    stats.opaque_nodes += 1;
                    stats.opaque_ast_nodes += ast_node_count(expr);
                    *stats.opaque_heads.entry(opaque_head(expr)).or_insert(0) += 1;
                }
                _ => stats.typed_nodes += 1,
            });
        }
        Ok(stats)
    }))
    .map_err(|_| AuditFailure::Panic)?
}

/// The head name of an un-lowered application, or a placeholder for
/// non-application expressions.
fn opaque_head(expr: &SymbolicExpression) -> String {
    if let SymbolicExpressionType::List(children) = &expr.expr {
        match children.split_first() {
            Some((head, _)) => head
                .match_atom()
                .map(|name| name.to_string())
                .unwrap_or_else(|| "<non-atom-head>".to_string()),
            None => "<empty-application>".to_string(),
        }
    } else {
        "<non-application>".to_string()
    }
}

fn ast_node_count(expr: &SymbolicExpression) -> u64 {
    let mut n = 1;
    if let SymbolicExpressionType::List(children) = &expr.expr {
        for child in children {
            n += ast_node_count(child);
        }
    }
    n
}

fn report(totals: &Totals, rows_scanned: u64) {
    println!("== clarity-lowering-audit ==");
    println!("metadata rows scanned:     {rows_scanned}");
    println!("contract blobs audited:    {}", totals.contracts);
    println!(
        "  duplicate contract ids:  {} (fork/stale rows; stats count every blob)",
        totals.duplicate_contract_rows
    );
    println!("functions lowered:         {}", totals.functions);
    println!(
        "fully-typed contracts:     {} ({:.1}%)",
        totals.fully_typed_contracts,
        pct(totals.fully_typed_contracts, totals.contracts)
    );
    println!("typed IR nodes:            {}", totals.typed_nodes);
    println!(
        "opaque fallback nodes:     {} (covering {} AST nodes)",
        totals.opaque_nodes, totals.opaque_ast_nodes
    );
    println!(
        "typed-path AST coverage:   {:.1}% ({} of {} AST nodes execute typed)",
        pct(
            totals
                .total_ast_nodes
                .saturating_sub(totals.opaque_ast_nodes),
            totals.total_ast_nodes
        ),
        totals
            .total_ast_nodes
            .saturating_sub(totals.opaque_ast_nodes),
        totals.total_ast_nodes
    );
    println!("panics during lowering:    {}", totals.panics.len());
    for c in &totals.panics {
        println!("  PANIC: {c}");
    }
    println!("deserialization failures:  {}", totals.deser_failures.len());
    for (c, e) in &totals.deser_failures {
        println!("  FAIL: {c}: {e}");
    }

    let mut heads: Vec<(&String, &u64)> = totals.opaque_heads.iter().collect();
    heads.sort_by(|a, b| b.1.cmp(a.1));
    println!("top opaque heads (typed-coverage priority list):");
    for (head, n) in heads.iter().take(40) {
        println!("  {n:>10}  {head}");
    }
}

fn pct(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        100.0 * part as f64 / whole as f64
    }
}

#[cfg(test)]
mod test {
    use clarity::vm::ClarityVersion;
    use clarity::vm::database::ClaritySerializable;
    use clarity::vm::types::QualifiedContractIdentifier;

    use super::*;

    #[test]
    fn contract_metadata_key_matches_clarity() {
        use clarity::vm::database::ClarityDatabase;
        use clarity::vm::database::clarity_db::StoreType;
        assert_eq!(
            CONTRACT_METADATA_KEY,
            ClarityDatabase::make_metadata_key(StoreType::Contract, "contract")
        );
    }

    #[test]
    fn audits_a_serialized_contract_blob() {
        let contract_context = ContractContext::new(
            QualifiedContractIdentifier::transient(),
            ClarityVersion::latest(),
        );
        let blob = contract_context.serialize();
        let stats = audit_contract_blob(&blob)
            .map_err(|_| "audit failed")
            .unwrap();
        assert_eq!(stats.functions, 0);
        assert_eq!(stats.opaque_nodes, 0);
    }

    #[test]
    fn garbage_blob_is_a_deser_failure_not_a_panic() {
        match audit_contract_blob("{not json") {
            Err(AuditFailure::Deser(_)) => {}
            _ => panic!("expected deser failure"),
        }
    }
}
