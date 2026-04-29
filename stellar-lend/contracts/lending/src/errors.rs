//! # Lending Protocol Error Registry
//!
//! Maps every internal contract error to a stable `u32` discriminant that
//! SDK consumers and frontends can rely on across upgrades.
//!
//! ## Code Ranges
//! | Range       | Domain                  |
//! |-------------|-------------------------|
//! | 1000–1999   | Borrowing & Repayment   |
//! | 2000–2999   | Deposits                |
//! | 3000–3999   | Withdrawals             |
//! | 4000–4999   | Flash Loans             |
//! | 5000–5999   | Oracles                 |
//! | 6000–6999   | Cross-Asset Operations  |

use soroban_sdk::contracterror;

// ── Borrowing & Repayment (1000–1999) ─────────────────────────────────
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum BorrowError {
    InsufficientCollateral = 1001,
    DebtCeilingReached = 1002,
    ProtocolPaused = 1003,
    InvalidAmount = 1004,
    Overflow = 1005,
    Unauthorized = 1006,
    AssetNotSupported = 1007,
    BelowMinimumBorrow = 1008,
    RepayAmountTooHigh = 1009,
    Reentrancy = 1010,
    BorrowCapExceeded = 1011,
    InvalidBorrowCap = 1012,
    ExceedsDepositCap = 1013,
}

// ── Deposits (2000–2999) ──────────────────────────────────────────────
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum DepositError {
    InvalidAmount = 2001,
    DepositPaused = 2002,
    Overflow = 2003,
    AssetNotSupported = 2004,
    ExceedsDepositCap = 2005,
    Unauthorized = 2006,
    Reentrancy = 2007,
}

// ── Withdrawals (3000–3999) ───────────────────────────────────────────
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum WithdrawError {
    InvalidAmount = 3001,
    WithdrawPaused = 3002,
    Overflow = 3003,
    InsufficientCollateral = 3004,
    InsufficientCollateralRatio = 3005,
    Unauthorized = 3006,
    Reentrancy = 3007,
}

// ── Flash Loans (4000–4999) ───────────────────────────────────────────
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum FlashLoanError {
    InvalidAmount = 4001,
    InsufficientRepayment = 4002,
    Unauthorized = 4003,
    InvalidFee = 4004,
    CallbackFailed = 4005,
    Reentrancy = 4006,
    ProtocolPaused = 4007,
    BorrowCapExceeded = 4008,
}

// ── Oracles (5000–5999) ───────────────────────────────────────────────
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum OracleError {
    InvalidPrice = 5001,
    StalePrice = 5002,
    Unauthorized = 5003,
    NoPriceFeed = 5004,
    InvalidOracle = 5005,
    OraclePaused = 5006,
    OracleDecimalMismatch = 5007,
    OracleDecimalsNotConfigured = 5008,
}

// ── Cross-Asset Operations (6000–6999) ────────────────────────────────
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum CrossAssetError {
    InsufficientCollateral = 6001,
    DebtCeilingReached = 6002,
    ProtocolPaused = 6003,
    InvalidAmount = 6004,
    Overflow = 6005,
    Unauthorized = 6006,
    AssetNotSupported = 6007,
    PriceUnavailable = 6008,
    AlreadyInitialized = 6009,
    BorrowCapExceeded = 6010,
    InvalidBorrowCap = 6011,
}