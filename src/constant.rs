use ethers::types::U256;
use lazy_static::lazy_static;

pub const REQUEST_RELAY_TIMEOUT: u64 = 15 * 60; // 15 minutes

// pub const RESPONSE_RELAY_TIMEOUT: u64 = 15 * 60; // 15 minutes
pub const MAX_GATEWAY_RETRIES: u8 = 2;

pub const GATEWAY_BLOCK_STATES_TO_MAINTAIN: u64 = 5;
pub const OFFEST_FOR_GATEWAY_EPOCH_STATE_CYCLE: u64 = 4; // 4 seconds
pub const WAIT_BEFORE_CHECKING_BLOCK: u64 = 5; // 5 seconds

lazy_static! {
    pub static ref MIN_GATEWAY_STAKE: U256 = U256::from(111111111111111110000 as u128);
}
