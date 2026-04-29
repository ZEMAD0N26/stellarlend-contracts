use soroban_sdk::{contracttype, Address, Env, Vec};
use crate::borrow::BorrowError;

/// Hard cap on simultaneously registered assets.
/// Keeps `get_supported_assets` bounded and deterministic on-chain.
pub const MAX_REGISTERED_ASSETS: u32 = 64;

#[contracttype]
#[derive(Clone)]
pub enum RegistryKey {
    AssetRegistry(Address),
    /// Ordered list of all registered asset addresses.
    AssetList,
}

pub fn is_registered(env: &Env, asset: &Address) -> bool {
    env.storage()
        .persistent()
        .get(&RegistryKey::AssetRegistry(asset.clone()))
        .unwrap_or(false)
}

pub fn require_registered_asset(env: &Env, asset: &Address) -> Result<(), BorrowError> {
    if !is_registered(env, asset) {
        return Err(BorrowError::AssetNotSupported);
    }
    Ok(())
}

/// Returns the ordered list of all currently registered asset addresses.
pub fn list_registered(env: &Env) -> Vec<Address> {
    env.storage()
        .persistent()
        .get(&RegistryKey::AssetList)
        .unwrap_or_else(|| Vec::new(env))
}

pub fn register(env: &Env, asset: &Address) -> Result<(), BorrowError> {
    if is_registered(env, asset) {
        return Err(BorrowError::InvalidAmount);
    }
    let mut list = list_registered(env);
    if list.len() >= MAX_REGISTERED_ASSETS {
        return Err(BorrowError::CapExceeded);
    }
    env.storage()
        .persistent()
        .set(&RegistryKey::AssetRegistry(asset.clone()), &true);
    list.push_back(asset.clone());
    env.storage()
        .persistent()
        .set(&RegistryKey::AssetList, &list);
    Ok(())
}

pub fn deregister(env: &Env, asset: &Address) -> Result<(), BorrowError> {
    if !is_registered(env, asset) {
        return Ok(());
    }
    env.storage()
        .persistent()
        .remove(&RegistryKey::AssetRegistry(asset.clone()));
    let mut list = list_registered(env);
    let mut new_list: Vec<Address> = Vec::new(env);
    for i in 0..list.len() {
        let a = list.get(i).unwrap();
        if a != *asset {
            new_list.push_back(a);
        }
    }
    env.storage()
        .persistent()
        .set(&RegistryKey::AssetList, &new_list);
    Ok(())
}