use reth_db::models::consensus::ConsensusBytes;
use reth_interfaces::provider::ProviderResult;
use reth_primitives::{BlockNumber, B256};

/// Client trait for getting important block numbers (such as the latest block number), converting
/// block hashes to numbers, and fetching a block hash from its block number.
///
/// This trait also supports fetching block hashes and block numbers from a [BlockHashOrNumber].
#[auto_impl::auto_impl(&, Arc)]
pub trait ConsensusNumberReader: Send + Sync {
    /// Returns the best block number in the chain.
    fn last_consensus_number(&self) -> ProviderResult<BlockNumber>;

    /// Gets the `BlockNumber` for the given hash. Returns `None` if no block with this hash exists.
    fn consensus_number(&self, hash: B256) -> ProviderResult<Option<BlockNumber>>;

    /// Gets the `BlockNumber` for the given hash. Returns `None` if no block with this hash exists.
    fn consensus_content(&self, hash: B256) -> ProviderResult<Option<ConsensusBytes>>;
}

/// Client trait for getting important block numbers (such as the latest block number), converting
/// block hashes to numbers, and fetching a block hash from its block number.
///
/// This trait also supports fetching block hashes and block numbers from a [BlockHashOrNumber].
#[auto_impl::auto_impl(&, Arc)]
pub trait ConsensusNumberWriter: Send + Sync {
    /// Gets the `BlockNumber` for the given hash. Returns `None` if no block with this hash exists.
    fn save_consensus_number(&self, hash: B256, num: BlockNumber) -> ProviderResult<bool>;

    /// Gets the `BlockNumber` for the given hash. Returns `None` if no block with this hash exists.
    fn save_consensus_content(&self, hash: B256, ct: ConsensusBytes) -> ProviderResult<bool>;
}
