use std::fmt::{LowerHex, UpperHex};
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use stacks_bench::{Network, StacksBlockRef};

pub trait IndexerArgs {
    fn start_at(&self) -> Option<&StacksBlockRef>;
    fn end_at(&self) -> Option<&StacksBlockRef>;
    fn block_count(&self) -> Option<u32>;
    fn tip(&self) -> Option<&StacksBlockRef>;
    fn network(&self) -> Option<Network>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxIdArg([u8; 32]);

impl FromStr for TxIdArg {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = hex::decode(s).with_context(|| format!("invalid hex in txid '{s}'"))?;
        if bytes.len() != 32 {
            bail!(
                "invalid txid length: expected 32 bytes, got {} bytes",
                bytes.len()
            );
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Ok(TxIdArg(arr))
    }
}

impl std::fmt::Display for TxIdArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

impl LowerHex for TxIdArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in &self.0 {
            write!(f, "{:02x}", byte)?;
        }
        Ok(())
    }
}

impl UpperHex for TxIdArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in &self.0 {
            write!(f, "{:02X}", byte)?;
        }
        Ok(())
    }
}

impl TxIdArg {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}
