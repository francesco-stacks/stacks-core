use diesel::prelude::*;

table! {
    db_config (version) {
        version -> Integer,
        mainnet -> Bool,
        chain_id -> Integer
    }
}
