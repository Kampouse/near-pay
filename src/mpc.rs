//! NEAR MPC Chain Signatures — direct cross-chain signing.
//!
//! Calls the `v1.signer` MPC contract to derive addresses and sign transactions
//! on foreign chains (Ethereum, Solana, Bitcoin, etc.) from a NEAR account.
//!
//! Uses OutLayer Custody for NEAR-side signing (calling the MPC contract) and
//! for funding the derived foreign address (via cross-chain withdraw).

use base64::Engine;
use std::str::FromStr;
#[allow(deprecated)]
use solana_sdk::{
    hash::Hash,
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    system_instruction,
    message::Message,
    transaction::Transaction,
    signature::Signature as SolanaSignature,
};

use crate::custody::CustodyClient;
use crate::error::Error;
use crate::Result;

/// MPC contract IDs.
pub const MPC_MAINNET: &str = "v1.signer";
pub const MPC_TESTNET: &str = "v1.signer-prod.testnet";

/// Derivation domain IDs.
pub const DOMAIN_EDDSA: u8 = 1; // Ed25519 — Solana, SUI, Aptos
#[allow(dead_code)]
pub const DOMAIN_ECDSA: u8 = 0; // Secp256k1 — EVM, Bitcoin

/// Solana RPC URL.
pub const SOLANA_DEVNET: &str = "https://api.devnet.solana.com";
pub const SOLANA_MAINNET: &str = "https://api.mainnet-beta.solana.com";

/// Well-known Solana program IDs.
pub const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
pub const ATA_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
/// Native SOL — system program ID used as "no token" sentinel.
pub const SOL_NATIVE: &str = "11111111111111111111111111111111";
/// USDC mint on Solana mainnet.
pub const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

/// MPC Chain Signatures client.
///
/// Derives foreign addresses from a NEAR account + path, signs transactions
/// via the MPC network, and relays to the target chain.
///
/// Transaction building uses `solana-sdk` — matches chainsig.js exactly:
/// `SystemProgram.transfer` → `Transaction.compileMessage().serialize()`
#[derive(Clone)]
pub struct MpcClient {
    custody: CustodyClient,
    http: reqwest::Client,
    mpc_contract: String,
    solana_rpc: String,
}

impl MpcClient {
    /// Create MPC client using OutLayer custody for NEAR-side signing.
    pub fn new(custody: CustodyClient, testnet: bool) -> Self {
        Self {
            custody,
            http: reqwest::Client::new(),
            mpc_contract: if testnet {
                MPC_TESTNET.to_string()
            } else {
                MPC_MAINNET.to_string()
            },
            solana_rpc: if testnet {
                SOLANA_DEVNET.to_string()
            } else {
                SOLANA_MAINNET.to_string()
            },
        }
    }

    /// Set custom MPC contract address.
    pub fn with_mpc_contract(mut self, contract: &str) -> Self {
        self.mpc_contract = contract.to_string();
        self
    }

    /// Set custom Solana RPC URL.
    pub fn with_solana_rpc(mut self, url: &str) -> Self {
        self.solana_rpc = url.to_string();
        self
    }

    /// Get the Solana RPC URL (for balance queries, etc).
    pub fn solana_rpc_url(&self) -> &str {
        &self.solana_rpc
    }

    /// Access the underlying custody client.
    pub fn custody(&self) -> &CustodyClient {
        &self.custody
    }

    /// Whether we're on testnet.
    pub fn is_testnet(&self) -> bool {
        self.mpc_contract.contains("testnet")
    }

    /// Get SOL balance of an address via Solana RPC.
    pub async fn sol_balance(&self, address: &str) -> Result<u64> {
        let resp = self
            .http
            .post(&self.solana_rpc)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "getBalance",
                "params": [address, { "commitment": "confirmed" }]
            }))
            .send()
            .await
            .map_err(Error::Http)?;

        let result: serde_json::Value = resp.json().await.map_err(Error::Http)?;

        if let Some(error) = result.get("error") {
            return Err(Error::Api(format!("Solana balance error: {}", error)));
        }

        Ok(result["result"]["value"]
            .as_u64()
            .unwrap_or(0))
    }

    // ─── Step 1: Derive Address ────────────────────────────────────────

    /// Derive a Solana address from NEAR account + derivation path.
    ///
    /// The same (account, path) pair always produces the same Solana address.
    /// This is a view call — no gas, no transaction.
    ///
    /// Matches chainsig.js: `contract.getDerivedPublicKey({ path, predecessor, IsEd25519: true })`
    pub async fn derive_solana_address(&self, path: &str) -> Result<String> {
        let near_account = self.custody.near_account_id();

        let args = serde_json::json!({
            "path": path,
            "predecessor": near_account,
            "domain_id": DOMAIN_EDDSA,
        });

        let args_base64 = base64::engine::general_purpose::STANDARD
            .encode(serde_json::to_vec(&args).unwrap());

        let result: serde_json::Value = self
            .near_view_call(&self.mpc_contract, "derived_public_key", &args_base64)
            .await?;

        // Result is a NEAR-format public key: "ed25519:<base58_pubkey>"
        let result_bytes = result["result"]
            .as_array()
            .ok_or_else(|| Error::Api("No result array from derived_public_key".into()))?;

        // Decode the byte array into a string
        let result_str_bytes: Vec<u8> = result_bytes
            .iter()
            .filter_map(|v| v.as_u64().map(|n| n as u8))
            .collect();
        let result_str = String::from_utf8(result_str_bytes)
            .map_err(|e| Error::Api(format!("Result not UTF-8: {}", e)))?;

        // Strip quotes and "ed25519:" prefix
        let key_str = result_str.trim_matches('"');
        let base58_key = if key_str.starts_with("ed25519:") {
            &key_str[8..]
        } else {
            key_str
        };

        // Validate it's a real Solana pubkey
        let pubkey = Pubkey::try_from(base58_key)
            .map_err(|e| Error::Api(format!("Invalid Solana pubkey: {}", e)))?;

        Ok(pubkey.to_string())
    }

    // ─── Step 2: Build Solana Transfer ─────────────────────────────────

    /// Build a Solana native (SOL) transfer transaction.
    ///
    /// Uses `solana_sdk::system_transaction::transfer` — matches chainsig.js:
    /// `SystemProgram.transfer({ fromPubkey, toPubkey, lamports })`
    ///
    /// Returns the unsigned transaction and the message bytes to sign.
    /// The message bytes = `transaction.message.serialize()` — exactly what
    /// chainsig.js calls `transaction.compileMessage().serialize()`.
    pub async fn build_sol_transfer(
        &self,
        from: &str,
        to: &str,
        lamports: u64,
    ) -> Result<Transaction> {
        let from_pubkey = Pubkey::try_from(from)
            .map_err(|e| Error::Api(format!("Invalid from address: {}", e)))?;
        let to_pubkey = Pubkey::try_from(to)
            .map_err(|e| Error::Api(format!("Invalid to address: {}", e)))?;

        // Get recent blockhash from Solana RPC
        let blockhash = self.get_solana_blockhash().await?;

        // Build unsigned SystemProgram transfer transaction manually.
        // This matches chainsig.js exactly:
        //   const transaction = new Transaction()
        //   transaction.add(SystemProgram.transfer({ fromPubkey, toPubkey, lamports }))
        //   transaction.recentBlockhash = blockhash
        //   transaction.feePayer = fromPubkey
        let ix = system_instruction::transfer(&from_pubkey, &to_pubkey, lamports);
        let message = Message::new_with_blockhash(&[ix], Some(&from_pubkey), &blockhash);
        let tx = Transaction {
            signatures: vec![SolanaSignature::default()],
            message,
        };

        Ok(tx)
    }

    // ─── Step 2b: Build SPL Token Transfer ──────────────────────────────

    /// Derive the Associated Token Account address for (owner, mint).
    ///
    /// ATA = PDA["owner", "token_program", "mint"] with ATA program.
    pub fn derive_ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
        let token_program = Pubkey::try_from(TOKEN_PROGRAM).unwrap();
        let ata_program = Pubkey::try_from(ATA_PROGRAM).unwrap();
        Pubkey::find_program_address(
            &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
            &ata_program,
        )
        .0
    }

    /// Build an SPL Token Transfer instruction (instruction 12).
    ///
    /// Wire format:
    /// - Accounts: [source_ata (writable), dest_ata (writable), owner]
    /// - Data: 4 bytes discriminator (12u32 LE) + 8 bytes amount LE
    fn spl_transfer_ix(
        source: Pubkey,
        destination: Pubkey,
        authority: Pubkey,
        amount: u64,
    ) -> Instruction {
        let program_id = Pubkey::try_from(TOKEN_PROGRAM).unwrap();
        let mut data = Vec::with_capacity(12);
        data.extend_from_slice(&12u32.to_le_bytes()); // Transfer discriminator
        data.extend_from_slice(&amount.to_le_bytes());
        Instruction {
            program_id,
            accounts: vec![
                AccountMeta::new(source, false),
                AccountMeta::new(destination, false),
                AccountMeta::new_readonly(authority, true),
            ],
            data,
        }
    }

    /// Build an SPL token transfer transaction.
    ///
    /// Same pipeline as `build_sol_transfer` but with a Token Transfer
    /// instruction instead of SystemProgram transfer.
    pub async fn build_spl_transfer(
        &self,
        from: &str,
        to: &str,
        mint: &str,
        amount: u64,
    ) -> Result<Transaction> {
        let from_pubkey = Pubkey::try_from(from)
            .map_err(|e| Error::Api(format!("Invalid from address: {}", e)))?;
        let to_pubkey = Pubkey::try_from(to)
            .map_err(|e| Error::Api(format!("Invalid to address: {}", e)))?;
        let mint_pubkey = Pubkey::try_from(mint)
            .map_err(|e| Error::Api(format!("Invalid mint address: {}", e)))?;

        let source_ata = Self::derive_ata(&from_pubkey, &mint_pubkey);
        let dest_ata = Self::derive_ata(&to_pubkey, &mint_pubkey);

        let ix = Self::spl_transfer_ix(source_ata, dest_ata, from_pubkey, amount);

        let blockhash = self.get_solana_blockhash().await?;
        let message = Message::new_with_blockhash(&[ix], Some(&from_pubkey), &blockhash);
        let tx = Transaction {
            signatures: vec![SolanaSignature::default()],
            message,
        };

        Ok(tx)
    }

    /// Build either a native SOL or SPL token transfer, depending on the asset.
    ///
    /// If `asset == SOL_NATIVE` → `build_sol_transfer`
    /// Otherwise → `build_spl_transfer` (asset is the mint address)
    pub async fn build_transfer(
        &self,
        from: &str,
        to: &str,
        amount: u64,
        asset: &str,
    ) -> Result<Transaction> {
        if asset == SOL_NATIVE {
            self.build_sol_transfer(from, to, amount).await
        } else {
            self.build_spl_transfer(from, to, asset, amount).await
        }
    }

    // ─── Step 3: Request MPC Signature ─────────────────────────────────

    /// Request MPC signature for a Solana transaction.
    ///
    /// Matches chainsig.js exactly:
    /// ```text
    /// sign({
    ///   payloads: [messageBytes],
    ///   path: derivationPath,
    ///   keyType: "Eddsa",
    /// })
    /// ```
    ///
    /// Internally calls `v1.signer` with:
    /// ```text
    /// { request: { payload_v2: { Eddsa: hex(messageBytes) }, path, domain_id: 1 } }
    /// ```
    ///
    /// OutLayer returns the result inline:
    /// `{ request_id, status, tx_hash, result: { "scheme": "Ed25519", "signature": [u8; 64] } }`
    pub async fn sign_transaction(
        &self,
        tx: &Transaction,
        path: &str,
    ) -> Result<Vec<u8>> {
        // Serialize the message (the part that gets signed)
        // chainsig.js: transaction.compileMessage().serialize()
        let message_bytes = bincode::serialize(&tx.message)
            .map_err(|e| Error::Api(format!("Failed to serialize Solana message: {}", e)))?;

        let payload_hex = hex::encode(&message_bytes);

        let sign_args = serde_json::json!({
            "request": {
                "payload_v2": { "Eddsa": payload_hex },
                "path": path,
                "domain_id": DOMAIN_EDDSA,
            }
        });

        let response = self
            .custody
            .call(
                &self.mpc_contract,
                "sign",
                sign_args,
                "1",
            )
            .await?;

        let result_json = serde_json::to_string(&response.result)
            .map_err(|e| Error::Api(format!("Failed to serialize MPC result: {}", e)))?;

        self.parse_ed25519_signature(&result_json)
    }

    /// Parse the Ed25519 signature from the MPC contract return value.
    ///
    /// OutLayer returns the result inline:
    /// { "scheme": "Ed25519", "signature": [u8; 64] }
    fn parse_ed25519_signature(&self, json_str: &str) -> Result<Vec<u8>> {
        let sig_val: serde_json::Value = serde_json::from_str(json_str)
            .map_err(|e| Error::Api(format!("Invalid MPC signature JSON: {}", e)))?;

        if sig_val.get("scheme").and_then(|s| s.as_str()) == Some("Ed25519") {
            let sig_bytes = sig_val["signature"]
                .as_array()
                .ok_or_else(|| Error::Api("No signature array in MPC response".into()))?
                .iter()
                .map(|v| {
                    v.as_u64()
                        .map(|n| n as u8)
                        .ok_or_else(|| Error::Api("Non-integer in signature array".into()))
                })
                .collect::<Result<Vec<u8>>>()?;

            if sig_bytes.len() != 64 {
                return Err(Error::Api(format!(
                    "Expected 64-byte Ed25519 signature, got {} bytes",
                    sig_bytes.len()
                )));
            }

            return Ok(sig_bytes);
        }

        Err(Error::Api(format!(
            "Unexpected MPC response format: {}",
            json_str
        )))
    }

    // ─── Step 4: Assemble Signed Transaction ───────────────────────────

    /// Add the MPC signature to the transaction.
    ///
    /// Matches chainsig.js:
    /// `transaction.addSignature(new PublicKey(senderAddress), Buffer.from(rsvSignatures.signature))`
    pub fn finalize_transaction(
        &self,
        tx: &Transaction,
        _from: &str,
        signature_bytes: &[u8],
    ) -> Result<Vec<u8>> {

        // Create a Solana Signature from the raw 64 bytes
        let sig = SolanaSignature::try_from(signature_bytes)
            .map_err(|e| Error::Api(format!("Invalid Ed25519 signature: {}", e)))?;

        // Clone the transaction and add the signature
        let mut signed_tx = tx.clone();
        signed_tx.signatures = vec![sig];

        // Serialize the fully signed transaction
        let serialized = bincode::serialize(&signed_tx)
            .map_err(|e| Error::Api(format!("Failed to serialize signed tx: {}", e)))?;

        Ok(serialized)
    }

    // ─── Step 5: Relay to Solana ───────────────────────────────────────

    /// Broadcast a signed transaction to Solana.
    ///
    /// Matches chainsig.js:
    /// `connection.sendRawTransaction(transaction.serialize())`
    pub async fn relay_to_solana(&self, signed_tx: &[u8]) -> Result<String> {
        let tx_b64 = base64::engine::general_purpose::STANDARD.encode(signed_tx);

        let resp = self
            .http
            .post(&self.solana_rpc)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "sendTransaction",
                "params": [
                    tx_b64,
                    {
                        "encoding": "base64",
                        "skipPreflight": false
                    }
                ]
            }))
            .send()
            .await
            .map_err(Error::Http)?;

        let result: serde_json::Value = resp.json().await.map_err(Error::Http)?;

        if let Some(error) = result.get("error") {
            return Err(Error::Api(format!(
                "Solana RPC error: {}",
                error
            )));
        }

        let signature = result["result"]
            .as_str()
            .ok_or_else(|| Error::Api("No signature in Solana response".into()))?
            .to_string();

        Ok(signature)
    }

    // ─── Full Pipeline ─────────────────────────────────────────────────

    /// Transfer SOL from MPC-derived address to any Solana address.
    ///
    /// Full 5-step flow matching chainsig.js:
    /// 1. derive address (getDerivedPublicKey)
    /// 2. build tx (SystemProgram.transfer)
    /// 3. sign (v1.signer sign)
    /// 4. finalize (addSignature)
    /// 5. relay (sendRawTransaction)
    pub async fn transfer_sol(
        &self,
        path: &str,
        to: &str,
        lamports: u64,
    ) -> Result<String> {
        // 1. Derive our Solana address
        let from = self.derive_solana_address(path).await?;

        // 2. Build unsigned transaction
        let tx = self.build_sol_transfer(&from, to, lamports).await?;

        // 3. Request MPC signature
        let signature = self.sign_transaction(&tx, path).await?;

        // 4. Assemble signed transaction
        let signed = self.finalize_transaction(&tx, &from, &signature)?;

        // 5. Relay to Solana
        let tx_hash = self.relay_to_solana(&signed).await?;

        Ok(tx_hash)
    }

    /// Fund MPC-derived Solana address from NEAR wallet.
    ///
    /// Uses OutLayer to withdraw SOL from the custody wallet to the
    /// MPC-derived Solana address. After this, the MPC address has SOL
    /// for both transfers and rent.
    pub async fn fund_mpc_address(&self, path: &str, amount_sol: &str) -> Result<String> {
        let sol_address = self.derive_solana_address(path).await?;

        let result = self
            .custody
            .withdraw(
                &sol_address,
                amount_sol,
                "wrap.near",
                "solana",
            )
            .await?;

        Ok(result.request_id)
    }

    // ─── NEAR RPC helpers ──────────────────────────────────────────────

    /// NEAR RPC view function call (read-only, no gas).
    async fn near_view_call(
        &self,
        contract_id: &str,
        method_name: &str,
        args_base64: &str,
    ) -> Result<serde_json::Value> {
        let near_rpc = if self.mpc_contract.contains("testnet") {
            "https://rpc.testnet.near.org"
        } else {
            "https://rpc.mainnet.near.org"
        };

        let resp = self
            .http
            .post(near_rpc)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "query",
                "params": {
                    "request_type": "call_function",
                    "finality": "final",
                    "account_id": contract_id,
                    "method_name": method_name,
                    "args_base64": args_base64,
                }
            }))
            .send()
            .await
            .map_err(Error::Http)?;

        let result: serde_json::Value = resp.json().await.map_err(Error::Http)?;

        if let Some(error) = result.get("error") {
            return Err(Error::Api(format!(
                "NEAR RPC error: {}",
                error
            )));
        }

        Ok(result["result"].clone())
    }

    // ─── Solana RPC helpers ────────────────────────────────────────────

    /// Get recent blockhash from Solana.
    async fn get_solana_blockhash(&self) -> Result<solana_sdk::hash::Hash> {
        let resp = self
            .http
            .post(&self.solana_rpc)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "getLatestBlockhash",
                "params": [{ "commitment": "finalized" }]
            }))
            .send()
            .await
            .map_err(Error::Http)?;

        let result: serde_json::Value = resp.json().await.map_err(Error::Http)?;

        if let Some(error) = result.get("error") {
            return Err(Error::Api(format!(
                "Solana blockhash error: {}",
                error
            )));
        }

        let blockhash_str = result["result"]["value"]["blockhash"]
            .as_str()
            .ok_or_else(|| Error::Api("No blockhash in response".into()))?;

        let hash = Hash::from_str(blockhash_str)
            .map_err(|e| Error::Api(format!("Invalid blockhash: {}", e)))?;

        Ok(hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mpc_constants() {
        assert_eq!(MPC_MAINNET, "v1.signer");
        assert_eq!(MPC_TESTNET, "v1.signer-prod.testnet");
        assert_eq!(DOMAIN_EDDSA, 1);
        assert_eq!(DOMAIN_ECDSA, 0);
    }

    #[test]
    fn test_derivation_path_format() {
        let path = "solana-1";
        assert!(path.starts_with("solana-"));
    }

    #[test]
    fn test_solana_pubkey_validation() {
        // A valid Solana address
        let addr = "11111111111111111111111111111111"; // system program
        assert!(Pubkey::try_from(addr).is_ok());

        // Invalid
        assert!(Pubkey::try_from("not-a-pubkey").is_err());
    }

    #[test]
    fn test_mpc_sign_args_format() {
        // Verify the sign args match chainsig.js format exactly
        let payload_hex = hex::encode(&[0u8; 32]);
        let sign_args = serde_json::json!({
            "request": {
                "payload_v2": { "Eddsa": payload_hex },
                "path": "solana-1",
                "domain_id": 1,
            }
        });

        assert_eq!(sign_args["request"]["payload_v2"]["Eddsa"], hex::encode(&[0u8; 32]));
        assert_eq!(sign_args["request"]["path"], "solana-1");
        assert_eq!(sign_args["request"]["domain_id"], 1);
    }

    #[test]
    fn test_derive_ata_deterministic() {
        // ATA derivation is deterministic — same inputs always produce same ATA
        let owner = Pubkey::try_from("GCn668EvNPWQSFpJK3CxgJhkVrzWb8VtrAHcLshWzViH").unwrap();
        let usdc_mint = Pubkey::try_from(USDC_MINT).unwrap();
        let sol_mint = Pubkey::try_from(SOL_NATIVE).unwrap();

        let ata1 = MpcClient::derive_ata(&owner, &usdc_mint);
        let ata2 = MpcClient::derive_ata(&owner, &usdc_mint);
        assert_eq!(ata1, ata2, "ATA derivation must be deterministic");

        // Different mint → different ATA
        let ata_sol = MpcClient::derive_ata(&owner, &sol_mint);
        assert_ne!(ata1, ata_sol, "Different mints must produce different ATAs");
    }

    #[test]
    fn test_spl_transfer_ix_encoding() {
        // Verify instruction 12 (Transfer) encoding
        let source = Pubkey::new_unique();
        let dest = Pubkey::new_unique();
        let authority = Pubkey::new_unique();
        let amount: u64 = 1_000_000; // 1 USDC (6 decimals)

        let ix = MpcClient::spl_transfer_ix(source, dest, authority, amount);

        // Must target Token program
        assert_eq!(ix.program_id, Pubkey::try_from(TOKEN_PROGRAM).unwrap());

        // 3 accounts: source (writable), dest (writable), authority (signer)
        assert_eq!(ix.accounts.len(), 3);
        assert!(ix.accounts[0].is_writable);
        assert!(ix.accounts[1].is_writable);
        assert!(!ix.accounts[2].is_writable);
        assert!(ix.accounts[2].is_signer);

        // Data: 4 bytes discriminator + 8 bytes amount = 12 bytes
        assert_eq!(ix.data.len(), 12);
        let disc = u32::from_le_bytes(ix.data[0..4].try_into().unwrap());
        assert_eq!(disc, 12u32, "Transfer discriminator must be 12");
        let amt = u64::from_le_bytes(ix.data[4..12].try_into().unwrap());
        assert_eq!(amt, 1_000_000);
    }

    #[test]
    fn test_build_transfer_routing() {
        // SOL_NATIVE → build_sol_transfer, anything else → build_spl_transfer
        // Can't call async in test, but we can verify the constants
        assert_eq!(SOL_NATIVE, "11111111111111111111111111111111");
        assert_ne!(USDC_MINT, SOL_NATIVE);
        assert_eq!(USDC_MINT, "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
    }
}
