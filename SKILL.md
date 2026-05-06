# SKILL.md — agent-pay

Multi-chain payment library + CLI for AI agents. NEAR custody (OutLayer) + MPC chain signatures (v1.signer) + pay.sh HTTP 402.

## Quick Start

```sh
# Build
cargo build --release

# Discover APIs (no key needed)
agent-pay search "ocr"
agent-pay list

# Pay for an API call
export OUTLAYER_API_KEY=wk_...
agent-pay quicknode/rpc '{"method":"getHealth"}'
echo '{"image":"..."}' | agent-pay alibaba/ocr-api

# Library
let mut pay = PayClient::new(CustodyClient::from_api_key(&key));
let resp = pay.get("https://api.example.com/data").await?;
let sent = pay.send("solana", &addr, "1000000", "sol").await?;
```

## Architecture

```
PayClient          High-level: get (auto-402), post, send (any-chain)
  ├── MpcClient    Solana: derive address, build tx, MPC sign, relay
  └── CustodyClient  OutLayer REST: wallet, balances, withdraw, cross-chain
```

Source layout:
- `src/main.rs` — CLI (search, list, info, call)
- `src/x402.rs` — PayClient + MPP/x402/Solana Charge + auto-fund (~1500 lines)
- `src/mpc.rs` — MPC signing + Solana tx building (~890 lines)
- `src/custody.rs` — OutLayer REST client (~440 lines)
- `src/types.rs` — Request/Response types
- `src/error.rs` — Error types

## Key Concepts

**HTTP 402 payment flow:** GET → 402 with payment requirements → detect MPP or x402 → build Solana tx → MPC sign → retry with credential → 200.

**Solana Charge (pay.sh wire format):** Nested `methodDetails` with `feePayer`, `feePayerKey`, `recentBlockhash`, `tokenProgram`, `decimals`. Pull mode (feePayer=true): sign tx, give to server. Push mode: sign + relay ourselves.

**SPL TransferChecked:** Instruction 13. 4 accounts (source writable, mint readonly, dest writable, authority signer). 13-byte data: 4-byte discriminator + 8-byte amount + 1-byte decimals.

**Server-provided blockhash:** pay.sh sends `methodDetails.recentBlockhash` — used directly instead of fetching our own. Saves RPC call, matches server expectations.

**Self-healing balance:** `ensure_funded()` checks MPC Solana address balance, auto-tops-up from OutLayer Intents when low.

## Testing

```sh
cargo test                                                # 19 unit tests
cargo test test_paysh_integration -- --ignored            # Parse real pay.sh 402
OUTLAYER_API_KEY=wk_... cargo test test_paysh_e2e -- --ignored --nocapture  # Full MPC sign + submit
```

## Known Issues

- OutLayer `intents_withdraw` returns `pending_approval` — blocks auto-fund and cross-chain sends until approved in dashboard.
- `x402-rs` and `solana-mpp` crates conflict with `solana-sdk v2` — we hand-roll x402, use `mpp` crate for MPP only.
- `PayClient::new(custody)` defaults to `testnet=true`. For mainnet, construct `MpcClient` manually with `testnet=false`.

## Publishing

Files synced to temp dir before git push (avoids .gitignore issues):
```sh
rsync -av src/ /var/folders/tf/qhb6jyw95v9fbwnvs30cc6y00000gn/T/agent-pay-publish/src/
cp Cargo.toml Cargo.lock README.md SKILL.md /var/folders/tf/.../T/agent-pay-publish/
cd /var/folders/tf/.../T/agent-pay-publish && git add -A && git commit -m "..." && git push
```
