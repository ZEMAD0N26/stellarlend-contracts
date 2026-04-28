use soroban_sdk::{contracttype, BytesN};

#[derive(Clone)]
#[contracttype]
pub enum StorageKey {
    Admin,
    Config,
    GovernanceConfig,
    Guardian,
    EmergencyState,
    QueuedAction(BytesN<32>),
}

#[derive(Clone)]
#[contracttype]
pub struct Config {
    pub min_delay: u64,
    pub grace_period: u64,
}

#[derive(Clone)]
#[contracttype]
pub enum EmergencyState {
    Normal,
    Shutdown,
    Recovery,
}
