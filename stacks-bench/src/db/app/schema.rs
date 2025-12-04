use diesel::prelude::*;

table! {
    network (id) {
        id -> Integer,
        name -> Text,
    }
}

table! {
    chainstate (id) {
        id -> Integer,
        network_id -> Integer,
        chain_id -> BigInt,
        tip_index_hash -> Binary,
        tip_height -> BigInt,
        epochs_hash -> Binary,
    }
}

table! {
    epoch (id) {
        id -> Integer,
        chainstate_id -> Integer,
        stacks_epoch_id -> Integer,
        network_epoch_id -> Integer,
        start_height -> BigInt,
        end_height -> BigInt,
        write_length_budget -> BigInt,
        write_count_budget -> BigInt,
        read_length_budget -> BigInt,
        read_count_budget -> BigInt,
        runtime_budget -> BigInt,
    }
}

table! {
    stacks_tx_type (id) {
        id -> Integer,
        name -> Text,
    }
}

table! {
    _staged_stacks_tx_type (name) {
        name -> Text,
    }
}

table! {
    principal (id) {
        id -> Integer,
        address -> Text,
    }
}

table! {
    _staged_principal (address) {
        address -> Text,
    }
}

table! {
    contract (id) {
        id -> Integer,
        issuer_principal_id -> Integer,
        name -> Text,
    }
}

table! {
    _staged_contract (issuer_address, name) {
        issuer_address -> Text,
        name -> Text,
    }
}

table! {
    burn_block (id) {
        id -> BigInt,
        block_hash -> Binary,
        block_hash_hex -> Text,
        height -> BigInt,
    }
}

table! {
    stacks_block (id) {
        id -> BigInt,
        index_hash -> Binary,
        index_hash_hex -> Text,
        block_hash -> Binary,
        block_hash_hex -> Text,
        height -> BigInt,
        parent_stacks_block_id -> Nullable<BigInt>,
        burn_block_id -> BigInt,
    }
}

table! {
    _staged_stacks_block (index_hash) {
        index_hash -> Binary,
        block_hash -> Binary,
        parent_index_hash -> Binary,
        height -> BigInt,
        burn_block_hash -> Binary,
        burn_block_height -> BigInt,
    }
}

table! {
    stacks_tx (id) {
        id -> BigInt,
        stacks_block_id -> BigInt,
        tx_hash -> Binary,
        tx_hash_hex -> Text,
        stacks_tx_type_id -> Integer,
        caller_principal_id -> Integer,
        contract_id -> Nullable<Integer>,
    }
}

table! {
    _staged_stacks_tx (block_index_hash, tx_hash) {
        block_index_hash -> Binary,
        tx_hash -> Binary,
        tx_type -> Text,
        caller_address -> Text,
        contract_issuer_address -> Nullable<Text>,
        contract_name -> Nullable<Text>,
    }
}

table! {
    benchmark_run (id) {
        id -> Integer,
        run_name -> Nullable<Text>,
        chainstate_id -> Integer,
        git_commit_hash -> Binary,
        start_time -> Timestamp,
        end_time -> Nullable<Timestamp>,
        args_json -> Text,
    }
}

table! {
    stacks_block_stats (id) {
        id -> BigInt,
        benchmark_run_id -> Integer,
        stacks_block_id -> BigInt,
        total_duration_us -> Integer,
        setup_duration_us -> Integer,
        execution_duration_us -> Integer,
        commit_duration_us -> Integer,
        commit_overhead_baseline_us -> Integer,
        clarity_write_length -> Integer,
        clarity_write_count -> Integer,
        clarity_read_length -> Integer,
        clarity_read_count -> Integer,
        clarity_runtime -> Integer,
    }
}

table! {
    stacks_tx_stats (id) {
        id -> BigInt,
        benchmark_run_id -> Integer,
        stacks_tx_id -> BigInt,
        duration_us -> Integer,
        estimated_commit_impact_us -> Integer,
        clarity_write_length -> Integer,
        clarity_write_count -> Integer,
        clarity_read_length -> Integer,
        clarity_read_count -> Integer,
        clarity_runtime -> Integer,
    }
}

table! {
    profiler_location (id) {
        id -> Integer,
        file -> Text,
        line -> Integer,
    }
}

table! {
    profiler_span (id) {
        id -> Integer,
        name -> Text,
    }
}

table! {
    profiler_record (id) {
        id -> Integer,
        benchmark_run_id -> Integer,
        parent_id -> Nullable<Integer>,
        profiler_span_id -> Integer,
        profiler_location_id -> Integer,
        child_index -> Integer,
        depth -> Integer,
        stacks_block_id -> Nullable<BigInt>,
        stacks_tx_id -> Nullable<BigInt>,
        wall_time_us -> BigInt,
        cpu_time_us -> BigInt,
        self_wall_time_us -> BigInt,
        self_cpu_time_us -> BigInt,
        call_count -> Integer,
    }
}

table! {
    chain_tip_cache (tip_index_hash, height) {
        tip_index_hash -> Binary,
        height -> BigInt,
        index_hash -> Binary,
    }
}

joinable!(chainstate -> network (network_id));
joinable!(epoch -> chainstate (chainstate_id));
joinable!(benchmark_run -> chainstate (chainstate_id));
joinable!(stacks_block -> burn_block (burn_block_id));
joinable!(stacks_tx -> stacks_block (stacks_block_id));
joinable!(stacks_tx -> stacks_tx_type (stacks_tx_type_id));
joinable!(stacks_tx -> principal (caller_principal_id));
joinable!(stacks_tx -> contract (contract_id));
joinable!(stacks_block_stats -> benchmark_run (benchmark_run_id));
joinable!(stacks_block_stats -> stacks_block (stacks_block_id));
joinable!(stacks_tx_stats -> benchmark_run (benchmark_run_id));
joinable!(stacks_tx_stats -> stacks_tx (stacks_tx_id));
joinable!(profiler_record -> benchmark_run (benchmark_run_id));
joinable!(profiler_record -> profiler_span (profiler_span_id));
joinable!(profiler_record -> profiler_location (profiler_location_id));
joinable!(profiler_record -> stacks_block (stacks_block_id));
joinable!(profiler_record -> stacks_tx (stacks_tx_id));

allow_tables_to_appear_in_same_query!(
    network,
    chainstate,
    epoch,
    stacks_tx_type,
    principal,
    contract,
    burn_block,
    stacks_block,
    stacks_tx,
    benchmark_run,
    stacks_block_stats,
    stacks_tx_stats,
    _staged_stacks_block,
    _staged_stacks_tx,
    _staged_stacks_tx_type,
    _staged_principal,
    _staged_contract,
    profiler_location,
    profiler_span,
    profiler_record,
    chain_tip_cache,
);
