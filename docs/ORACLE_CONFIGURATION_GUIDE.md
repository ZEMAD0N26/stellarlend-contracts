# Oracle Configuration Management Guide

## Overview

This document outlines the procedures for managing oracle configurations in the StellarLend protocol, including role separation, security considerations, and operational guidelines.

## Architecture Overview

### Components

1. **Off-chain Oracle Service** (TypeScript/Node.js)
   - Fetches prices from multiple external sources
   - Aggregates and validates price data
   - Updates smart contract with validated prices via authorized `update_price_feed` operations

2. **Smart Contract Oracle Module** (Rust/Soroban)
   - Stores on-chain price feeds
   - Enforces validation rules and role separation
   - Manages oracle configuration, decimals calibration, and per-asset staleness boundaries

3. **Price Providers**
   - CoinGecko (primary, 60% weight)
   - Binance (secondary, 40% weight)
   - CoinMarketCap (optional, 35% weight)

## Signed Oracle Price Updates

### Admin configuration

- `set_oracle_pubkey(pubkey)` registers the oracle's ed25519 public key.
- Only the contract admin may call this entrypoint.
- The stored key is used to verify all subsequent `set_price` updates.

### Price update entrypoint

- `set_price(caller, asset, price, timestamp, signature)` accepts a signed price update.
- The update is accepted only when:
  - the caller is admin,
  - the signature verifies against the registered oracle pubkey,
  - the timestamp is within the configured freshness window.

### Wire format for signed updates

The oracle signs the following payload exactly:

1. ASCII domain prefix: `stellar-lend:oracle-price`
2. 32-byte canonical asset address
3. 16-byte big-endian signed integer price
4. 8-byte big-endian unsigned integer timestamp

### Security notes

- The contract rejects stale or future timestamps.
- This scheme prevents an attacker with only the admin signer from forging prices without the oracle's ed25519 key.
