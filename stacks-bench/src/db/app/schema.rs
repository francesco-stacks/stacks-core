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
    burn_block (id) {
        id -> BigInt,
        block_hash -> Binary,
        height -> BigInt,
    }
}

table! {
    stacks_block (id) {
        id -> BigInt,
        index_hash -> Binary,
        height -> BigInt,
        parent_stacks_block_id -> Nullable<BigInt>,
        burn_block_id -> BigInt,
    }
}

table! {
    stacks_tx (id) {
        id -> BigInt,
        stacks_block_id -> BigInt,
        tx_hash -> Binary,
        tx_type -> Text,
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

joinable!(chainstate -> network (network_id));
joinable!(epoch -> chainstate (chainstate_id));
joinable!(benchmark_run -> chainstate (chainstate_id));
joinable!(stacks_block -> burn_block (burn_block_id));
joinable!(stacks_tx -> stacks_block (stacks_block_id));
joinable!(stacks_block_stats -> benchmark_run (benchmark_run_id));
joinable!(stacks_block_stats -> stacks_block (stacks_block_id));
joinable!(stacks_tx_stats -> benchmark_run (benchmark_run_id));
joinable!(stacks_tx_stats -> stacks_tx (stacks_tx_id));

allow_tables_to_appear_in_same_query!(
    network,
    chainstate,
    epoch,
    burn_block,
    stacks_block,
    stacks_tx,
    benchmark_run,
    stacks_block_stats,
    stacks_tx_stats,
);