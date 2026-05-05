use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("API error: {0}")]
    Api(String),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("x402 payment failed: {0}")]
    X402(String),

    #[error("Insufficient {asset} balance: need {needed} lamports, have {available}")]
    InsufficientBalance {
        asset: &'static str,
        needed: u64,
        available: u64,
    },

    #[error("Policy violation: {0}")]
    Policy(String),

    #[error("Wallet not registered")]
    NotRegistered,

    #[error("{0}")]
    Other(#[from] anyhow::Error),
}
