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

//! SQLite pages are warm in these benchmarks; "cold" means an empty VM cache.
//! Run: cargo bench -p clarity --bench contract_loading -- --quick

use std::alloc::System;
use std::hint::black_box;
use std::time::{Duration, Instant};

use clarity::vm::ClarityVersion;
use clarity::vm::callables::FunctionDefinition;
use clarity::vm::contexts::{ContractContext, FunctionExecutionOptions, OwnedEnvironment};
use clarity::vm::costs::cost_functions::ClarityCostFunction;
use clarity::vm::costs::{CostTracker, ExecutionCost, LimitedCostTracker, runtime_cost};
use clarity::vm::database::contract_storage::split_contract;
use clarity::vm::database::{
    ClarityDatabase, ClarityDeserializable, ClarityExecutionCache, ClaritySerializable,
    ContractCache, MemoryBackingStore,
};
use clarity::vm::errors::VmExecutionError;
use clarity::vm::types::QualifiedContractIdentifier;
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use stacks_common::alloc_tracker::{TrackingAllocator, thread_allocated};
use stacks_common::consts::CHAIN_ID_TESTNET;
use stacks_common::types::StacksEpochId;

#[global_allocator]
static ALLOCATOR: TrackingAllocator<System> = TrackingAllocator { inner: System };

fn source(functions: usize, broad: bool) -> String {
    source_with_constants(functions, broad, false)
}

fn source_with_constants(functions: usize, broad: bool, large_constants: bool) -> String {
    let mut source = "(define-constant answer u42)\n".to_string();
    if large_constants {
        let item = format!("{{ index: u42, buffer: 0x{} }} ", "ab".repeat(256));
        source.push_str(&format!(
            "(define-constant payload (list {}))\n",
            item.repeat(256)
        ));
    }
    for i in 0..functions {
        source.push_str(&format!(
            "(define-private (f{i}) (+ answer u{i} u1 u2 u3 u4 u5 u6 u7 u8))\n"
        ));
    }
    if broad {
        source.push_str("(define-public (run) (ok (+ ");
        for i in 0..functions {
            source.push_str(&format!("(f{i}) "));
        }
        source.push_str(")))");
    } else {
        source.push_str("(define-public (run) (ok (f0)))");
    }
    source
}

fn deploy(store: &mut MemoryBackingStore, source: &str) -> QualifiedContractIdentifier {
    deploy_named(store, "bench", source)
}

fn deploy_named(
    store: &mut MemoryBackingStore,
    name: &str,
    source: &str,
) -> QualifiedContractIdentifier {
    let id = QualifiedContractIdentifier::local(name).unwrap();
    let mut db = store.as_clarity_db();
    db.begin();
    db.set_clarity_epoch_version(StacksEpochId::Epoch40)
        .unwrap();
    db.commit().unwrap();
    let mut env = OwnedEnvironment::new_free(false, CHAIN_ID_TESTNET, db, StacksEpochId::Epoch40);
    env.initialize_versioned_contract(id.clone(), ClarityVersion::Clarity2, source, None)
        .unwrap();
    id
}

// Mirror the previous no-cache DB path, including the epoch and legacy-size reads.
fn eager_load(db: &mut ClarityDatabase, id: &QualifiedContractIdentifier) -> ContractContext {
    let json = db
        .store
        .get_metadata(id, "benchmark-legacy")
        .unwrap()
        .unwrap();
    let mut context = ContractContext::deserialize(&json).unwrap();
    context
        .canonicalize_types(&db.get_clarity_epoch_version().unwrap())
        .unwrap();
    db.get_contract_size(id).unwrap();
    context
}

fn benchmarks(c: &mut Criterion) {
    let mut group = c.benchmark_group("contract_loading");
    group.sample_size(20);
    group.warm_up_time(Duration::from_millis(250));
    group.measurement_time(Duration::from_secs(1));
    for (count, broad, constants) in [
        (1, false, false),
        (100, false, false),
        (100, true, false),
        (100, false, true),
    ] {
        let label = format!(
            "{count}_{}_{}",
            if broad { "broad" } else { "sparse" },
            if constants {
                "large_constants"
            } else {
                "scalar"
            }
        );
        let mut store = MemoryBackingStore::new();
        let id = deploy(&mut store, &source_with_constants(count, broad, constants));
        let mut db = store.as_clarity_db();
        db.begin();
        let full = db.get_contract(&id).unwrap();
        let json = ClaritySerializable::serialize(&*full);
        db.store
            .insert_metadata(&id, "benchmark-legacy", &json)
            .unwrap();
        db.commit().unwrap();
        db.begin();

        let before = thread_allocated();
        let eager = ContractContext::deserialize(&json).unwrap();
        let eager_bytes = thread_allocated().net_allocated(&before);
        drop(eager);
        let before = thread_allocated();
        let partial = db.get_contract_for_function(&id, "run").unwrap();
        let partial_bytes = thread_allocated().net_allocated(&before);
        eprintln!(
            "{label}: retained heap legacy={eager_bytes}, split={partial_bytes}; loaded functions={}/{}",
            partial.loaded_functions().len(),
            full.loaded_functions().len()
        );
        drop(partial);

        group.bench_function(BenchmarkId::new("legacy_load", &label), |b| {
            b.iter(|| black_box(eager_load(&mut db, &id)))
        });
        group.bench_function(BenchmarkId::new("split_load", &label), |b| {
            b.iter(|| black_box(db.get_contract_for_function(&id, "run").unwrap()))
        });
        // Component measurement only: this is not a complete legacy invocation
        // and must not be compared as a warm transaction speedup.
        let eager_cache = std::collections::HashMap::from([(id.clone(), full.clone())]);
        group.bench_function(BenchmarkId::new("legacy_cache_lookup_only", &label), |b| {
            b.iter(|| black_box(eager_cache.get(black_box(&id)).unwrap().clone()))
        });
        group.bench_function(BenchmarkId::new("body_clone", &label), |b| {
            b.iter(|| black_box(full.lookup_function("run").unwrap().body().clone()))
        });
        group.bench_function(BenchmarkId::new("borrow_lookup", &label), |b| {
            b.iter(|| black_box(full.lookup_function(black_box("run")).unwrap()))
        });
        db.roll_back().unwrap();
        drop(db);

        let mut cache = ClarityExecutionCache::default();
        let mut db = store.as_clarity_db().with_cache(&mut cache);
        db.begin();
        db.get_contract_for_function(&id, "run").unwrap();
        group.bench_function(BenchmarkId::new("split_cached_load", &label), |b| {
            b.iter(|| black_box(db.get_contract_for_function(&id, "run").unwrap()))
        });
        db.roll_back().unwrap();
        drop(db);
        let exact_budget = cache.contracts.total_weight();
        for (scenario, budget) in [
            ("warm_exact_budget", exact_budget),
            ("eviction_pressure", exact_budget / 2),
        ] {
            let mut cache = ClarityExecutionCache {
                contracts: ContractCache::new(budget),
            };
            let before_fill = thread_allocated();
            for pass in 0..2 {
                let before = (
                    store.data_reads,
                    store.block_height_reads,
                    store.metadata_resolutions,
                );
                let mut db = store.as_clarity_db().with_cache(&mut cache);
                db.begin();
                black_box(db.get_contract_for_function(&id, "run").unwrap());
                db.roll_back().unwrap();
                drop(db);
                eprintln!(
                    "{label}/{scenario}/pass-{pass}: data_reads={}, block_height_calls={}, resolutions={}, encoded_cache_bytes={}",
                    store.data_reads - before.0,
                    store.block_height_reads - before.1,
                    store.metadata_resolutions - before.2,
                    cache.contracts.total_weight()
                );
            }
            eprintln!(
                "{label}/{scenario}: retained cache heap={}, encoded cache bytes={}",
                thread_allocated().net_allocated(&before_fill),
                cache.contracts.total_weight()
            );
            let mut db = store.as_clarity_db().with_cache(&mut cache);
            db.begin();
            group.bench_function(BenchmarkId::new(scenario, &label), |b| {
                b.iter(|| black_box(db.get_contract_for_function(&id, "run").unwrap()))
            });
            db.roll_back().unwrap();
            drop(db);
        }
        group.bench_function(BenchmarkId::new("eager_transaction", &label), |b| {
            b.iter(|| {
                let db = store.as_clarity_db();
                let mut env =
                    OwnedEnvironment::new_free(false, CHAIN_ID_TESTNET, db, StacksEpochId::Epoch40);
                black_box(
                    env.execute_in_env(
                        id.issuer.clone().into(),
                        None,
                        None,
                        |state, invocation| -> Result<_, VmExecutionError> {
                            let size = state.global_context.database.get_contract_size(&id)?;
                            runtime_cost(ClarityCostFunction::LoadContract, state, size)?;
                            state.add_memory(size)?;
                            let contract = eager_load(&mut state.global_context.database, &id);
                            let function = contract.lookup_function("run").unwrap();
                            let function_id = function.get_identifier();
                            state.call_stack.insert(&function_id, true);
                            let result = state.execute_function_as_transaction(
                                invocation,
                                function,
                                &[],
                                FunctionExecutionOptions::default().with_next_contract(&contract),
                            );
                            state.call_stack.remove(&function_id, true)?;
                            state.drop_memory(size)?;
                            result
                        },
                    )
                    .unwrap(),
                )
            })
        });
        group.bench_function(BenchmarkId::new("split_transaction", &label), |b| {
            b.iter(|| {
                let mut cache = ClarityExecutionCache::default();
                let db = store.as_clarity_db().with_cache(&mut cache);
                let mut env =
                    OwnedEnvironment::new_free(false, CHAIN_ID_TESTNET, db, StacksEpochId::Epoch40);
                black_box(
                    env.execute_transaction(id.issuer.clone().into(), None, id.clone(), "run", &[])
                        .unwrap(),
                )
            })
        });
    }
    group.finish();
    trait_dispatch(c);

    let mut store = MemoryBackingStore::new();
    let id = deploy(&mut store, &source(100, true));
    let mut db = store.as_clarity_db();
    db.begin();
    let contract = db.get_contract(&id).unwrap();
    // A synthetic in-memory migration sample; this measures conversion work,
    // not production disk throughput or whole-node upgrade time.
    let legacy = ClaritySerializable::serialize(&*contract);
    let conn = clarity::vm::database::SqliteConnection::memory().unwrap();
    for block in 0..100 {
        conn.execute(
            "INSERT INTO metadata_table (key, blockhash, value) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                format!("clr-meta::{id}::vm-metadata::9::contract"),
                format!("{block:064x}"),
                legacy
            ],
        )
        .unwrap();
    }
    let start = Instant::now();
    let converted =
        clarity::vm::database::contract_migration::migrate_contract_storage(&conn).unwrap();
    eprintln!(
        "synthetic in-memory migration: {converted} contract versions, {} legacy bytes, {:?}",
        legacy.len() * 100,
        start.elapsed()
    );
    let (_, functions) = split_contract(&contract).unwrap();
    let record: &FunctionDefinition = functions.get("run").unwrap();
    let json = ClaritySerializable::serialize(record);
    let binary = clarity::vm::database::contract_codec::encode_function(record).unwrap();
    assert_eq!(
        ClaritySerializable::serialize(
            &clarity::vm::database::contract_codec::decode_function(&binary).unwrap()
        ),
        json
    );
    let mut codecs = c.benchmark_group("function_decode");
    codecs.bench_function("json", |b| {
        b.iter(|| black_box(serde_json::from_str::<FunctionDefinition>(black_box(&json)).unwrap()))
    });
    codecs.bench_function("postcard", |b| {
        b.iter(|| {
            black_box(
                clarity::vm::database::contract_codec::decode_function(black_box(&binary)).unwrap(),
            )
        })
    });
    codecs.finish();
    db.roll_back().unwrap();
}

// Metered end-to-end dynamic dispatch, with fresh per-transaction caches.
// This measures both explicit metadata-only and implicit signature preflight.
// Repeating eight transactions models cache lifetime, not MARF block processing.
fn trait_dispatch(c: &mut Criterion) {
    use clarity::vm::{SymbolicExpression, Value};
    let mut store = MemoryBackingStore::new();
    deploy_named(
        &mut store,
        "definition",
        "(define-trait t ((run () (response uint uint))))",
    );
    let caller = deploy_named(
        &mut store,
        "caller",
        "(use-trait t .definition.t) (define-public (call (target <t>)) (contract-call? target run))",
    );
    let mut group = c.benchmark_group("trait_dispatch");
    group.sample_size(20);
    group.warm_up_time(Duration::from_millis(250));
    group.measurement_time(Duration::from_secs(1));
    for (name, declaration) in [("explicit", "(impl-trait .definition.t)"), ("implicit", "")] {
        let id = deploy_named(
            &mut store,
            name,
            &format!("{declaration} {}", source(100, false)),
        );
        let args = [SymbolicExpression::atom_value(Value::Principal(id.into()))];
        for count in [1, 8] {
            group.bench_function(BenchmarkId::new(name, count), |b| {
                b.iter(|| {
                    for _ in 0..count {
                        let mut cache = ClarityExecutionCache::default();
                        let db = store.as_clarity_db().with_cache(&mut cache);
                        let mut env = OwnedEnvironment::new_cost_limited(
                            false,
                            CHAIN_ID_TESTNET,
                            db,
                            LimitedCostTracker::new_with_limit(
                                StacksEpochId::Epoch40,
                                ExecutionCost::max_value(),
                            ),
                            StacksEpochId::Epoch40,
                        );
                        black_box(
                            env.execute_transaction(
                                caller.issuer.clone().into(),
                                None,
                                caller.clone(),
                                "call",
                                &args,
                            )
                            .unwrap(),
                        );
                        black_box(env.get_cost_total());
                    }
                });
            });
        }
    }
    group.finish();
}

criterion_group!(benches, benchmarks);
criterion_main!(benches);
