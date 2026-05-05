//! agent-pay: Multi-chain payments for NEAR agents.
//!
//! Wraps OutLayer Agent Custody (multi-chain wallet in TEE) with direct
//! NEAR MPC Chain Signatures for cross-chain signing.
//!
//! - Hold funds on any chain (NEAR, ETH, BTC, SOL) via one wallet
//! - Pay workers cross-chain (gasless via NEAR Intents)
//! - Sign Solana transactions directly via MPC (for pay.sh/x402)
//! - Swap tokens between chains
//!
//! # Quick start
//!
//! ```no_run
//! use agent_pay::CustodyClient;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     // Register a new wallet
//!     let client = CustodyClient::register().await?;
//!     println!("NEAR: {}", client.near_account_id());
//!     println!("ETH:  {}", client.address("ethereum").await?);
//!     println!("SOL:  {}", client.address("solana").await?);
//!     Ok(())
//! }
//! ```

mod custody;
mod error;
mod mpc;
mod types;
mod x402;

pub use custody::CustodyClient;
pub use error::Error;
pub use mpc::{MpcClient, MPC_MAINNET, MPC_TESTNET, USDC_MINT, SOL_NATIVE};
pub use types::CrossChainResult;
pub use types::SendResult;
pub use types::*;
pub use x402::PayClient;

pub type Result<T> = std::result::Result<T, Error>;
