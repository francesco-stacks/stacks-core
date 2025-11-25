use anyhow::{Result, bail};
use diesel::prelude::*;

use crate::Network;

#[derive(Queryable, Debug)]
#[diesel(table_name = db_config)]
pub struct DbConfig {
    pub version: i32,
    pub mainnet: bool,
    pub chain_id: i32,
}

impl DbConfig {
    /// Asserts that this database configuration is compatible with the target network.
    pub fn assert_matches_network(&self, network: Network) -> Result<()> {
        let expected_mainnet = network.is_mainnet();
        let expected_chain_id = network.to_chain_id();

        if self.mainnet != expected_mainnet {
            bail!(
                "Network mismatch: CLI specified {}, but DB is configured for {}",
                network,
                if self.mainnet { "mainnet" } else { "testnet/regtest" }
            );
        }

        // Cast i32 from DB to u32 for comparison
        let db_chain_id = self.chain_id as u32;
        if db_chain_id != expected_chain_id {
            bail!(
                "Chain ID mismatch: CLI expects {} (0x{:x}), but DB has {} (0x{:x})",
                expected_chain_id, expected_chain_id, db_chain_id, db_chain_id
            );
        }

        Ok(())
    }

    pub fn is_mainnet(&self) -> bool {
        self.mainnet
    }

    pub fn chain_id(&self) -> u32 {
        self.chain_id as u32
    }
}