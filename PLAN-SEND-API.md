# PLAN: Unified Send API for agent-pay

## Architecture

```
┌─────────────────────────────────────────────────┐
│  PayClient (high-level)                         │
│  send(chain, address, amount, token) ← unified  │
│  get(url) / post(url, body)  ← MPP/x402 auto   │
│  fund_self(amount)             ← Intents bridge │
│  transfer_sol(to, lamports)    ← MPC direct     │
├─────────────────┬───────────────────────────────┤
│  MpcSolanaProvider (MPP PaymentProvider trait)  │
│  supports("solanampc", "charge") → true         │
│  pay(challenge) → build+sign+relay → credential │
├─────────────────┼───────────────────────────────┤
│  MpcClient      │  CustodyClient                │
│  derive_address │  call (MPC sign)              │
│  build_transfer │  withdraw (Intents → chain)   │
│  sign_tx        │  balance                      │
│  finalize       │  policy                       │
│  relay          │  deposit_to_intents           │
│  sol_balance    │  request_status               │
│  derive_ata     │  transfer (NEAR native)       │
│  spl_transfer   │  swap                         │
└─────────────────┴───────────────────────────────┘
         │                    │
         ▼                    ▼
   Solana RPC          OutLayer API
   (broadcast)         (NEAR custody)
```

## Current Status

### Implemented

| Feature | Status | Notes |
|---------|--------|-------|
| CustodyClient | ✅ | All REST methods: transfer, call, withdraw, swap, deposit_to_intents, balance |
| MpcClient | ✅ | derive_address, build_sol_transfer, sign, finalize, relay, sol_balance |
| SPL Token support | ✅ | build_spl_transfer, build_transfer router, ATA derivation, USDC/SOL constants |
| MpcSolanaProvider | ✅ | PaymentProvider trait impl, method "solanampc" |
| PayClient.get/post | ✅ | Dual-protocol 402: MPP (WWW-Authenticate) + x402 (JSON body) |
| PayClient.transfer_sol | ✅ | Direct MPC Solana transfer |
| PayClient.fund_sol | ✅ | Fund MPC address via OutLayer Intents |
| Balance gate | ✅ | Checks SOL balance before building tx, clean error on insufficient |
| ensure_funded | ✅ | Auto-topup from OutLayer Intents (written, blocked by approval policy) |
| Error types | ✅ | InsufficientBalance with concrete numbers |

### Not Yet Implemented

| Feature | Plan Step | Notes |
|---------|-----------|-------|
| `send()` unified method | Step 2 | Smart router: MPC for Solana, Intents for other chains |
| `send_cross_chain()` | Step 1 | Intents deposit + withdraw pipeline for ETH/BTC/etc |
| `search_catalog()` | — | pay.sh service discovery, low priority |

## MPP Integration

Uses the `mpp` crate (v0.10) for IETF-standard HTTP 402 payments.

### Custom Payment Method: `solanampc`

MPP method names are `1*LOWERALPHA` (a-z only). Our method is `solanampc`.

The `MpcSolanaProvider` implements `PaymentProvider`:

```rust
impl PaymentProvider for MpcSolanaProvider {
    fn supports(&self, method: &str, intent: &str) -> bool {
        method == "solanampc" && intent == "charge"
    }

    async fn pay(&self, challenge: &PaymentChallenge) -> Result<PaymentCredential> {
        // 1. Decode charge request from challenge
        // 2. Build Solana transfer (SOL or SPL via asset field)
        // 3. Sign via NEAR MPC chain signatures
        // 4. Relay to Solana
        // 5. Return credential with tx hash
    }
}
```

### Dual-Protocol 402 Detection

```
Server returns 402
  │
  ├─ WWW-Authenticate: Payment ... header present?
  │   → MPP flow (mpp crate handles header parse/format)
  │
  ├─ JSON body with x402Version + accepts?
  │   → x402 flow (hand-rolled JSON body parsing)
  │
  └─ Neither → Error("Unknown 402 format")
```

Both share the same `execute_sol_payment()` backend:
- Balance gate: check SOL balance, return InsufficientBalance if low
- `build_transfer(from, to, amount, asset)`: routes SOL vs SPL
- MPC sign via `v1.signer` contract
- Relay to Solana RPC

### SPL Token Support

```rust
// Auto-routed by asset field in payment requirements
build_transfer(from, to, amount, "11111111111111111111111111111111") // → SOL
build_transfer(from, to, amount, "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v") // → USDC
```

- `derive_ata(owner, mint)` — PDA-based ATA derivation
- `spl_transfer_ix(source, dest, authority, amount)` — instruction 12 encoding
- `build_spl_transfer(from, to, mint, amount)` — full tx with Token Transfer

### MPP Protocol Flow

```
Client                          Server
  │                               │
  │  GET /api/data                │
  │──────────────────────────────→│
  │                               │
  │  402 WWW-Authenticate: Payment│
  │  id="abc", method="solanampc",│
  │  intent="charge",             │
  │  request="eyJhbW91...fQ"      │
  │←──────────────────────────────│
  │                               │
  │  MPC sign + Solana relay      │
  │  (MpcSolanaProvider.pay())    │
  │                               │
  │  GET /api/data                │
  │  Authorization: Payment ...   │
  │──────────────────────────────→│
  │                               │
  │  200 + Payment-Receipt        │
  │←──────────────────────────────│
```

### Usage

```rust
// Option 1: Use MpcSolanaProvider with any reqwest request
let provider = pay.mpp_provider();
let resp = reqwest::Client::new()
    .get("https://api.example.com/paid")
    .send_with_payment(&provider)
    .await?;

// Option 2: Use PayClient convenience methods (auto-detects MPP vs x402)
let resp = pay.get("https://api.example.com/paid").await?;
let resp = pay.post("https://api.example.com/action", Some(body), vec![]).await?;
```

## Send API (unified — NOT YET IMPLEMENTED)

```rust
/// One method to send anywhere.
pub async fn send(&mut self, chain: &str, address: &str, amount: u64, token: &str) -> Result<SendResult>
```

### Routing logic

```
send(chain, address, amount, token)
  │
  ├─ chain == "solana"?
  │   → MPC transfer (sign + relay)
  │
  └─ any other chain (eth, btc, base)?
      → OutLayer Intents cross-chain (deposit + withdraw + poll)
```

## Remaining Steps

### Step 1: `CustodyClient::send_cross_chain()`
- Check Intents balance
- If insufficient: `deposit_to_intents` + poll
- `withdraw` to destination chain + poll
- Returns `CrossChainResult { tx_hash, chain, address, amount }`

### Step 2: `PayClient::send()` — smart routing
- Derive MPC address for the chain
- Route to MPC path or Intents path
- Returns unified `SendResult`

## Blockers

- **Approval bottleneck**: `intents_withdraw` requires `kampouse.near` approval via dashboard. No programmatic approval API exists. Need policy change to auto-approve.
- **OutLayer Solana address endpoint broken**: `/wallet/v1/address?chain=solana` returns error, but Intents withdraw to Solana works. Use MPC-derived address instead.
- **MPC address has 0 SOL**: Balance gate will reject payments until funded. Auto-fund (`ensure_funded`) is written but blocked by Intents withdraw approval.

## API Surface (current)

```rust
// CustodyClient (low-level — OutLayer REST)
custody.balance_near()                        // NEAR balance
custody.balance_token(token)                  // FT balance
custody.address(chain)                        // chain address
custody.transfer(receiver_id, amount)         // NEAR native transfer
custody.call(contract, method, args, deposit) // NEAR contract call (MPC sign!)
custody.withdraw(addr, amount, token, chain)  // Intents cross-chain withdraw
custody.deposit_to_intents(token, amount)     // Deposit to Intents
custody.request_status(id)                    // Poll request status
custody.policy()                              // Read approval policy
custody.swap(from_token, to_token, amount)    // Token swap
custody.sign_message(message)                 // Sign arbitrary message

// MpcClient (Solana-specific)
mpc.derive_solana_address(path)               // View call to v1.signer
mpc.build_sol_transfer(from, to, lamports)    // Build unsigned Solana SOL tx
mpc.build_spl_transfer(from, to, mint, amount)// Build unsigned Solana SPL tx
mpc.build_transfer(from, to, amount, asset)   // Router: SOL or SPL
mpc.sign_transaction(tx, path)                // MPC sign via v1.signer
mpc.finalize_transaction(tx, from, sig)       // Attach signature
mpc.relay_to_solana(signed)                   // Broadcast to Solana RPC
mpc.transfer_sol(path, to, lamports)          // Full pipeline (build+sign+relay)
mpc.sol_balance(address)                      // Query SOL balance
mpc.derive_ata(owner, mint)                   // Derive Associated Token Account

// PayClient (high-level — MPP + x402 + MPC)
pay.get(url)                                  // GET with 402 auto-payment (MPP or x402)
pay.post(url, body, headers)                  // POST with 402 auto-payment
pay.transfer_sol(to, lamports)                // Direct Solana transfer
pay.fund_sol(amount_sol)                      // Fund MPC address via Intents
pay.sol_balance()                             // MPC address SOL balance
pay.sol_address()                             // MPC-derived Solana address
pay.mpp_provider()                            // Get MPP provider for custom use
pay.custody()                                 // Access underlying CustodyClient
pay.mpc()                                     // Access underlying MpcClient
```

## Test Suite (17 tests)

| Module | Count | Coverage |
|--------|-------|----------|
| mpc | 7 | Constants, pubkey validation, derivation path, sign args, ATA determinism, SPL ix encoding, transfer routing |
| x402 | 10 | MPP supports, MPP charge decode, x402 parse (SOL + USDC), x402 network detection, x402 payload serialization, MPP challenge roundtrip, MPP credential format, derivation path constant |

## File Structure

```
agent-pay/src/
├── lib.rs          — Re-exports: CustodyClient, MpcClient, PayClient, USDC_MINT, SOL_NATIVE
├── custody.rs      — OutLayer REST client (~350 lines)
├── mpc.rs          — NEAR MPC chain signatures + Solana tx building (~720 lines)
├── x402.rs         — MPP client + x402 client + MpcSolanaProvider + PayClient (~830 lines)
├── types.rs        — Shared types (RequestResponse, RequestEntry, PaidResponse)
└── error.rs        — Error types (Api, Http, X402, InsufficientBalance, Policy, NotRegistered)
```

## Dependencies

```toml
reqwest = { version = "0.12", features = ["json"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
solana-sdk = "2"
bincode = "1"
mpp = "0.10"
base64 = "0.22"
sha2 = "0.10"
hex = "0.4"
thiserror = "2"
tracing = "0.1"
tokio = { version = "1", features = ["sync"] }
```

No x402-rs, no solana-mpp — both conflict with solana-sdk v2 due to atomic solana crate version mismatches.
