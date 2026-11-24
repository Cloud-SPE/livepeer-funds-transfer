# Livepeer Funds Transfer

A Rust-based automated service for managing Livepeer Protocol token (LPT) staking operations on Arbitrum. This application periodically transfers bonded LPT tokens and withdraws accumulated fees from an orchestrator wallet to designated recipient addresses.

## Table of Contents

- [Overview](#overview)
- [Architecture](#architecture)
- [Features](#features)
- [Prerequisites](#prerequisites)
- [Installation](#installation)
- [Configuration](#configuration)
- [Building](#building)
- [Testing](#testing)
- [Running](#running)
- [Docker Deployment](#docker-deployment)
- [Operations](#operations)
- [Security Considerations](#security-considerations)
- [Troubleshooting](#troubleshooting)

## Overview

This service automates Livepeer orchestrator operations by monitoring the Livepeer round lifecycle and executing actions based on on-chain round state.

The application performs the following tasks:

1. **Reward Claiming**: Calls `rewardWithHint()` exactly once per round when a new round is initialized
2. **Bond Transfer**: Transfers bonded LPT from the orchestrator wallet once the round is locked, while retaining a minimum bonded balance
3. **Fee Withdrawal**: Withdraws accumulated ETH fees when they exceed a configurable threshold

The application runs continuously using a polling loop and safely retries failed actions on subsequent iterations.

## Architecture

### Components

```
livepeer-funds-transfer/
├── Cargo.toml              # Rust dependencies and project metadata
├── Dockerfile              # Multi-stage Docker build configuration
├── src/
│   ├── bin/
│   │   └── funds_transfer.rs    # Main application logic
│   ├── lib.rs
│   ├── client.rs                # signing provider + receipt polling
│   ├── config.rs                # environment configuration
│   ├── contracts.rs             # alloy sol! bindings
│   ├── reward_hints.rs          # rewardWithHint position hints + reward math
│   ├── mock_rpc.rs              # scripted JSON-RPC transport (tests only)
│   └── abi/                     # From livepeer/protocol deployments/arbitrumMainnet
│       ├── BondingManager.json
│       ├── Controller.json
│       ├── Minter.json
│       └── RoundsManager.json
├── tests/
│   └── reward_hints_fork.rs     # Arbitrum fork test (anvil)
└── README.md
```

### Key Dependencies

- **alloy**: Ethereum/Arbitrum contract bindings (`sol!`), JSON-RPC provider, local keystore signing
- **tokio**: Async runtime
- **tracing** / **tracing-subscriber**: Structured logging and diagnostics

### Smart Contract Integration

The application interacts with two Livepeer Protocol smart contracts on Arbitrum:

- **BondingManager** (`0x35Bcf3c30594191d53231E4FF333E8A770453e40`): Manages staking, bonding, and fee operations
- **RoundsManager** (`0xdd6f56DcC28D3F5f27084381fE8Df634985cc39f`): Manages protocol rounds and their states

## Features

- **Round-Aware Rewarding**: Calls `rewardWithHint()` once per initialized round using on-chain state to prevent duplicates
- **Reward Hints**: Computes the orchestrator's new position in the transcoder pool so the contract skips its linear search (see [Reward Hints](#reward-hints))
- **Automated LPT Transfer**: Transfers all excess bonded LPT while retaining a minimum bonded amount on the orchestrator
- **Fee Management**: Withdraws accumulated ETH fees above a configurable threshold
- **Round Safety**: Bond transfers and fee withdrawals only occur when the round is locked
- **Polling-Based Execution**: Runs continuously and reacts to round state changes
- **Keystore-Based Signing**: Uses encrypted JSON keystore and passphrase files (no private keys in env vars)
- **Configurable**: All operational parameters controlled via environment variables
- **Logging**: Structured logging suitable for auditing and monitoring
- **Docker Support**: Production-ready containerization
- **Error Handling**: No panics; failures are logged and retried on the next polling cycle

## Prerequisites

### Development

- Rust 1.90.0 or later
- Cargo (comes with Rust)
- Access to an Arbitrum RPC endpoint
- Encrypted Ethereum keystore file (JSON format)
- Passphrase file for keystore decryption

### Production

- Docker (for containerized deployment)
- Kubernetes/orchestration platform (optional)

## Installation

### Clone the Repository

```bash
git clone <repository-url>
cd livepeer-funds-transfer
```

### Install Rust (if not already installed)

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source $HOME/.cargo/env
```

## Configuration

### Environment Variables

Configuration is read from environment variables only; the binary does not load a `.env`
file itself. Keeping them in a `.env` file works with Docker (`--env-file .env`, or `env_file:`
in Compose) or by exporting it into your shell (see [Local Execution](#local-execution)):

```bash
# Logging configuration
RUST_LOG=funds_transfer=info

# Arbitrum RPC endpoint
HTTP_RPC_URL=https://arb1.arbitrum.io/rpc

# Path to encrypted keystore file
JSON_KEY_FILE=/path/to/keystore.json

# Path to keystore passphrase file
PASSPHRASE_FILE=/path/to/passphrase.txt

# Optional: orchestrator address (derived from keystore if omitted)
# ORCHESTRATOR_ADDR=0xYourOrchestratorAddress

# Recipient address for transferred bonded LPT
LPT_RECEIVER_ADDR=0xYourStakeRecipientAddress

# Minimum bonded LPT to retain on the orchestrator (wei)
# Example: 1 LPT = 1000000000000000000
LPT_MIN_RETAIN_WEI=1000000000000000000

# Recipient address for withdrawn ETH fees
ETH_FEE_RECEIVER_ADDR=0xYourFeeRecipientAddress

# Minimum ETH fees required before withdrawal (wei)
# Example:          0.03 ETH = 30000000000000000
ETH_FEE_WITHDRAW_THRESHOLD_WEI=30000000000000000

# Chain ID (Arbitrum One = 42161)
CHAIN_ID=42161

# Polling interval in seconds
LOOP_SLEEP_SECS=60

```

### Configuration Details

#### HTTP_RPC_URL
- Public Arbitrum RPC: `https://arb1.arbitrum.io/rpc`
- Consider using a dedicated RPC provider (Infura, Alchemy, QuickNode) for production
- Rate limits may apply to public endpoints

#### Wallet Setup

1. **Generate Keystore**:
   ```bash
   # Using geth or similar tool
   geth account new --keystore ./keystore
   ```

2. **Create Passphrase File**:
   ```bash
   echo "your-secure-passphrase" > passphrase.txt
   chmod 600 passphrase.txt
   ```

3. **Security Note**: Never commit keystore files or passphrases to version control

#### Address Configuration

- **ORCHESTRATOR_ADDR** (optional): The orchestrator wallet that holds bonded LPT and accumulates fees. Defaults to the keystore's address.
- **LPT_RECEIVER_ADDR**: Destination for transferred LPT bonds (typically your main staking wallet)
- **ETH_FEE_RECEIVER_ADDR**: Destination for withdrawn ETH fees (can be the same as bond recipient)

#### Third-Party Bonder Authorization

`transferBond` bonds the signer's stake to `LPT_RECEIVER_ADDR`, which makes the signer a third-party bonder for that receiver. Once Livepeer enforces third-party bonding authorization ([forum post](https://forum.livepeer.org/t/upcoming-breaking-protocol-change-third-party-bonding-will-require-authorization/3401)), the transfer only succeeds if the receiver has authorized the signer. The receiver does this once, from its own wallet, on the BondingManager proxy `0x35Bcf3c30594191d53231E4FF333E8A770453e40`:

```
setThirdPartyBonderAuthorization(<signer address>, true)
```

When `ENABLE_TRANSFER_BOND=true`, the app checks `isThirdPartyBonderAuthorized(LPT_RECEIVER_ADDR, signer)` at startup, before the main loop. If the authorization is in place it logs:

```
INFO funds_transfer: transferBond authorization OK: LPT_RECEIVER_ADDR=0x… has authorized signer=0x… as a third-party bonder
```

If it is missing, the app **refuses to start** (exit code 1, before any transaction) and says how to fix it:

```
ERROR funds_transfer: refusing to start: authorization missing: LPT_RECEIVER_ADDR=0x… has not authorized signer=0x… as a third-party bonder, so transferBond fails once the protocol enforces third-party bonding authorization. Fix: from 0x…, call setThirdPartyBonderAuthorization(0x…, true) on BondingManager 0x…, then restart. To run without transfers, set ENABLE_TRANSFER_BOND=false.
```

It also refuses to start when the check itself fails (for example an RPC error), since the authorization cannot be confirmed. Under `restart: unless-stopped` Docker retries, so a brief RPC outage clears on its own. While the app is not running, it calls no rewards either, so fix the authorization promptly, or set `ENABLE_TRANSFER_BOND=false` to keep rewards and fee withdrawals running without transfers. The check is skipped when transfers are disabled or `LPT_RECEIVER_ADDR` is the signer itself.

#### Operational Parameters

All operational parameters are configurable via environment variables:

- **LPT_MIN_RETAIN_WEI**  
  Minimum bonded LPT that must remain on the orchestrator after transfers. An empty value is rejected at startup (it would otherwise read as 0 and transfer the entire stake); set `"0"` explicitly if that is really intended.

- **ETH_FEE_WITHDRAW_THRESHOLD_WEI**  
  ETH fees must meet or exceed this value before withdrawal.

- **LOOP_SLEEP_SECS**  
  Polling interval. Failed actions are retried on the next loop iteration.

- **MAX_FEE_BASE_MULTIPLIER** (default `2`)  
  Transactions are sent with `maxFeePerGas = baseFee * MAX_FEE_BASE_MULTIPLIER + MAX_PRIORITY_FEE_WEI`, using the latest block's base fee. Raise it if transactions sit pending during base-fee spikes.

- **MAX_PRIORITY_FEE_WEI** (default `0`)  
  Priority fee (tip). Arbitrum does not pay tips to the sequencer, so `0` is normally correct.

These values can be adjusted without rebuilding the application.

## Building

### Development Build

```bash
cargo build
```

### Release Build (Optimized)

```bash
cargo build --release
```

The compiled binary will be located at:
- Debug: `./target/debug/funds_transfer`
- Release: `./target/release/funds_transfer`

### Build with Specific Features

```bash
# With all features
cargo build --all-features

# Check for compilation errors without building
cargo check
```

## Testing

```bash
# Unit + property tests (offline)
cargo test

# Fork test: sends real rewardWithHint txs for every active orchestrator on an
# anvil fork of Arbitrum and checks each lands exactly at the hinted position
# with the predicted reward. Needs Foundry's anvil and network access; ~30 min
# for the full pool. The public RPC may prune the fork block's state before a full
# run ends; use an archive RPC or split it with FORK_TEST_SKIP / FORK_TEST_LIMIT.
cargo test --test reward_hints_fork -- --ignored --nocapture
FORK_TEST_LIMIT=10 ARB_RPC_URL=https://your-rpc cargo test --test reward_hints_fork -- --ignored --nocapture
```

The unit tests pin the reward math to real mainnet `Reward` events, and a property
test checks that hints always satisfy `SortedDoublyLL.validInsertPosition` on random pools.

## Running

### Local Execution

1. **Set up environment** (export the variables from your `.env` file):
   ```bash
   set -a; source .env; set +a
   ```

2. **Run the application**:
   ```bash
   # Using cargo (development)
   cargo run

   # Or run the compiled binary
   ./target/release/funds_transfer
   ```

3. **With custom logging**:
   ```bash
   RUST_LOG=debug cargo run
   ```

### Application Behavior

The application operates in a continuous loop:

1. Loads configuration and decrypts the keystore
2. Polls the Livepeer `RoundsManager` contract to determine:
   - Current round number
   - Whether the round is initialized
   - Whether the round is locked
3. If the round is initialized:
   - Calls `rewardWithHint()` once per round if it has not already been called
4. If the round is locked:
   - Transfers bonded LPT in excess of `LPT_MIN_RETAIN_WEI`
   - Withdraws ETH fees if they exceed the configured threshold
5. Sleeps for `LOOP_SLEEP_SECS`
6. Repeats

If a transaction fails, the error is logged and the operation is retried on the next polling cycle.

### Stopping the Application

- Press `Ctrl+C` to gracefully stop
- The application will complete its current operation before exiting

## Docker Deployment

The image runs as an unprivileged user, UID/GID **10001**, and expects the keystore and passphrase files under **`/keys`**. Both files must be readable by UID or GID 10001. For volumes created for older images, see [Upgrading from 0.0.7 or Earlier](#upgrading-from-007-or-earlier).

### Building the Docker Image

```bash
docker build -t livepeer-funds-transfer:latest .
```

`.dockerignore` is an allow-list: only `Cargo.toml`, `Cargo.lock` and `src/` are sent to the build, so keys, `.env` files and deployment stacks never reach it.

### Running with Docker

With `JSON_KEY_FILE=/keys/keystore.json` and `PASSPHRASE_FILE=/keys/passphrase.txt` in `.env`:

```bash
# One time: let the container user read the key files
sudo chown -R 10001:10001 /path/to/keys
sudo chmod 700 /path/to/keys && sudo chmod 600 /path/to/keys/*

docker run -d \
  --name livepeer-funds-transfer \
  --env-file .env \
  --read-only --cap-drop ALL --security-opt no-new-privileges:true \
  -v /path/to/keys:/keys:ro \
  livepeer-funds-transfer:latest
```

The app writes nothing to disk, so a read-only root filesystem, no Linux capabilities and a read-only key mount all work.

### Docker Compose

`docker-compose.yml` in this repo is a complete example. The parts that matter:

```yaml
services:
  funds-transfer:
    image: tztcloud/livepeer-funds-transfer:0.0.9
    restart: unless-stopped
    read_only: true
    cap_drop: [ALL]
    security_opt: ["no-new-privileges:true"]
    environment:
      JSON_KEY_FILE: "/keys/orchestrator_keystore.json"
      PASSPHRASE_FILE: "/keys/orchestrator_passphrase.txt"
      # ...the rest of the configuration
    volumes:
      - lpData:/keys:ro
volumes:
  lpData:
    external: true
```

Run with:
```bash
docker compose up -d
```

### Upgrading from 0.0.7 or Earlier

Images up to 0.0.7 ran as root, and stacks mounted the key volume at `/root/keys`. From 0.0.8 the app runs as UID 10001, which cannot enter `/root` or read root-only (`600`) files. An unchanged stack stops at startup, before any RPC call or transaction:

```
Error: Wallet("failed to read PASSPHRASE_FILE=/root/keys/eth-secret.txt: Permission denied (os error 13)")
```

With `restart: unless-stopped` the container then restart-loops. To upgrade:

1. **Let UID/GID 10001 read the two files in the existing volume.** This is a one-time change. Root stays the owner, so anything else that uses the volume is unaffected, and it is safe to run while the old root container is still running.
   ```bash
   docker run --rm -v <volume>:/v debian:trixie-slim sh -c \
     'chgrp 10001 /v /v/key.json /v/eth-secret.txt && chmod g+x /v && chmod g+r /v/key.json /v/eth-secret.txt'
   ```
   Use your own file names. If only this app uses the volume, `chown -R 10001:10001 /v` works too.
2. **Mount the volume at `/keys`, read-only, and point the paths there:**
   ```yaml
   volumes:
     - <volume>:/keys:ro
   environment:
     - JSON_KEY_FILE=/keys/key.json
     - PASSPHRASE_FILE=/keys/eth-secret.txt
   ```
3. **Optionally harden the service** with `read_only: true`, `cap_drop: [ALL]` and `security_opt: ["no-new-privileges:true"]`.

Stacks written for 0.0.1–0.0.2 also use older variable names. Rename them; the app does not read the old ones and stops at startup naming the first missing variable:

| Old name | Current name |
|---|---|
| `RPC_ENDPOINT_URL` | `HTTP_RPC_URL` |
| `ORCH_ETH_ADDR` | `ORCHESTRATOR_ADDR` (optional) |
| `TRANSFER_BOND_RECIPIENT_ETH_ADDR` | `LPT_RECEIVER_ADDR` |
| `ETH_FEE_RECIPIENT_ETH_ADDR` | `ETH_FEE_RECEIVER_ADDR` |
| `LPT_RESERVE_AMOUNT` (LPT, e.g. `1125.0`) | `LPT_MIN_RETAIN_WEI` (wei, e.g. `1125000000000000000000`) |

The current version also requires `ROUNDS_MANAGER_ADDR`, `BONDING_MANAGER_ADDR` and `ETH_FEE_WITHDRAW_THRESHOLD_WEI`; see [Environment Variables](#environment-variables).

### Image Build

The Dockerfile uses a multi-stage build:
1. **Builder stage** (`rust:1.97.1-trixie`): `cargo install --locked` builds exactly the dependency versions in `Cargo.lock`
2. **Runtime stage** (`debian:trixie-slim`): only the binary, `ca-certificates` (TLS to the RPC is verified against the OS trust store) and the unprivileged `app` user

Both stages are pinned to Debian 13 (trixie), so the runtime's glibc matches the one the binary was built against. The runtime image is about 95 MB.

## Operations

### Monitoring

#### Log Messages

The application provides detailed logging at various levels:

```
2025-12-20T14:40:54.859032Z  INFO funds_transfer: starting funds_transfer: chain_id=42161 rounds_manager=0xdd6f56dcc28d3f5f27084381fe8df634985cc39f bonding_manager=0x35bcf3c30594191d53231e4ff333e8a770453e40 sleep_secs=60 flags(reward=true, transfer_bond=true, withdraw_fees=true)
2025-12-20T14:40:55.527227Z  INFO funds_transfer: orchestrator/signer address: 0xYourOrchAddress
2025-12-20T14:40:55.756562Z  INFO funds_transfer: round state changed: round=4035 initialized=true locked=false
```

#### Viewing Logs

```bash
# Docker logs
docker logs -f livepeer-funds-transfer

# Docker Compose logs
docker-compose logs -f

# System journal (if running as systemd service)
journalctl -u livepeer-funds-transfer -f
```

### Transaction Verification

After each transaction, verify on Arbiscan:
- Bond transfers: Check BondingManager contract events
- Fee withdrawals: Check ETH transfers to recipient address

### Health Checks

Monitor for:
- Regular log output every 60 seconds
- Successful transaction hashes
- No error messages in logs
- Recipient wallet balances increasing

### Operational Scenarios

#### Scenario 1: Normal Operation
```
2025-12-20T18:29:38.310281Z  INFO funds_transfer: round state changed: round=4035 initialized=true locked=true
2025-12-20T18:29:38.404593Z  INFO funds_transfer: transferBond sending: round=4035 from_orchestrator=0xYourOrchAddress to_receiver=0xYourStakeOrTreasuryAddress amountWei=1083763440135192443657
2025-12-20T18:29:39.201740Z  INFO funds_transfer: transferBond tx sent: round=4035 tx_hash=0xc5....
2025-12-20T18:29:39.541239Z  INFO funds_transfer: transferBond confirmed: round=4035 tx_hash=0xc5... status=Some(1) block=Some(412709680) gas_used=Some(532629)
2025-12-20T18:29:39.587134Z  INFO funds_transfer: withdrawFees sending: round=4035 from_orchestrator=0xYourOrchAddress to_receiver=0xYourFeeRecipientAddress amountWei=7200000000000000
2025-12-20T18:29:40.193946Z  INFO funds_transfer: withdrawFees tx sent: round=4035 tx_hash=0x93eec...
2025-12-20T18:29:40.529429Z  INFO funds_transfer: withdrawFees confirmed: round=4035 tx_hash=0x93eec... status=Some(1) block=Some(412709684) gas_used=Some(153100)
```

### Reward Hints

The transcoder pool is a linked list sorted by stake. `reward()` re-inserts the
orchestrator with its new stake by walking the list from the top; `rewardWithHint(prev, next)`
skips the walk when given the correct neighbours. Before each reward the app:

1. Predicts the reward exactly as the contracts do: `Minter.currentMintableTokens` ×
   the orchestrator's earnings-pool stake ÷ `currentRoundTotalActiveStake` (MathUtilsV2,
   1e9 precision), minus `treasuryRewardCutRate`
2. Walks the pool from the head until it reaches the first orchestrator with less stake
   than `currentStake + reward`
3. Logs `reward hints: prev=… next=… stakeWei=… expectedRewardWei=…` and sends `rewardWithHint`

Stale or wrong hints never cause a revert; the contract falls back to searching from
them. If computing hints fails, the app sends `rewardWithHint(0x0, 0x0)`, which is
identical to `reward()`. Savings grow with pool depth. Measured on an Arbitrum fork
across all 100 pool positions: ~4% gas for the top 10, ~30% for positions 30–59, ~43%
for 60–87 and ~49% for the bottom 12 (≈1.02M → ≈515k gas); 33% overall. With hints, gas
stays at ~490–550k regardless of position.

## Security Considerations

### Key Management

1. **Keystore Protection**:
   - Store keystore files in secure locations with restricted permissions (`chmod 600`)
   - Use strong passphrases (20+ characters, mixed case, numbers, symbols)
   - Never commit keystores to version control
   - Consider hardware wallets for production environments

2. **Passphrase Security**:
   - Store passphrase files separately from keystores
   - Use environment variables or secrets management systems in production
   - Rotate passphrases regularly

### Network Security

1. **RPC Endpoints**:
   - Use authenticated RPC endpoints when possible
   - Consider running your own Arbitrum node for maximum security
   - Implement rate limiting and request filtering

2. **Firewall Rules**:
   - Restrict outbound connections to necessary RPC endpoints
   - Block all inbound connections except monitoring

### Operational Security

1. **Minimum Privilege**:
   - The Docker image runs as UID 10001, not root, and works with a read-only filesystem, no capabilities and a read-only key mount (see [Docker Deployment](#docker-deployment))
   - Run the application with minimal necessary permissions
   - Use dedicated service accounts
   - Implement principle of least privilege

2. **Monitoring**:
   - Set up alerts for failed transactions
   - Monitor wallet balances regularly
   - Track gas costs and fee withdrawals

3. **Auditing**:
   - Review logs regularly
   - Verify all transactions on-chain
   - Maintain audit trail of configuration changes

### Smart Contract Risks

1. **Contract Addresses**: The hardcoded Livepeer contract addresses are for Arbitrum mainnet. Verify these before deployment.
2. **Protocol Upgrades**: Monitor Livepeer governance for protocol upgrades that might affect these addresses
3. **Gas Prices**: Fees are derived from the latest base fee (see `MAX_FEE_BASE_MULTIPLIER`); gas limits come from `eth_estimateGas`

## Troubleshooting

### Common Issues

#### 1. "HTTP_RPC_URL missing"
**Cause**: Environment variable not set
**Solution**: 
```bash
export HTTP_RPC_URL=https://arb1.arbitrum.io/rpc
# Or add to .env file
```

#### 2. "failed to read PASSPHRASE_FILE=…" or "failed to decrypt JSON_KEY_FILE=…"
The message names the path the app tried.

**"No such file or directory"**: nothing is at that path inside the container.
- Check `JSON_KEY_FILE` / `PASSPHRASE_FILE` point under the mount point, e.g. `/keys/key.json`
- Check the volume line uses a colon: `- <volume>:/keys:ro`. With a comma (`/keys,ro`) Docker mounts the volume at a directory literally named `/keys,ro`, and `/keys` stays empty
- An unset `${PASSPHRASE_FILE}` in a compose file becomes an empty string; the app rejects that at startup with "must not be empty"

**"Permission denied"**: see [#6](#6-failed-to-read-passphrase_file-permission-denied-os-error-13).

**Decrypt errors** (for example a MAC mismatch): the passphrase does not match the keystore, or the keystore is corrupted. A trailing newline in the passphrase file is ignored.

#### 3. "Transfer Bond Transaction failed"
**Causes**:
- Insufficient gas
- Round not properly locked
- Invalid recipient address
- Insufficient LPT balance

**Solution**: 
- Check wallet has enough ETH for gas
- Verify round status on Livepeer Explorer
- Confirm recipient address is valid
- Review logs for specific error messages

#### 4. "gas required exceeds allowance"
**Cause**: `eth_estimateGas` caps gas at `walletBalance / maxFeePerGas`. Versions up to 0.0.5 used ethers' mainnet fee defaults (a 3 gwei minimum tip), which is ~100x the Arbitrum base fee, so a wallet holding less than ~0.003 ETH could not estimate `reward()` (~700k–900k gas).
**Solution**: 
- Upgrade to a version that prices fees from the base fee (`MAX_FEE_BASE_MULTIPLIER`)
- Keep a small ETH buffer on the orchestrator wallet for gas

#### 5. Panic at startup: "No CA certificates were loaded from the system"
**Cause**: The HTTP client verifies the RPC's TLS certificate against the operating system's
trust store, and none is installed (for example a custom image built `FROM debian` without
`ca-certificates`). The binary panics before the first RPC call, so a container with
`restart: unless-stopped` restart-loops.
**Solution**: Install `ca-certificates` in the runtime image, as the provided Dockerfile does.

#### 6. "failed to read PASSPHRASE_FILE=…: Permission denied (os error 13)"
**Cause**: The container runs as UID 10001 and cannot read the key files: they are root-only, or mounted under `/root` (for example a stack written for a root image).
**Solution**: Grant UID/GID 10001 read access and mount the files at `/keys`; see [Upgrading from 0.0.7 or Earlier](#upgrading-from-007-or-earlier).

#### 7. "refusing to start: authorization missing"
**Cause**: `ENABLE_TRANSFER_BOND=true`, and `LPT_RECEIVER_ADDR` has not authorized the signer as a third-party bonder. Once Livepeer enforces the authorization, every `transferBond` would fail, so the app stops at startup instead. No rewards are called while it is down.
**Solution**: From the receiver's wallet, call `setThirdPartyBonderAuthorization(<signer>, true)` on the BondingManager, then restart; see [Third-Party Bonder Authorization](#third-party-bonder-authorization). To keep rewards and fee withdrawals running in the meantime, set `ENABLE_TRANSFER_BOND=false`.
If the message is instead `isThirdPartyBonderAuthorized() failed`, the check could not reach the contract: see [Connection timeouts](#8-connection-timeouts).

#### 8. Connection timeouts
**Cause**: RPC endpoint unreachable or rate limited
**Solution**:
- Try alternative RPC endpoint
- Use authenticated/paid RPC service
- Increase timeout values (requires code modification)

#### 9. "Current Round is not locked"
**Cause**: Normal operation - rounds are not always locked
**Solution**: Wait for next round lock (rounds lock periodically in Livepeer protocol)

### Debug Mode

Enable verbose logging:
```bash
RUST_LOG=debug cargo run
```

Or for even more detail:
```bash
RUST_LOG=trace cargo run
```

### Testing Without Transactions

To test configuration without executing real transactions, you can:
1. Use a testnet (change CHAIN_ID to 421614 for Arbitrum Sepolia)
2. Modify the code to add a "dry-run" mode
3. Use a test wallet with minimal funds

### Getting Help

1. Check application logs first
2. Verify all environment variables are set correctly
3. Test RPC connectivity: `curl -X POST $HTTP_RPC_URL -H "Content-Type: application/json" -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}'`
4. Verify wallet address has necessary permissions in Livepeer protocol
5. Review Livepeer Protocol documentation for round and staking mechanics

### Using Alternative Networks

For Arbitrum Sepolia testnet:
```bash
CHAIN_ID=421614
HTTP_RPC_URL=https://sepolia-rollup.arbitrum.io/rpc
```

Update contract addresses in code if different on testnet.

## Performance Considerations

- **RPC Rate Limits**: The 30-minute polling interval is conservative. Adjust based on your RPC provider's limits
- **Gas Costs**: Monitor gas prices on Arbitrum - typically very low compared to mainnet
- **Memory Usage**: Application uses minimal memory (~10-20MB) during operation
- **CPU Usage**: Negligible except during transaction signing

## License

MIT

## Contributing

[Add contribution guidelines if applicable]

## Support

For issues related to:
- **This application**: [Create an issue in the repository]
- **Livepeer Protocol**: Visit [Livepeer Discord](https://discord.gg/livepeer)
- **Arbitrum Network**: Visit [Arbitrum Discord](https://discord.gg/arbitrum)

---

**Disclaimer**: This software is provided as-is. Always test thoroughly in a testnet environment before deploying to production with real funds. The authors assume no liability for any losses incurred through the use of this software.
