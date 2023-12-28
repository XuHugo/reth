use crate::error::{PrettyReqwestError, RpcError};
use alloy_primitives::{B256, U256};
use reqwest::StatusCode;
use reth_rpc_types::{engine::PayloadStatus, ExecutionPayloadV2};
use serde::{Deserialize, Serialize};
use std::convert::TryFrom;
use strum::IntoStaticStr;
use superstruct::superstruct;

pub mod auth;
pub mod http;
pub mod json_structures;

pub const LATEST_TAG: &str = "latest";

#[derive(Debug)]
pub enum ClRpcError {
    HttpClient(PrettyReqwestError),
    Auth(auth::Error),
    BadResponse(String),
    RequestFailed(String),
    InvalidExecutePayloadResponse(&'static str),
    JsonRpc(RpcError),
    Json(serde_json::Error),
    ServerMessage { code: i64, message: String },
    Eip155Failure,
    IsSyncing,
    // ExecutionBlockNotFound(ExecutionBlockHash),
    // ExecutionHeadBlockNotFound,
    // ParentHashEqualsBlockHash(ExecutionBlockHash),
    // PayloadIdUnavailable,
    // TransitionConfigurationMismatch,
    // PayloadConversionLogicFlaw,
    // DeserializeTransaction(ssz_types::Error),
    // DeserializeTransactions(ssz_types::Error),
    // DeserializeWithdrawals(ssz_types::Error),
    // BuilderApi(builder_client::Error),
    // IncorrectStateVariant,
    // RequiredMethodUnsupported(&'static str),
    // UnsupportedForkVariant(String),
    // BadConversion(String),
    // RlpDecoderError(rlp::DecoderError),
}

impl From<reqwest::Error> for ClRpcError {
    fn from(e: reqwest::Error) -> Self {
        if matches!(e.status(), Some(StatusCode::UNAUTHORIZED) | Some(StatusCode::FORBIDDEN)) {
            ClRpcError::Auth(auth::Error::InvalidToken)
        } else {
            ClRpcError::HttpClient(e.into())
        }
    }
}

impl From<serde_json::Error> for ClRpcError {
    fn from(e: serde_json::Error) -> Self {
        ClRpcError::Json(e)
    }
}

impl From<auth::Error> for ClRpcError {
    fn from(e: auth::Error) -> Self {
        ClRpcError::Auth(e)
    }
}

// impl From<builder_client::Error> for Error {
//     fn from(e: builder_client::Error) -> Self {
//         Error::BuilderApi(e)
//     }
// }

// impl From<rlp::DecoderError> for Error {
//     fn from(e: rlp::DecoderError) -> Self {
//         Error::RlpDecoderError(e)
//     }
// }

/// Representation of an exection block with enough detail to determine the terminal PoW block.
///
/// See `get_pow_block_hash_at_total_difficulty`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionBlock {
    #[serde(rename = "hash")]
    pub block_hash: B256,
    #[serde(rename = "number", with = "serde_utils::u64_hex_be")]
    pub block_number: u64,
    pub parent_hash: B256,
    pub total_difficulty: U256,
    #[serde(with = "serde_utils::u64_hex_be")]
    pub timestamp: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionPayloadWrapperV2 {
    /// Execution payload, which could be either V1 or V2
    ///
    /// V1 (_NO_ withdrawals) MUST be returned if the payload timestamp is lower than the Shanghai
    /// timestamp
    ///
    /// V2 (_WITH_ withdrawals) MUST be returned if the payload timestamp is greater or equal to
    /// the Shanghai timestamp
    pub execution_payload: ExecutionPayloadV2,
    /// The expected value to be received by the feeRecipient in wei
    pub block_value: U256,
}
