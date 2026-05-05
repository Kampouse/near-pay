use serde::{Deserialize, Serialize};

/// Chain identifiers for multi-chain operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Chain {
    Near,
    Ethereum,
    Solana,
    Bitcoin,
}

impl Chain {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Near => "near",
            Self::Ethereum => "ethereum",
            Self::Solana => "solana",
            Self::Bitcoin => "bitcoin",
        }
    }
}

/// Response from wallet registration.
#[derive(Debug, Deserialize)]
pub struct RegisterResponse {
    pub api_key: String,
    pub near_account_id: String,
    pub handoff_url: String,
}

/// Address response for a chain.
#[derive(Debug, Deserialize)]
pub struct AddressResponse {
    pub address: String,
    pub chain: String,
}

/// Balance response.
#[derive(Debug, Deserialize)]
pub struct BalanceResponse {
    pub balance: String,
    pub chain: String,
    #[serde(default)]
    pub token: Option<String>,
}

/// Transfer request.
#[derive(Debug, Serialize)]
pub struct TransferRequest {
    pub receiver_id: String,
    pub amount: String,
}

/// Generic request response (transfer, call, swap, withdraw).
#[derive(Debug, Deserialize)]
pub struct RequestResponse {
    pub request_id: String,
    pub status: String,
    #[serde(default)]
    pub tx_hash: Option<String>,
    /// Function call result (present for contract calls that return data).
    /// e.g. MPC sign returns { "scheme": "Ed25519", "signature": [u8; 64] }
    #[serde(default)]
    pub result: serde_json::Value,
}

/// Swap request.
#[derive(Debug, Serialize)]
pub struct SwapRequest {
    pub token_in: String,
    pub token_out: String,
    pub amount_in: String,
}

/// Cross-chain withdraw request.
#[derive(Debug, Serialize)]
pub struct WithdrawRequest {
    /// Destination address (any chain).
    pub to: String,
    /// Amount in smallest unit.
    pub amount: String,
    /// Token contract or identifier.
    pub token: String,
    /// Destination chain.
    pub chain: String,
}

/// NEP-413 sign message request.
#[derive(Debug, Serialize)]
pub struct SignMessageRequest {
    pub message: String,
    pub recipient: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
}

/// NEP-413 sign message response.
#[derive(Debug, Deserialize)]
pub struct SignMessageResponse {
    pub account_id: String,
    pub public_key: String,
    pub signature: String,
    pub nonce: String,
}

/// Token info from /tokens endpoint.
#[derive(Debug, Deserialize)]
pub struct TokenInfo {
    pub token_id: String,
    pub symbol: String,
    pub decimals: u8,
    pub balance: String,
}

/// Request status entry.
#[derive(Debug, Deserialize)]
pub struct RequestEntry {
    pub request_id: String,
    pub status: String,
    #[serde(default)]
    pub tx_hash: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
}

/// x402 payment challenge from a 402 response.
#[derive(Debug, Deserialize)]
pub struct X402Challenge {
    /// Payment version (x402 or MPP).
    pub version: String,
    /// The payment token contract.
    pub token: Option<String>,
    /// Amount to pay.
    pub amount: Option<String>,
    /// Payment recipient.
    pub pay_to: Option<String>,
    /// Maximum fee for the payment.
    pub max_fee: Option<String>,
    /// Network/chain identifier.
    pub network: Option<String>,
    /// Description of what's being paid for.
    pub description: Option<String>,
    /// HTTP resource being paid for.
    pub resource: Option<String>,
    /// Raw WWW-Authenticate header value.
    pub raw_header: String,
}

/// x402 payment proof to attach to retry request.
#[derive(Debug, Serialize)]
pub struct PaymentProof {
    pub x_payment: String,
}

/// Response after successful x402-paid API call.
#[derive(Debug)]
pub struct PaidResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response body.
    pub body: String,
    /// Amount paid.
    pub amount_paid: String,
    /// Token used for payment.
    pub token: String,
}

/// Result of a cross-chain transfer.
#[derive(Debug, Clone)]
pub struct CrossChainResult {
    /// Destination chain.
    pub chain: String,
    /// Destination address.
    pub address: String,
    /// Amount transferred (smallest unit).
    pub amount: String,
    /// Token identifier on the destination chain.
    pub token: String,
    /// On-chain tx hash on the destination chain (if available).
    pub tx_hash: Option<String>,
    /// OutLayer request ID for tracking.
    pub request_id: String,
}

/// Unified send result — one type for all chains.
#[derive(Debug, Clone)]
pub struct SendResult {
    /// Destination chain.
    pub chain: String,
    /// Destination address.
    pub address: String,
    /// Amount sent (smallest unit).
    pub amount: String,
    /// Token/mint identifier.
    pub token: String,
    /// On-chain tx hash.
    pub tx_hash: String,
}
