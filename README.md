# agent-pay

Multi-chain payments for AI agents on NEAR.

One Rust crate. One wallet. Any chain. Pay for APIs with HTTP 402. Send SOL, ETH, BTC, or NEAR-native tokens to anyone. Self-healing balance management.

## How it works

agent-pay sits between your agent and the financial infrastructure. It wraps two NEAR primitives into a single payment layer:

**OutLayer Agent Custody** — a multi-chain wallet held in TEE. Holds NEAR, ETH, BTC, SOL balances. Exposes a REST API for transfers, swaps, and cross-chain withdrawals via NEAR Intents.

**NEAR MPC Chain Signatures** — the v1.signer contract derives and signs keys for non-NEAR chains. Your agent's NEAR account becomes the identity. No private keys anywhere.

```
your agent
    │
    ▼
┌──────────────────────────────────────┐
│  PayClient                           │
│                                      │
│  pay.get(url)     ← 402 auto-payment │
│  pay.send(...)    ← any-chain send   │
│  pay.fund_sol(x)  ← self-healing     │
├──────────────┬───────────────────────┤
│  MpcClient   │  CustodyClient        │
│  (Solana)    │  (OutLayer REST)      │
└──────┬───────┴──────────┬────────────┘
       │                  │
       ▼                  ▼
  Solana RPC         OutLayer API
  (sign + relay)     (NEAR custody)
```

## The three flows

### 1. Pay for API access (HTTP 402)

An API returns 402 with payment requirements. agent-pay detects the protocol, signs a Solana transaction via MPC, relays it, and retries the request with payment proof.

Two protocols supported:

**MPP** (IETF draft-ietf-httpapi-payments) — `WWW-Authenticate: Payment` header. Uses the `mpp` crate for header parsing. Our custom method is `solanampc`.

**x402** (Coinbase) — JSON body with `x402Version` + `accepts`. We parse the payment requirements, sign via MPC, and return a `X-Payment` base64 header.

```rust
let pay = PayClient::new(custody, mpc);

// GET a paid API — auto-detects MPP or x402, pays, returns data
let resp = pay.get("https://api.example.com/data").await?;
println!("Status: {}, Paid: {}", resp.status, resp.amount_paid);

// POST with auto-payment
let resp = pay.post("https://api.example.com/action", Some(body), vec![]).await?;
```

Under the hood:
```
GET /data → 402 + payment requirements
  │
  ├─ ensure_sol_address()  → derive MPC Solana address
  ├─ ensure_funded()       → check balance, top up if low
  ├─ execute_sol_payment() → build tx → MPC sign → relay
  └─ retry GET with payment proof → 200 + data
```

### 2. Send to any chain

One method. Automatic routing.

```rust
// Solana — MPC sign + relay (fast)
let result = pay.send("solana", "GCn6...WzViH", "1000000", SOL_NATIVE).await?;

// Ethereum — OutLayer Intents cross-chain bridge
let result = pay.send("ethereum", "0x7f...3aB", "500000", "wrap.near").await?;
```

Routing logic:
- `solana` → derive MPC address, auto-fund if needed, build/sign/relay directly
- any other chain → deposit tokens into NEAR Intents, withdraw to destination chain, poll for completion

Returns `SendResult { chain, address, amount, token, tx_hash }`.

### 3. Self-healing balance

The MPC-derived Solana address starts at 0 SOL. If it runs dry mid-payment, `ensure_funded()` automatically withdraws from OutLayer Intents to top it up.

```rust
// Called automatically before every payment
// Can also be called manually
pay.fund_sol("0.01").await?;
```

```
ensure_funded(sol_addr, needed)
  ├─ sol_balance(sol_addr) → current balance
  ├─ balance >= needed + 0.01 SOL reserve → done
  └─ balance < threshold → withdraw from Intents to sol_addr
```

## Signing stack

All Solana operations go through the same pipeline:

```
MpcClient
  │
  ├─ derive_solana_address("solana-1")
  │   → view call to v1.signer on NEAR → "GCn6...WzViH"
  │
  ├─ build_transfer(from, to, amount, asset)
  │   ├─ SOL_NATIVE → SystemProgram::transfer
  │   └─ mint addr  → Token Transfer (ix 12) + ATA derivation
  │
  ├─ sign_transaction(tx, "solana-1")
  │   → POST /wallet/v1/call → v1.signer payload_v2 Ed25519
  │   → 64-byte signature
  │
  ├─ finalize_transaction(tx, from, sig)
  │   → replace placeholder signature with real one
  │
  └─ relay_to_solana(signed)
      → POST Solana RPC sendTransaction → tx_hash
```

No private keys leave the MPC contract. The agent never sees key material.

## API surface

### PayClient (high-level)

```rust
pay.get(url)                               // GET with 402 auto-payment
pay.post(url, body, headers)               // POST with 402 auto-payment
pay.send(chain, address, amount, token)    // Send to any chain
pay.transfer_sol(to, lamports)             // Direct Solana transfer
pay.fund_sol(amount_sol)                   // Top up MPC address
pay.sol_balance()                          // MPC address SOL balance
pay.sol_address()                          // MPC-derived Solana address
```

### CustodyClient (OutLayer REST)

```rust
custody.register()                         // Create wallet
custody.balance_near()                     // NEAR balance
custody.balance_token(token)               // FT balance
custody.address(chain)                     // Chain address
custody.transfer(receiver, amount)         // NEAR native transfer
custody.call(contract, method, args, yocto)// NEAR contract call
custody.withdraw(addr, amount, token, chain)// Intents cross-chain withdraw
custody.deposit_to_intents(token, amount)  // Deposit to Intents
custody.send_cross_chain(to, amt, tok, ch) // Full pipeline: deposit+withdraw+poll
custody.swap(from, to, amount)             // Token swap
custody.sign_message(msg)                  // Sign arbitrary message
custody.policy()                           // Read approval policy
```

### MpcClient (Solana signing)

```rust
mpc.derive_solana_address(path)            // View call to v1.signer
mpc.build_sol_transfer(from, to, lamports) // Build SOL transfer tx
mpc.build_spl_transfer(from, to, mint, amt)// Build SPL token tx
mpc.build_transfer(from, to, amt, asset)   // Router: SOL or SPL
mpc.sign_transaction(tx, path)             // MPC sign via v1.signer
mpc.finalize_transaction(tx, from, sig)    // Attach signature
mpc.relay_to_solana(signed)                // Broadcast to Solana
mpc.transfer_sol(path, to, lamports)       // Full pipeline
mpc.sol_balance(address)                   // Query SOL balance
mpc.derive_ata(owner, mint)                // Derive Associated Token Account
```

## SPL tokens

SPL token support is built from raw instruction bytes — no extra Solana crates needed. `build_transfer` auto-routes:

```rust
// Native SOL
mpc.build_transfer(from, to, 1000000, SOL_NATIVE).await?;

// USDC
mpc.build_transfer(from, to, 1000000, USDC_MINT).await?;

// Any SPL token
mpc.build_transfer(from, to, 500000, "EPjFWdd5...wyTDt1v").await?;
```

Internally: `derive_ata()` for source/dest ATAs, then Token Program instruction 12 (Transfer) with 12-byte data encoding.

## File structure

```
src/
├── lib.rs       — Re-exports, Result type
├── custody.rs   — OutLayer REST client (~440 lines)
├── mpc.rs       — MPC signing + Solana tx building (~720 lines)
├── x402.rs      — PayClient + MPP + x402 + auto-fund (~920 lines)
├── types.rs     — Request/Response types, CrossChainResult, SendResult
└── error.rs     — Error types (Api, Http, X402, InsufficientBalance, Policy)
```

## Dependencies

```toml
reqwest = "0.12"          # HTTP client
solana-sdk = "2"          # Solana tx construction
mpp = "0.10"              # IETF 402 protocol
bincode = "1"             # Solana tx serialization
ed25519-dalek = "2"       # Ed25519 types
```

Notably absent: `x402-rs` and `solana-mpp`. Both have atomic crate conflicts with `solana-sdk v2`. We hand-roll x402 parsing and use the `mpp` crate for MPP only.

## Current blockers

1. **OutLayer withdraw approval** — `intents_withdraw` returns `pending_approval`. Requires manual approval in the OutLayer dashboard. No programmatic API. Blocks auto-fund and cross-chain sends until resolved.

2. **MPC address has 0 SOL** — The balance gate catches this cleanly with `InsufficientBalance`. Auto-fund is wired but blocked by #1. Manual SOL transfer to the MPC address works as a workaround.

---

## How to implement this in OutLayer directly

agent-pay is an external client that calls OutLayer's REST API. Everything it does could be moved inside OutLayer's TEE, eliminating the external dependency and making the whole flow trustless.

### What agent-pay does externally that OutLayer could do natively

| agent-pay (external) | OutLayer (native) |
|---|---|
| REST call to `/wallet/v1/call` for MPC signing | Direct contract call inside TEE |
| REST call to `/wallet/v1/address` for address derivation | Local key derivation in TEE |
| REST call to `/wallet/v1/withdraw` for Intents | Direct Intents contract interaction |
| External Solana RPC for tx relay | Built-in RPC relay from TEE |
| `ensure_funded()` balance management | Internal balance tracking + auto-refill |
| `poll_request()` for Intents status | Event-driven completion inside TEE |

### Why native is better

1. **No API key** — the TEE already holds the identity. No `wk_4f3e...` key to leak or rotate.

2. **No pending_approval** — the TEE IS the approval. Policy checks happen inside the enclave, not in a dashboard. Cross-chain sends become instant.

3. **No external signing roundtrip** — MPC signing is already inside the TEE. No REST call to yourself. Just call the contract directly.

4. **Self-healing is trivial** — the TEE can monitor its own Solana balance and refill from Intents without polling.

5. **x402/MPP becomes a TEE capability** — the agent asks "pay this API" and the TEE handles everything: detect protocol, sign, relay, retry.

### Implementation path

**Phase 1: Internal signing (remove the REST roundtrip)**

The TEE already has access to v1.signer. Instead of an HTTP call to `/wallet/v1/call`, the WASM module inside the TEE calls the MPC contract directly via NEAR runtime.

```
Current:  TEE → HTTP → OutLayer API → NEAR RPC → v1.signer → signature
Native:   TEE → NEAR runtime → v1.signer → signature
```

This removes the API key, the HTTP latency, and the trust-on-first-use REST auth.

**Phase 2: Built-in Solana relay**

The TEE adds a Solana RPC client. After signing, it relays directly instead of returning the signed tx to the external client.

```
Current:  TEE signs → returns signed tx → agent-pay relays to Solana
Native:   TEE signs → TEE relays to Solana → returns tx_hash
```

**Phase 3: Internal Intents**

Cross-chain sends happen inside the TEE. No `pending_approval` — the TEE's policy engine decides. No polling — event-driven via NEAR receipts.

```
Current:  agent-pay → REST withdraw → poll → poll → completed
Native:   TEE → NEAR Intents deposit → NEAR Intents withdraw → callback → done
```

**Phase 4: 402 as a TEE primitive**

The TEE runs the full 402 flow. The agent says "fetch this URL, pay up to X if needed". The TEE detects MPP vs x402, signs, relays, retries.

```rust
// Agent's perspective (inside TEE)
let data = custody.fetch_paid("https://api.example.com/data", max_sol: 0.001).await?;
```

The agent never sees the payment mechanics. It just gets the data.

**Phase 5: Policy as code**

Replace the dashboard approval with programmable policies:

```rust
// Auto-approve sends under 1 NEAR equivalent
policy.auto_approve(|req| req.usd_value() < 1.0);

// Require explicit approval for sends over 10 NEAR
policy.require_approval(|req| req.usd_value() >= 10.0);

// Whitelist specific API domains for 402
policy.allow_402_domain("api.openai.com");
policy.allow_402_domain("api.anthropic.com");
```

Policies execute inside the TEE. No human in the loop unless the policy says so.

### What agent-pay becomes

After native integration, agent-pay shrinks to a thin SDK:

```rust
// Before (external client, ~3000 lines)
let pay = PayClient::new(custody_client, mpc_client);
pay.get(url).await?;
pay.send(chain, addr, amt, token).await?;

// After (TEE SDK, ~200 lines)
let custody = outlayer::Custody::connect().await?;
custody.fetch_paid(url).await?;                    // 402
custody.send(chain, addr, amt, token).await?;      // any-chain
custody.balance(chain).await?;                      // balance check
```

All the signing, relaying, polling, and auto-funding moves inside the TEE. The SDK is just a typed interface to the TEE's capabilities.

### Migration order (recommended)

1. **Internal signing first** — biggest security win, removes API key dependency
2. **Built-in Solana relay** — removes external RPC dependency
3. **Internal Intents** — unblocks cross-chain, removes `pending_approval`
4. **402 primitive** — highest-level abstraction, depends on 1-3
5. **Policy engine** — can be done in parallel with 3-4

agent-pay can remain as the external client during migration. Once a phase is native, we swap the REST call for a TEE call. One phase at a time. No big bang.
