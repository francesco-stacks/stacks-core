use blockstack_lib::burnchains::Txid;
use blockstack_lib::chainstate::stacks::{StacksTransaction, TransactionPayload};

#[derive(Debug, Clone)]
pub enum TxFilter {
    ContractCall,
    Txid(Txid),
}

impl TxFilter {
    pub fn matches(&self, tx: &StacksTransaction) -> bool {
        match self {
            TxFilter::ContractCall => matches!(tx.payload, TransactionPayload::ContractCall(..)),
            TxFilter::Txid(target) => tx.txid() == *target,
        }
    }
}
