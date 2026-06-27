use crate::{LendingContract, LendingContractClient, PauseType};
use soroban_sdk::{
    contract, contractimpl,
    testutils::Address as _,
    Address, Bytes, Env,
};

fn setup() -> (Env, LendingContractClient<'static>, Address, Address, Address) {
    let env = Env::default();
    env.mock_all_auths();

    let lending_id = env.register(LendingContract, ());
    let client = LendingContractClient::new(&env, &lending_id);

    let admin = Address::generate(&env);
    let user = Address::generate(&env);
    let receiver = Address::generate(&env);

    client.initialize(&admin);

    // Seed treasury so flash_loan has liquidity to reach the pause/emergency
    // gate (the treasury check comes after those gates).
    let asset = Address::generate(&env);
    env.storage()
        .persistent()
        .set(&(crate::DataKey::Treasury(asset.clone())), &1_000_000i128);

    (env, client, admin, user, receiver)
}

fn set_flash_pause(
    env: &Env,
    client: &LendingContractClient<'static>,
    admin: &Address,
    paused: bool,
) {
    let expires_at = env.ledger().sequence().saturating_add(5);
    client.set_pause(admin, &PauseType::FlashLoan, &paused, &expires_at);
}

// ---------------------------------------------------------------------------
// Pause-gate tests
// ---------------------------------------------------------------------------

#[test]
#[should_panic(expected = "OperationPaused")]
fn flash_loan_rejected_when_granular_flash_pause_active() {
    let (env, client, admin, initiator, receiver) = setup();
    set_flash_pause(&env, &client, &admin, true);
    let asset = Address::generate(&env);
    client.flash_loan(&initiator, &receiver, &asset, &10, &Bytes::new(&env));
}

#[test]
#[should_panic(expected = "OperationPaused")]
fn repay_flash_loan_rejected_when_granular_flash_pause_active() {
    let (env, client, admin, payer, _receiver) = setup();
    set_flash_pause(&env, &client, &admin, true);
    let asset = Address::generate(&env);
    client.repay_flash_loan(&payer, &asset, &1);
}

#[test]
#[should_panic(expected = "OperationDisabledDuringShutdown")]
fn flash_loan_rejected_during_emergency_shutdown() {
    let (env, client, _admin, initiator, receiver) = setup();
    client.set_emergency_state(&crate::EmergencyState::Shutdown);
    let asset = Address::generate(&env);
    client.flash_loan(&initiator, &receiver, &asset, &10, &Bytes::new(&env));
}

#[test]
#[should_panic(expected = "OperationDisabledDuringShutdown")]
fn repay_flash_loan_rejected_during_emergency_shutdown() {
    let (env, client, _admin, payer, _receiver) = setup();
    client.set_emergency_state(&crate::EmergencyState::Shutdown);
    let asset = Address::generate(&env);
    client.repay_flash_loan(&payer, &asset, &1);
}

#[test]
fn flash_loan_allowed_when_unpaused_and_normal_emergency_state() {
    let (env, client, _admin, initiator, _receiver) = setup();

    set_flash_pause(&env, &client, &client.get_admin(), false);
    client.set_emergency_state(&crate::EmergencyState::Normal);

    let receiver_id = env.register(FlashReceiverOk, ());
    let asset = Address::generate(&env);

    env.storage()
        .persistent()
        .set(&(crate::DataKey::Treasury(asset.clone())), &1_000_000i128);

    env.storage().persistent().set(
        &(crate::DataKey::Balance(asset.clone(), receiver_id.clone())),
        &0i128,
    );

    client.flash_loan(&initiator, &receiver_id, &asset, &10, &Bytes::new(&env));
}

// ---------------------------------------------------------------------------
// Minimal compliant receiver for the success-path test above.
// ---------------------------------------------------------------------------

#[contract]
pub struct FlashReceiverOk;

#[contractimpl]
impl FlashReceiverOk {
    /// Repays `amount + fee` back to the lending contract.
    pub fn on_flash_loan(
        env: Env,
        initiator: Address,
        asset: Address,
        amount: i128,
        fee: i128,
        _params: Bytes,
    ) {
        let contract_id: Address = env.invoker();
        let total = amount.saturating_add(fee);
        let lending = LendingContractClient::new(&env, &contract_id);
        initiator.require_auth();
        lending.repay_flash_loan(&initiator, &asset, &total);
    }
}
