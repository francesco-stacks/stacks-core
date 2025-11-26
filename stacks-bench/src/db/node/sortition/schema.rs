use diesel::prelude::*;

table! {
    epochs (start_block_height, epoch_id) {
        start_block_height -> BigInt,
        end_block_height -> BigInt,
        epoch_id -> Integer,
        block_limit -> Text,
        network_epoch -> Integer
    }
}
