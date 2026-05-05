//! OutLayer Agent Custody client.
//!
//! Multi-chain wallet running inside TEE. Keys never leave the enclave.
//! Supports NEAR, Ethereum, Solana, Bitcoin — all via one API key.

use crate::error::Error;
use crate::types::*;
use crate::Result;

const BASE_URL: &str = "https://api.outlayer.fastnear.com";

/// OutLayer Agent Custody wallet client.
///
/// One wallet, multiple chains. Gasless cross-chain transfers via NEAR Intents.
#[derive(Clone)]
pub struct CustodyClient {
    http: reqwest::Client,
    api_key: String,
    near_account_id: String,
}

impl CustodyClient {
    // ─── Construction ──────────────────────────────────────────────────

    /// Register a new wallet. Returns a client with the API key.
    ///
    /// The API key is shown only once — store it after registration.
    pub async fn register() -> Result<Self> {
        let http = reqwest::Client::new();
        let resp = http
            .post(format!("{}/register", BASE_URL))
            .send()
            .await
            .map_err(Error::Http)?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Api(format!("Register failed ({}): {}", status, body)));
        }

        let reg: RegisterResponse = resp.json().await.map_err(Error::Http)?;
        let near_account_id = reg.near_account_id.clone();
        let api_key = reg.api_key.clone();

        Ok(Self {
            http,
            api_key,
            near_account_id,
        })
    }

    /// Create a client from an existing API key.
    pub fn from_api_key(api_key: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key: api_key.to_string(),
            near_account_id: String::new(),
        }
    }

    // ─── Identity ──────────────────────────────────────────────────────

    /// NEAR account ID (from registration).
    pub fn near_account_id(&self) -> &str {
        &self.near_account_id
    }

    /// Clone the client for use by MpcClient (shares the same API key).
    pub fn clone_for_mpc(&self) -> Self {
        Self {
            http: self.http.clone(),
            api_key: self.api_key.clone(),
            near_account_id: self.near_account_id.clone(),
        }
    }

    /// Get address for a specific chain.
    pub async fn address(&self, chain: &str) -> Result<String> {
        let resp: AddressResponse = self
            .get(&format!("/wallet/v1/address?chain={}", chain))
            .await?;
        Ok(resp.address)
    }

    /// Get all chain addresses at once.
    pub async fn all_addresses(&self) -> Result<Vec<(String, String)>> {
        let chains = ["near", "ethereum", "solana", "bitcoin"];
        let mut addresses = Vec::new();
        for chain in chains {
            match self.address(chain).await {
                Ok(addr) => addresses.push((chain.to_string(), addr)),
                Err(_) => continue, // Chain not supported for this wallet
            }
        }
        Ok(addresses)
    }

    // ─── Balances ──────────────────────────────────────────────────────

    /// Get native NEAR balance.
    pub async fn balance_near(&self) -> Result<String> {
        let resp: BalanceResponse = self.get("/wallet/v1/balance?chain=near").await?;
        Ok(resp.balance)
    }

    /// Get FT token balance (e.g., USDT, USDC).
    pub async fn balance_token(&self, token: &str) -> Result<String> {
        let resp: BalanceResponse = self
            .get(&format!(
                "/wallet/v1/balance?chain=near&token={}",
                token
            ))
            .await?;
        Ok(resp.balance)
    }

    /// Get balances for all tokens.
    pub async fn tokens(&self) -> Result<Vec<TokenInfo>> {
        let resp: Vec<TokenInfo> = self.get("/wallet/v1/tokens").await?;
        Ok(resp)
    }

    // ─── Transfers (NEAR native) ───────────────────────────────────────

    /// Transfer NEAR to another account.
    pub async fn transfer(&self, receiver_id: &str, amount: &str) -> Result<RequestResponse> {
        self.post(
            "/wallet/v1/transfer",
            &TransferRequest {
                receiver_id: receiver_id.to_string(),
                amount: amount.to_string(),
            },
        )
        .await
    }

    // ─── Contract Calls ────────────────────────────────────────────────

    /// Call a NEAR smart contract method.
    pub async fn call(
        &self,
        receiver_id: &str,
        method_name: &str,
        args: serde_json::Value,
        deposit: &str,
    ) -> Result<RequestResponse> {
        self.post(
            "/wallet/v1/call",
            &serde_json::json!({
                "receiver_id": receiver_id,
                "method_name": method_name,
                "args": args,
                "deposit": deposit,
            }),
        )
        .await
    }

    // ─── Token Swaps (via NEAR Intents) ────────────────────────────────

    /// Swap tokens. Example: wrap.near → usdt.tether-token.near
    pub async fn swap(
        &self,
        token_in: &str,
        token_out: &str,
        amount_in: &str,
    ) -> Result<RequestResponse> {
        self.post(
            "/wallet/v1/intents/swap",
            &SwapRequest {
                token_in: token_in.to_string(),
                token_out: token_out.to_string(),
                amount_in: amount_in.to_string(),
            },
        )
        .await
    }

    // ─── Cross-Chain Withdraw (NEAR Intents, gasless) ──────────────────

    /// Withdraw tokens to any chain. Gasless — no destination chain gas needed.
    ///
    /// Tokens must be in Intents balance first (use `swap` or `deposit_to_intents`).
    ///
    /// Examples:
    /// - `withdraw("bob.near", "1000000", "usdt.tether-token.near", "near")`
    /// - `withdraw("0x7f3a...", "500000", "usdt.tether-token.near", "ethereum")`
    /// - `withdraw("DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy", "200000", "usdt.tether-token.near", "solana")`
    pub async fn withdraw(
        &self,
        to: &str,
        amount: &str,
        token: &str,
        chain: &str,
    ) -> Result<RequestResponse> {
        self.post(
            "/wallet/v1/intents/withdraw",
            &WithdrawRequest {
                to: to.to_string(),
                amount: amount.to_string(),
                token: token.to_string(),
                chain: chain.to_string(),
            },
        )
        .await
    }

    /// Deposit tokens into Intents balance (required before cross-chain withdraw).
    pub async fn deposit_to_intents(
        &self,
        token: &str,
        amount: &str,
    ) -> Result<RequestResponse> {
        self.post(
            "/wallet/v1/intents/deposit",
            &serde_json::json!({
                "token": token,
                "amount": amount,
            }),
        )
        .await
    }

    /// Dry-run a cross-chain withdraw — checks fees and route without executing.
    pub async fn withdraw_dry_run(
        &self,
        to: &str,
        amount: &str,
        token: &str,
        chain: &str,
    ) -> Result<serde_json::Value> {
        self.post_value(
            "/wallet/v1/intents/withdraw/dry-run",
            &serde_json::json!({
                "to": to,
                "amount": amount,
                "token": token,
                "chain": chain,
            }),
        )
        .await
    }

    // ─── Signing ───────────────────────────────────────────────────────

    /// Sign a message using NEP-413 (NEAR standard).
    /// Use for authenticating to external services without on-chain tx.
    pub async fn sign_message(
        &self,
        message: &str,
        recipient: &str,
    ) -> Result<SignMessageResponse> {
        self.post(
            "/wallet/v1/sign-message",
            &SignMessageRequest {
                message: message.to_string(),
                recipient: recipient.to_string(),
                nonce: None,
            },
        )
        .await
    }

    // ─── Request Management ────────────────────────────────────────────

    /// Check status of a previous request.
    pub async fn request_status(&self, request_id: &str) -> Result<RequestEntry> {
        self.get(&format!("/wallet/v1/requests/{}", request_id))
            .await
    }

    /// List recent requests.
    pub async fn list_requests(&self) -> Result<Vec<RequestEntry>> {
        self.get("/wallet/v1/requests").await
    }

    // ─── Wallet Management ─────────────────────────────────────────────

    /// Delete wallet (irreversible). Withdraw all tokens first.
    pub async fn delete(&self, beneficiary: &str) -> Result<RequestResponse> {
        self.post(
            "/wallet/v1/delete",
            &serde_json::json!({
                "beneficiary": beneficiary,
                "chain": "near",
            }),
        )
        .await
    }

    /// View current policy (spending limits, whitelists, etc.).
    pub async fn policy(&self) -> Result<serde_json::Value> {
        self.get_value("/wallet/v1/policy").await
    }

    /// View audit log.
    pub async fn audit_log(&self) -> Result<serde_json::Value> {
        self.get_value("/wallet/v1/audit").await
    }

    // ─── Cross-Chain Send (high-level) ─────────────────────────────────

    /// Send tokens cross-chain via NEAR Intents.
    ///
    /// Handles the full pipeline:
    /// 1. Deposit tokens into Intents if needed
    /// 2. Withdraw to destination chain
    /// 3. Poll until complete
    ///
    /// `token` is the NEAR token contract (e.g. "wrap.near", "usdt.tether-token.near").
    /// `chain` is the destination chain ("ethereum", "solana", "bitcoin").
    /// `amount` is in smallest unit.
    pub async fn send_cross_chain(
        &self,
        to: &str,
        amount: &str,
        token: &str,
        chain: &str,
    ) -> crate::Result<CrossChainResult> {
        // Step 1: Deposit to Intents
        let deposit = self.deposit_to_intents(token, amount).await?;
        tracing::info!(
            request_id = %deposit.request_id,
            status = %deposit.status,
            "Deposited to Intents"
        );
        self.poll_request(&deposit.request_id, 30).await?;

        // Step 2: Withdraw to destination chain
        let withdraw = self.withdraw(to, amount, token, chain).await?;
        tracing::info!(
            request_id = %withdraw.request_id,
            status = %withdraw.status,
            "Submitted withdraw to {}",
            chain
        );
        self.poll_request(&withdraw.request_id, 60).await?;

        // Step 3: Build result
        let final_status = self.request_status(&withdraw.request_id).await?;
        Ok(CrossChainResult {
            chain: chain.to_string(),
            address: to.to_string(),
            amount: amount.to_string(),
            token: token.to_string(),
            tx_hash: final_status.tx_hash.clone(),
            request_id: withdraw.request_id.clone(),
        })
    }

    /// Poll a request until it reaches a terminal state.
    ///
    /// Terminal states: "completed", "failed", "expired".
    /// Polls every 2 seconds for up to `timeout_secs`.
    pub async fn poll_request(
        &self,
        request_id: &str,
        timeout_secs: u64,
    ) -> crate::Result<RequestEntry> {
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_secs(timeout_secs);

        loop {
            let entry = self.request_status(request_id).await?;
            match entry.status.as_str() {
                "completed" => return Ok(entry),
                "failed" | "expired" => {
                    return Err(Error::Api(format!(
                        "Request {} {}",
                        request_id, entry.status
                    )))
                }
                "pending_approval" => {
                    return Err(Error::Api(format!(
                        "Request {} pending_approval — requires manual approval",
                        request_id
                    )))
                }
                _ => {
                    if std::time::Instant::now() >= deadline {
                        return Err(Error::Api(format!(
                            "Request {} timed out after {}s (status: {})",
                            request_id, timeout_secs, entry.status
                        )));
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
        }
    }

    // ─── Internal HTTP helpers ─────────────────────────────────────────

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.api_key)
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let resp = self
            .http
            .get(format!("{}{}", BASE_URL, path))
            .header("Authorization", self.auth_header())
            .send()
            .await
            .map_err(Error::Http)?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Api(format!("GET {} failed ({}): {}", path, status, body)));
        }

        resp.json().await.map_err(Error::Http)
    }

    async fn get_value(&self, path: &str) -> Result<serde_json::Value> {
        self.get(path).await
    }

    async fn post<T: Serialize, R: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &T,
    ) -> Result<R> {
        let resp = self
            .http
            .post(format!("{}{}", BASE_URL, path))
            .header("Authorization", self.auth_header())
            .json(body)
            .send()
            .await
            .map_err(Error::Http)?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Api(format!("POST {} failed ({}): {}", path, status, body)));
        }

        resp.json().await.map_err(Error::Http)
    }

    async fn post_value(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
        self.post(path, body).await
    }
}

use serde::Serialize;
