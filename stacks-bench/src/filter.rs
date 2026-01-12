use blockstack_lib::chainstate::stacks::{StacksTransaction, TransactionPayload};

#[derive(Debug, Clone)]
pub enum TxFilter {
    ContractCall,
}

impl TxFilter {
    pub fn matches(&self, tx: &StacksTransaction) -> bool {
        match self {
            TxFilter::ContractCall => matches!(tx.payload, TransactionPayload::ContractCall(..)),
        }
    }
}
