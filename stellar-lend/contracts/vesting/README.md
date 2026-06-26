# Vesting Contract (stellarlend-vesting)

This contract implements on-ledger vesting for tokens with a configurable cliff, linear vesting duration, and administrative revocation. Unvested tokens are clawed back to a designated treasury address upon revocation.

## On-Ledger Interface

### `initialize(env, admin, treasury, token)`
Configures the contract instance parameters. Can only be initialized once.
- `admin`: The administrative address allowed to add and revoke grants.
- `treasury`: The destination address for clawed-back unvested tokens.
- `token`: The Stellar Asset/Token contract address being vested.

### `add_grant(env, grantee, total, start_seconds, duration_seconds, cliff_seconds)`
Creates a new vesting grant.
- Gated on `admin` authorization.
- Transfers `total` tokens from the `admin` account to the contract to escrow them.
- `grantee`: Address of the vesting recipient.
- `total`: Total tokens to vest.
- `start_seconds`: Timestamp (in seconds) when vesting begins.
- `duration_seconds`: Linear vesting period duration (in seconds).
- `cliff_seconds`: Duration of the cliff period (in seconds) starting from `start_seconds`.

### `claim(env, grantee)`
Claims all currently vested but unclaimed tokens.
- Gated on `grantee` authorization.
- Transfers claimable tokens from the contract to the `grantee` address.

### `revoke(env, grantee)`
Revokes a vesting grant.
- Gated on `admin` authorization.
- Calculates vested tokens up to the current block timestamp.
- Transfers all unvested tokens from the contract to the `treasury` address.
- Marks the grant as revoked and locks the total grant to the vested amount.

### View Functions
- `get_admin(env)`: Returns the admin address.
- `get_treasury(env)`: Returns the treasury address.
- `get_token(env)`: Returns the token address.
- `get_grant(env, grantee)`: Returns the `Grant` struct details for a grantee address.

---

## Storage Layout

The contract utilizes both `instance` and `persistent` storage layers:

### Instance Storage
Configured at initialization, stored as instance data:
- `DataKey::Admin`: `Address`
- `DataKey::Treasury`: `Address`
- `DataKey::Token`: `Address`

### Persistent Storage
Stored individually per grantee:
- `DataKey::Grant(Address)`: `Grant`
  - `grantee`: `Address`
  - `total`: `u128`
  - `claimed`: `u128`
  - `start_seconds`: `u64`
  - `duration_seconds`: `u64`
  - `cliff_seconds`: `u64`
  - `revoked`: `bool`

Every storage access or read of the `Grant` struct extends its TTL with a threshold/expiry window of up to 1,000,000 ledgers.
