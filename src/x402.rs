//! Dual-protocol 402 payment client for NEAR agents.
//!
//! Handles both payment protocols transparently:
//!
//! **MPP** (IETF standard — `mpp` crate):
//! - Challenge: `WWW-Authenticate: Payment ...` header
//! - Credential: `Authorization: Payment ...` header
//! - Used by: pay.sh, solana-mpp servers, mpp.dev services
//!
//! **x402** (Coinbase — hand-rolled):
//! - Challenge: JSON body with `x402Version`, `accepts` array
//! - Credential: `X-Payment` header with base64-encoded signed payload
//! - Used by: Coinbase x402 servers, facilitator-based flows
//!
//! Both use the same MPC signer backend (NEAR → Solana chain signatures).
//!
//! Two address spaces:
//! - OutLayer custody address: holds NEAR funds, does swaps/withdraws
//! - MPC-derived address: controlled by (NEAR account + path), signs Solana txs

use base64::Engine;
use mpp::client::PaymentProvider;
use mpp::protocol::core::{PaymentChallenge, PaymentCredential, PaymentPayload as MppPayload};
use serde::{Deserialize, Serialize};

use crate::custody::CustodyClient;
use crate::error::Error;
use crate::mpc::MpcClient;
use crate::types::*;
use crate::Result;

/// Derivation path for the payment address on Solana.
const PAY_SOL_PATH: &str = "solana-pay-0";

// ─── Shared MPC payment execution ───────────────────────────────────

/// Shared MPC payment execution — builds tx, signs via MPC, relays.
///
/// Routes to SOL or SPL based on `asset`:
/// - `SOL_NATIVE` (all 1s) → native SystemProgram transfer
/// - anything else → SPL Token transfer (asset = mint address)
/// Result of building + signing a Solana payment.
/// Used to construct the Solana Charge credential.
struct SignedPayment {
    /// Base64-encoded serialized signed transaction (for pull mode credential).
    signed_tx_b64: String,
    /// Base58-encoded transaction signature (for push mode credential).
    tx_signature: String,
}

/// Build, sign, and optionally relay a Solana payment.
///
/// If `relay` is true (push mode), broadcasts to Solana RPC.
/// If `relay` is false (pull mode), returns the signed tx without broadcasting.
///
/// Returns the signed tx bytes (base64) and the tx signature (base58).
async fn sign_sol_payment(
    mpc: &MpcClient,
    _custody: &CustodyClient,
    sol_addr: &str,
    pay_to: &str,
    amount: u64,
    asset: &str,
    decimals: Option<u8>,
    fee_payer: Option<&str>,
    relay: bool,
) -> Result<SignedPayment> {
    // Balance gate: check before building tx
    let balance = mpc.sol_balance(sol_addr).await?;
    let needed = if asset == crate::mpc::SOL_NATIVE {
        amount + 10_000_000 // payment + rent reserve
    } else {
        10_000_000 // SPL transfer only needs gas
    };
    if balance < needed {
        let asset_name: &'static str = if asset == crate::mpc::SOL_NATIVE {
            "SOL"
        } else {
            "SPL"
        };
        return Err(Error::InsufficientBalance {
            asset: asset_name,
            needed,
            available: balance,
        });
    }

    // Build the transfer instruction
    let tx = if asset == crate::mpc::SOL_NATIVE {
        mpc.build_sol_transfer(sol_addr, pay_to, amount).await?
    } else {
        mpc.build_spl_transfer_checked(sol_addr, pay_to, asset, amount, decimals.unwrap_or(6)).await?
    };

    // If fee payer is provided (pull mode), set the fee payer on the tx
    // The server will co-sign with its fee payer key before broadcasting
    let tx = if let Some(_fp) = fee_payer {
        // TODO: set tx.message.account_keys[0] = fee_payer
        // For now the server handles this
        tx
    } else {
        tx
    };

    let signature = mpc.sign_transaction(&tx, PAY_SOL_PATH).await?;
    let signed_bytes = mpc.finalize_transaction(&tx, sol_addr, &signature)?;

    // The tx signature is the first (only) signature on the signed tx
    let tx_sig = bs58::encode(&signature).into_string();
    let signed_b64 = base64::engine::general_purpose::STANDARD.encode(&signed_bytes);

    if relay {
        mpc.relay_to_solana(&signed_bytes).await?;
    }

    Ok(SignedPayment {
        signed_tx_b64: signed_b64,
        tx_signature: tx_sig,
    })
}

/// Legacy helper: build, sign, relay — returns tx hash only.
async fn execute_sol_payment(
    mpc: &MpcClient,
    custody: &CustodyClient,
    sol_addr: &str,
    pay_to: &str,
    amount: u64,
    asset: &str,
) -> Result<String> {
    let payment = sign_sol_payment(mpc, custody, sol_addr, pay_to, amount, asset, None, None, true).await?;
    Ok(payment.tx_signature)
}

// ─── MPP PaymentProvider ─────────────────────────────────────────────

/// MPP payment provider backed by NEAR MPC → Solana.
///
/// Implements `PaymentProvider` from the `mpp` crate for use with
/// `.send_with_payment()` on any reqwest request.
#[derive(Clone)]
pub struct MpcSolanaProvider {
    mpc: MpcClient,
    custody: CustodyClient,
    sol_address: std::sync::Arc<tokio::sync::Mutex<Option<String>>>,
}

impl MpcSolanaProvider {
    pub fn new(mpc: MpcClient, custody: CustodyClient) -> Self {
        Self {
            mpc,
            custody,
            sol_address: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    async fn get_sol_address(&self) -> Result<String> {
        let mut cached = self.sol_address.lock().await;
        if cached.is_none() {
            let addr = self.mpc.derive_solana_address(PAY_SOL_PATH).await?;
            *cached = Some(addr);
        }
        Ok(cached.clone().unwrap())
    }
}

impl PaymentProvider for MpcSolanaProvider {
    fn supports(&self, method: &str, intent: &str) -> bool {
        // draft-solana-charge-00: method="solana", intent="charge"
        method == "solana" && intent == "charge"
    }

    async fn pay(
        &self,
        challenge: &PaymentChallenge,
    ) -> std::result::Result<PaymentCredential, mpp::error::MppError> {
        let req: SolanaChargeRequest = challenge
            .request
            .decode()
            .map_err(|e| mpp::error::MppError::InvalidChallenge {
                id: Some(challenge.id.clone()),
                reason: Some(format!("Failed to decode charge request: {}", e)),
            })?;

        let sol_addr = self
            .get_sol_address()
            .await
            .map_err(|e| mpp::error::MppError::Http(format!("Failed to derive address: {}", e)))?;

        let amount: u64 = req
            .amount
            .parse()
            .map_err(|e: std::num::ParseIntError| mpp::error::MppError::InvalidChallenge {
                id: Some(challenge.id.clone()),
                reason: Some(format!("Invalid amount: {}", e)),
            })?;

        // Resolve asset: native SOL or SPL token mint
        let (asset, is_native) = req.resolve();

        // Determine mode: pull (server broadcasts) if feePayerKey present, else push
        let is_pull = req.fee_payer_key.is_some();

        // Sign the payment. Don't relay in pull mode — the server does it.
        let payment = sign_sol_payment(
            &self.mpc,
            &self.custody,
            &sol_addr,
            &req.recipient,
            amount,
            asset,
            if !is_native { req.decimals } else { None },
            req.fee_payer_key.as_deref(),
            !is_pull, // relay only in push mode
        )
        .await
        .map_err(|e| mpp::error::MppError::Http(format!("Payment failed: {}", e)))?;

        // Build Solana Charge credential per draft-solana-charge-00.
        // The credential is a JSON envelope: { challenge, payload }.
        // We bypass the MPP crate's format_authorization because Solana Charge
        // uses "transaction" field instead of "signature", and "signature" for push mode.
        let payload_json = if is_pull {
            serde_json::json!({
                "type": "transaction",
                "transaction": payment.signed_tx_b64,
            })
        } else {
            serde_json::json!({
                "type": "signature",
                "signature": payment.tx_signature,
            })
        };

        // The MPP crate's PaymentCredential won't produce the right format.
        // Instead we encode directly as the MPP crate would but with Solana Charge fields.
        // Use hash payload as a carrier — the handle_mpp() method on PayClient
        // will build the proper Authorization header.
        let echo = challenge.to_echo();
        Ok(PaymentCredential::new(
            echo,
            MppPayload::hash(serde_json::to_string(&payload_json).unwrap_or_default()),
        ))
    }
}
///
/// Per draft-solana-charge-00:
/// - `recipient`: base58-encoded pubkey receiving payment
/// - `amount`: integer amount in base units
/// - `currency`: "sol" for native, or base58 mint address for SPL tokens
/// - `decimals`: required for SPL tokens (0–9)
/// - `feePayerKey`: optional, server-sponsored fees
/// - `externalId`: optional memo
#[derive(Debug, Deserialize)]
pub struct SolanaChargeRequest {
    pub recipient: String,
    pub amount: String,
    /// "sol" for native SOL, or base58 mint address (e.g. USDC)
    #[serde(default)]
    pub currency: Option<String>,
    /// Required when currency is a mint address
    #[serde(default)]
    pub decimals: Option<u8>,
    /// Server fee payer key (pull mode — server broadcasts)
    #[serde(default, rename = "feePayerKey")]
    pub fee_payer_key: Option<String>,
    #[serde(default, rename = "externalId")]
    pub external_id: Option<String>,
}

impl SolanaChargeRequest {
    /// Whether this is a native SOL payment.
    pub fn is_native_sol(&self) -> bool {
        self.currency.as_deref() == Some("sol") || self.currency.is_none()
    }

    /// Resolve the currency to a static asset string.
    /// For native SOL → SOL_NATIVE. For SPL tokens → USDC_MINT or the raw mint.
    /// Returns (asset_str, is_native).
    ///
    /// Note: for arbitrary SPL mints, the caller should use `self.currency` directly
    /// since we can't return `&'static str` for a dynamic string.
    pub fn resolve(&self) -> (&str, bool) {
        if self.is_native_sol() {
            (crate::mpc::SOL_NATIVE, true)
        } else {
            // Return the currency (mint address) from the struct
            // Default to USDC if no currency specified and somehow not native
            match self.currency.as_deref() {
                Some(mint) => (mint, false),
                None => (crate::mpc::USDC_MINT, false),
            }
        }
    }
}

// ─── x402 charge types (JSON body) ──────────────────────────────────

/// x402 V1 `PaymentRequired` response body.
///
/// Server returns this as JSON when status is 402 and there's no
/// `WWW-Authenticate` header (i.e. x402 protocol, not MPP).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct X402PaymentRequired {
    /// Protocol version (always 1).
    pub x402_version: u8,
    /// Acceptable payment methods.
    #[serde(default)]
    pub accepts: Vec<X402PaymentRequirements>,
    /// Optional error message.
    #[serde(default)]
    pub error: Option<String>,
}

/// Payment requirements from an x402 server.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct X402PaymentRequirements {
    /// Payment scheme (e.g. "exact").
    pub scheme: String,
    /// Network name (e.g. "solana", "base-sepolia").
    pub network: String,
    /// Amount to pay in smallest token unit.
    pub max_amount_required: String,
    /// Resource being paid for.
    pub resource: String,
    /// Human-readable description.
    #[serde(default)]
    pub description: String,
    /// Payment recipient address.
    pub pay_to: String,
    /// Token asset address.
    pub asset: String,
    /// Payment timeout in seconds.
    pub max_timeout_seconds: u64,
    /// Scheme-specific extra data (contains fee_payer for Solana).
    #[serde(default)]
    pub extra: Option<X402Extra>,
}

/// Extra data in x402 Solana payment requirements.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct X402Extra {
    /// Fee payer address (required for Solana x402).
    pub fee_payer: String,
}

/// x402 signed payment payload.
///
/// This is what goes in the `X-Payment` header as base64.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct X402PaymentPayload {
    /// Protocol version (1).
    pub x402_version: u8,
    /// Payment scheme ("exact").
    pub scheme: String,
    /// Network name.
    pub network: String,
    /// The signed transaction payload.
    pub payload: X402SolanaPayload,
}

/// Solana-specific payload inside x402 payment.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct X402SolanaPayload {
    /// Base64-encoded signed Solana transaction.
    pub transaction: String,
}

// ─── High-level PayClient ────────────────────────────────────────────

/// Payment client for NEAR agents. Handles both MPP and x402 protocols.
///
/// Usage:
/// ```ignore
/// let mut pay = PayClient::from_api_key("wk_...").with_testnet(false);
///
/// // GET with automatic 402 handling (MPP or x402 — detected automatically)
/// let resp = pay.get("https://api.example.com/paid").await?;
///
/// // Direct Solana transfer (no 402)
/// let tx_hash = pay.transfer_sol("RecipAddr...", 1_000_000).await?;
/// ```
pub struct PayClient {
    custody: CustodyClient,
    mpc: MpcClient,
    http: reqwest::Client,
    sol_address: Option<String>,
}

impl PayClient {
    pub fn new(custody: CustodyClient) -> Self {
        let mpc = MpcClient::new(custody.clone_for_mpc(), true);
        Self {
            custody,
            mpc,
            http: reqwest::Client::new(),
            sol_address: None,
        }
    }

    pub fn from_api_key(api_key: &str) -> Self {
        Self::new(CustodyClient::from_api_key(api_key))
    }

    pub fn with_testnet(mut self, testnet: bool) -> Self {
        self.mpc = MpcClient::new(self.custody.clone_for_mpc(), testnet);
        self
    }

    async fn ensure_sol_address(&mut self) -> Result<&str> {
        if self.sol_address.is_none() {
            let addr = self.mpc.derive_solana_address(PAY_SOL_PATH).await?;
            self.sol_address = Some(addr);
        }
        Ok(self.sol_address.as_ref().unwrap())
    }

    /// Minimum SOL balance to keep for rent exemption + fees (0.01 SOL).
    const MIN_SOL_RESERVE: u64 = 10_000_000;

    /// Check SOL balance and auto-fund from OutLayer if below threshold.
    ///
    /// Returns the funded amount (0 if already funded, >0 if topped up).
    async fn ensure_funded(&self, sol_addr: &str, needed_lamports: u64) -> Result<u64> {
        let balance = self.mpc.sol_balance(sol_addr).await?;
        let threshold = needed_lamports + Self::MIN_SOL_RESERVE;

        if balance >= threshold {
            tracing::debug!(
                "Balance {} lamports sufficient (need {})",
                balance,
                threshold
            );
            return Ok(0);
        }

        let deficit = threshold - balance;
        // Round up to nearest 0.01 SOL for withdraw
        let fund_lamports = ((deficit + 10_000_000 - 1) / 10_000_000) * 10_000_000;
        let fund_sol = fund_lamports as f64 / 1_000_000_000.0;

        tracing::info!(
            "Auto-funding {} SOL (balance={}, need={})",
            fund_sol,
            balance,
            threshold
        );

        let result = self
            .custody
            .withdraw(sol_addr, &format!("{:.9}", fund_sol), "wrap.near", "solana")
            .await?;

        tracing::info!("Fund request submitted: {}", result.request_id);
        Ok(fund_lamports)
    }

    // ─── MPP provider ────────────────────────────────────────────────

    /// Build an `MpcSolanaProvider` for use with MPP's `send_with_payment`.
    pub fn mpp_provider(&self) -> MpcSolanaProvider {
        MpcSolanaProvider::new(
            MpcClient::new(self.custody.clone_for_mpc(), self.mpc.is_testnet()),
            self.custody.clone_for_mpc(),
        )
    }

    // ─── Direct Solana transfers ─────────────────────────────────────

    pub async fn transfer_sol(&self, to: &str, lamports: u64) -> Result<String> {
        self.mpc.transfer_sol(PAY_SOL_PATH, to, lamports).await
    }

    // ─── Unified send ────────────────────────────────────────────────

    /// Send to any chain. Routes automatically:
    /// - solana → MPC sign + relay
    /// - anything else → OutLayer Intents cross-chain
    ///
    /// `amount` is in smallest unit (lamports for SOL, wei for ETH, etc).
    /// `token` is the asset identifier (SOL_NATIVE for native SOL, mint address for SPL,
    /// "wrap.near" for NEAR native, "usdt.tether-token.near" for USDT, etc).
    pub async fn send(
        &mut self,
        chain: &str,
        address: &str,
        amount: &str,
        token: &str,
    ) -> Result<SendResult> {
        match chain {
            "solana" => self.send_solana(address, amount, token).await,
            _ => self.send_intents(chain, address, amount, token).await,
        }
    }

    /// MPC Solana path — sign + relay.
    async fn send_solana(
        &mut self,
        address: &str,
        amount_str: &str,
        token: &str,
    ) -> Result<SendResult> {
        let amount: u64 = amount_str
            .parse()
            .map_err(|e| Error::Api(format!("Invalid amount: {}", e)))?;

        let sol_addr = self.ensure_sol_address().await?.to_string();

        // Auto-fund if needed
        self.ensure_funded(&sol_addr, amount).await?;

        let tx_hash = execute_sol_payment(
            &self.mpc,
            &self.custody,
            &sol_addr,
            address,
            amount,
            token,
        )
        .await?;

        Ok(SendResult {
            chain: "solana".to_string(),
            address: address.to_string(),
            amount: amount_str.to_string(),
            token: token.to_string(),
            tx_hash,
        })
    }

    /// Intents cross-chain path — deposit + withdraw + poll.
    async fn send_intents(
        &self,
        chain: &str,
        address: &str,
        amount: &str,
        token: &str,
    ) -> Result<SendResult> {
        let result = self
            .custody
            .send_cross_chain(address, amount, token, chain)
            .await?;

        Ok(SendResult {
            chain: result.chain,
            address: result.address,
            amount: result.amount,
            token: result.token,
            tx_hash: result.tx_hash.unwrap_or(result.request_id),
        })
    }

    // ─── Auto-detect 402 protocol ─────────────────────────────────────

    /// GET with automatic 402 payment (MPP or x402, detected per response).
    pub async fn get(&mut self, url: &str) -> Result<PaidResponse> {
        self.request("GET", url, None, vec![]).await
    }

    /// POST with automatic 402 payment.
    pub async fn post(
        &mut self,
        url: &str,
        body: Option<serde_json::Value>,
        headers: Vec<(String, String)>,
    ) -> Result<PaidResponse> {
        self.request("POST", url, body, headers).await
    }

    /// Core request with dual-protocol 402 handling.
    ///
    /// Detection logic:
    /// 1. Check `WWW-Authenticate` header → MPP protocol
    /// 2. Parse body as x402 JSON → x402 protocol
    /// 3. Neither → return the 402 as-is (caller's problem)
    async fn request(
        &mut self,
        method: &str,
        url: &str,
        body: Option<serde_json::Value>,
        extra_headers: Vec<(String, String)>,
    ) -> Result<PaidResponse> {
        let resp = self.raw_request(method, url, &body, &extra_headers).await?;
        let status = resp.status().as_u16();

        if status != 402 {
            let resp_body = resp.text().await.map_err(Error::Http)?;
            return Ok(PaidResponse {
                status,
                body: resp_body,
                amount_paid: "0".to_string(),
                token: "sol".to_string(),
            });
        }

        // ── Protocol detection ──

        // Try MPP first: WWW-Authenticate header
        let www_auth = resp
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        if www_auth.starts_with("Payment ") {
            return self.handle_mpp(method, url, body, extra_headers, www_auth).await;
        }

        // Try x402: JSON body
        let resp_body = resp.text().await.map_err(Error::Http)?;
        if let Ok(x402_req) = serde_json::from_str::<X402PaymentRequired>(&resp_body) {
            return self
                .handle_x402(method, url, body, extra_headers, x402_req)
                .await;
        }

        // Unknown 402 format
        Err(Error::X402(format!(
            "Unknown 402 format (no WWW-Authenticate, not x402 JSON)"
        )))
    }

    // ── MPP flow ──────────────────────────────────────────────────────

    async fn handle_mpp(
        &mut self,
        method: &str,
        url: &str,
        body: Option<serde_json::Value>,
        extra_headers: Vec<(String, String)>,
        www_auth: &str,
    ) -> Result<PaidResponse> {
        let challenge = mpp::protocol::core::PaymentChallenge::from_header(www_auth)
            .map_err(|e| Error::X402(format!("MPP parse error: {}", e)))?;

        let amount_str = match challenge.request.decode_value() {
            Ok(v) => v
                .get("amount")
                .and_then(|a| a.as_str())
                .unwrap_or("0")
                .to_string(),
            Err(_) => "0".to_string(),
        };

        let req: SolanaChargeRequest = challenge
            .request
            .decode()
            .map_err(|e| Error::X402(format!("MPP decode error: {}", e)))?;

        let amount: u64 = req
            .amount
            .parse()
            .map_err(|e: std::num::ParseIntError| Error::X402(format!("Invalid amount: {}", e)))?;

        self.ensure_sol_address().await?;
        let sol_addr = self.ensure_sol_address().await?.to_string();

        // Resolve asset from Solana Charge fields
        let (asset, is_native) = req.resolve();

        // Auto-fund if balance too low
        let needed = if req.is_native_sol() { amount } else { 0 };
        let _ = self.ensure_funded(&sol_addr, needed).await;

        // Determine mode: pull if feePayerKey present, else push
        let is_pull = req.fee_payer_key.is_some();

        let payment = sign_sol_payment(
            &self.mpc,
            &self.custody,
            &sol_addr,
            &req.recipient,
            amount,
            asset,
            if !is_native { req.decimals } else { None },
            req.fee_payer_key.as_deref(),
            !is_pull,
        ).await?;

        // Build Solana Charge credential JSON
        let payload = if is_pull {
            serde_json::json!({
                "type": "transaction",
                "transaction": payment.signed_tx_b64,
            })
        } else {
            serde_json::json!({
                "type": "signature",
                "signature": payment.tx_signature,
            })
        };

        let credential = serde_json::json!({
            "challenge": {
                "id": challenge.id,
                "realm": challenge.realm,
                "method": challenge.method,
                "intent": challenge.intent,
                "request": challenge.request,
                "expires": challenge.expires,
            },
            "payload": payload,
        });

        // Base64url-encode the credential for the Authorization header
        let cred_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_string(&credential).unwrap_or_default());

        let mut retry_headers = extra_headers;
        retry_headers.push(("Authorization".to_string(), format!("Payment {}", cred_b64)));

        let retry_resp = self.raw_request(method, url, &body, &retry_headers).await?;
        let retry_status = retry_resp.status().as_u16();
        let retry_body = retry_resp.text().await.map_err(Error::Http)?;

        Ok(PaidResponse {
            status: retry_status,
            body: retry_body,
            amount_paid: amount_str,
            token: req.currency.unwrap_or_else(|| "sol".to_string()),
        })
    }

    // ── x402 flow ─────────────────────────────────────────────────────

    async fn handle_x402(
        &mut self,
        method: &str,
        url: &str,
        body: Option<serde_json::Value>,
        extra_headers: Vec<(String, String)>,
        x402_req: X402PaymentRequired,
    ) -> Result<PaidResponse> {
        // Find a Solana payment requirement we can fulfill
        let req = x402_req
            .accepts
            .iter()
            .find(|r| {
                // Accept Solana networks
                r.network == "solana"
                    || r.network == "solana-mainnet"
                    || r.network == "solana-devnet"
                    || r.network == "solana-testnet"
            })
            .ok_or_else(|| Error::X402("No Solana payment option in x402 accepts".into()))?
            .clone();

        let pay_to = req.pay_to.clone();
        let amount: u64 = req
            .max_amount_required
            .parse()
            .map_err(|e: std::num::ParseIntError| {
                Error::X402(format!("Invalid x402 amount: {}", e))
            })?;

        self.ensure_sol_address().await?;
        let sol_addr = self.ensure_sol_address().await?.to_string();

        // Auto-fund if balance too low
        let needed = if req.asset == crate::mpc::SOL_NATIVE { amount } else { 0 };
        let _ = self.ensure_funded(&sol_addr, needed).await;

        // Build and sign the transfer — asset determines SOL vs SPL
        let tx = self.mpc.build_transfer(&sol_addr, &pay_to, amount, &req.asset).await?;
        let signature = self.mpc.sign_transaction(&tx, PAY_SOL_PATH).await?;
        let signed = self.mpc.finalize_transaction(&tx, &sol_addr, &signature)?;

        // Serialize the signed transaction as base64 (x402 wire format)
        let tx_bytes = bincode::serialize(&signed)
            .map_err(|e| Error::X402(format!("Tx serialize error: {}", e)))?;
        let tx_b64 = base64::engine::general_purpose::STANDARD.encode(&tx_bytes);

        // Build the x402 payment payload
        let payload = X402PaymentPayload {
            x402_version: 1,
            scheme: "exact".to_string(),
            network: req.network.clone(),
            payload: X402SolanaPayload {
                transaction: tx_b64,
            },
        };

        let payload_json = serde_json::to_vec(&payload)
            .map_err(|e| Error::X402(format!("Payload serialize error: {}", e)))?;
        let payload_b64 = base64::engine::general_purpose::STANDARD.encode(&payload_json);

        // Relay to Solana (we still broadcast even though facilitator might too)
        let _tx_hash = execute_sol_payment(&self.mpc, &self.custody, &sol_addr, &pay_to, amount, &req.asset).await?;

        // Retry with X-Payment header
        let mut retry_headers = extra_headers;
        retry_headers.push(("X-Payment".to_string(), payload_b64));

        let retry_resp = self.raw_request(method, url, &body, &retry_headers).await?;
        let retry_status = retry_resp.status().as_u16();
        let retry_body = retry_resp.text().await.map_err(Error::Http)?;

        Ok(PaidResponse {
            status: retry_status,
            body: retry_body,
            amount_paid: req.max_amount_required,
            token: "sol".to_string(),
        })
    }

    // ─── Raw HTTP ─────────────────────────────────────────────────────

    async fn raw_request(
        &self,
        method: &str,
        url: &str,
        body: &Option<serde_json::Value>,
        headers: &[(String, String)],
    ) -> Result<reqwest::Response> {
        let mut req = match method {
            "POST" => self.http.post(url),
            "PUT" => self.http.put(url),
            _ => self.http.get(url),
        };

        for (key, value) in headers {
            req = req.header(key.as_str(), value.as_str());
        }

        if let Some(body) = body {
            req = req.json(body);
        }

        req.send().await.map_err(Error::Http)
    }

    // ─── Funding ──────────────────────────────────────────────────────

    pub async fn fund_sol(&self, amount_sol: &str) -> Result<String> {
        let addr = self.mpc.derive_solana_address(PAY_SOL_PATH).await?;
        let result = self
            .custody
            .withdraw(&addr, amount_sol, "wrap.near", "solana")
            .await?;
        Ok(result.request_id)
    }

    pub async fn sol_balance(&self) -> Result<u64> {
        let addr = self.mpc.derive_solana_address(PAY_SOL_PATH).await?;
        self.mpc.sol_balance(&addr).await
    }

    // ─── Direct access ────────────────────────────────────────────────

    pub fn custody(&self) -> &CustodyClient {
        &self.custody
    }

    pub fn mpc(&self) -> &MpcClient {
        &self.mpc
    }

    pub async fn sol_address(&mut self) -> Result<String> {
        self.ensure_sol_address().await.map(|s| s.to_string())
    }

    pub async fn wait_for_request(&self, request_id: &str) -> Result<RequestEntry> {
        for _ in 0..30 {
            let entry = self.custody.request_status(request_id).await?;
            match entry.status.as_str() {
                "success" => return Ok(entry),
                "failed" => {
                    return Err(Error::Api(format!("Request {} failed", request_id)))
                }
                _ => tokio::time::sleep(std::time::Duration::from_secs(2)).await,
            }
        }
        Err(Error::Api(format!("Request {} timed out", request_id)))
    }
}

// ─── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mpp_provider_supports() {
        let custody = CustodyClient::from_api_key("test-key");
        let mpc = MpcClient::new(custody.clone_for_mpc(), true);
        let provider = MpcSolanaProvider::new(mpc, custody);

        assert!(provider.supports("solana", "charge"));
        assert!(!provider.supports("tempo", "charge"));
        assert!(!provider.supports("solana", "session"));
    }

    #[test]
    fn test_solana_charge_request_decode() {
        let json = serde_json::json!({
            "recipient": "GCn668EvNPWQSFpJK3CxgJhkVrzWb8VtrAHcLshWzViH",
            "amount": "1000000",
            "currency": "sol"
        });
        let req: SolanaChargeRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.amount, "1000000");
        assert_eq!(req.recipient, "GCn668EvNPWQSFpJK3CxgJhkVrzWb8VtrAHcLshWzViH");
        assert!(req.is_native_sol());
    }

    #[test]
    fn test_solana_charge_request_spl() {
        let json = serde_json::json!({
            "recipient": "GCn668EvNPWQSFpJK3CxgJhkVrzWb8VtrAHcLshWzViH",
            "amount": "500000",
            "currency": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            "decimals": 6
        });
        let req: SolanaChargeRequest = serde_json::from_value(json).unwrap();
        assert!(!req.is_native_sol());
        let (asset, is_native) = req.resolve();
        assert_eq!(asset, "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
        assert!(!is_native);
    }

    #[test]
    fn test_x402_payment_required_parse() {
        let json = serde_json::json!({
            "x402Version": 1,
            "accepts": [{
                "scheme": "exact",
                "network": "solana",
                "maxAmountRequired": "1000000",
                "resource": "https://api.example.com/data",
                "description": "API call",
                "payTo": "GCn668EvNPWQSFpJK3CxgJhkVrzWb8VtrAHcLshWzViH",
                "asset": "11111111111111111111111111111111",
                "maxTimeoutSeconds": 300,
                "extra": {
                    "feePayer": "FeePayerAddr123"
                }
            }]
        });
        let req: X402PaymentRequired = serde_json::from_value(json).unwrap();
        assert_eq!(req.x402_version, 1);
        assert_eq!(req.accepts.len(), 1);
        assert_eq!(req.accepts[0].scheme, "exact");
        assert_eq!(req.accepts[0].network, "solana");
        assert_eq!(req.accepts[0].max_amount_required, "1000000");
        assert_eq!(req.accepts[0].pay_to, "GCn668EvNPWQSFpJK3CxgJhkVrzWb8VtrAHcLshWzViH");
        assert_eq!(
            req.accepts[0].extra.as_ref().unwrap().fee_payer,
            "FeePayerAddr123"
        );
    }

    #[test]
    fn test_x402_solana_network_detection() {
        let json = serde_json::json!({
            "x402Version": 1,
            "accepts": [
                {
                    "scheme": "exact",
                    "network": "base-sepolia",
                    "maxAmountRequired": "1000",
                    "resource": "/api",
                    "description": "",
                    "payTo": "0xEVM",
                    "asset": "0xUSDC",
                    "maxTimeoutSeconds": 60
                },
                {
                    "scheme": "exact",
                    "network": "solana-mainnet",
                    "maxAmountRequired": "5000",
                    "resource": "/api",
                    "description": "",
                    "payTo": "RecipAddr",
                    "asset": "11111111111111111111111111111111",
                    "maxTimeoutSeconds": 300,
                    "extra": { "feePayer": "FeePayer" }
                }
            ]
        });
        let req: X402PaymentRequired = serde_json::from_value(json).unwrap();
        let sol_req = req
            .accepts
            .iter()
            .find(|r| r.network.starts_with("solana"))
            .unwrap();
        assert_eq!(sol_req.network, "solana-mainnet");
        assert_eq!(sol_req.max_amount_required, "5000");
    }

    #[test]
    fn test_x402_payment_payload_serialization() {
        let payload = X402PaymentPayload {
            x402_version: 1,
            scheme: "exact".to_string(),
            network: "solana".to_string(),
            payload: X402SolanaPayload {
                transaction: "base64tx".to_string(),
            },
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["x402Version"], 1);
        assert_eq!(json["scheme"], "exact");
        assert_eq!(json["network"], "solana");
        assert_eq!(json["payload"]["transaction"], "base64tx");
    }

    #[test]
    fn test_x402_usdc_payment_parse() {
        // x402 server requesting USDC payment
        let json = serde_json::json!({
            "x402Version": 1,
            "accepts": [{
                "scheme": "exact",
                "network": "solana",
                "maxAmountRequired": "1000000",
                "resource": "https://api.example.com/premium",
                "description": "Premium API access",
                "payTo": "RecipientAddr123",
                "asset": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "maxTimeoutSeconds": 300,
                "extra": { "feePayer": "FeePayerAddr" }
            }]
        });
        let req: X402PaymentRequired = serde_json::from_value(json).unwrap();
        let accept = &req.accepts[0];
        assert_eq!(accept.asset, "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
        assert_ne!(accept.asset, "11111111111111111111111111111111", "USDC != SOL");
        // This would route to build_transfer → build_spl_transfer
    }

    #[test]
    fn test_x402_native_sol_payment_parse() {
        // x402 server requesting native SOL payment (asset = system program)
        let json = serde_json::json!({
            "x402Version": 1,
            "accepts": [{
                "scheme": "exact",
                "network": "solana",
                "maxAmountRequired": "5000000",
                "resource": "https://api.example.com/data",
                "description": "Data access",
                "payTo": "RecipientAddr",
                "asset": "11111111111111111111111111111111",
                "maxTimeoutSeconds": 60
            }]
        });
        let req: X402PaymentRequired = serde_json::from_value(json).unwrap();
        assert_eq!(req.accepts[0].asset, "11111111111111111111111111111111");
        // This would route to build_transfer → build_sol_transfer (native SOL)
    }

    #[test]
    fn test_mpp_challenge_roundtrip() {
        let request_b64 = base64::engine::general_purpose::STANDARD
            .encode(r#"{"amount":"1000","pay_to":"Abc123"}"#);
        let header = format!(
            r#"Payment id="test-123", realm="api.example.com", method="solana", intent="charge", request="{}""#,
            request_b64
        );
        let challenge = mpp::protocol::core::PaymentChallenge::from_header(&header).unwrap();
        assert_eq!(challenge.id, "test-123");
        assert_eq!(challenge.method.as_str(), "solana");
        assert_eq!(challenge.intent.as_str(), "charge");
    }

    #[test]
    fn test_mpp_credential_format() {
        let request_b64 = base64::engine::general_purpose::STANDARD.encode("{}");
        let header = format!(
            r#"Payment id="test-456", realm="api.example.com", method="solana", intent="charge", request="{}""#,
            request_b64
        );
        let challenge = mpp::protocol::core::PaymentChallenge::from_header(&header).unwrap();
        let echo = challenge.to_echo();
        let credential = PaymentCredential::new(echo, MppPayload::hash("0xabc123"));
        let auth = mpp::protocol::core::format_authorization(&credential).unwrap();
        assert!(auth.starts_with("Payment "));
    }

    #[test]
    fn test_derivation_path_constant() {
        assert!(PAY_SOL_PATH.starts_with("solana-"));
    }
}
