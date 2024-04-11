use ethers::signers::LocalWallet;
use ethers::types::{Address, Bytes};
use k256::ecdsa::SigningKey;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::RwLock;

use crate::common_chain_gateway_state_service::GatewayData;
use crate::common_chain_interaction::RequestChainData;

pub struct AppState {
    pub enclave_signer_key: SigningKey,
    pub wallet: Mutex<Option<LocalWallet>>,
    pub common_chain_id: u64,
    pub common_chain_http_url: String,
    pub common_chain_ws_url: String,
    pub gateway_contract_addr: Address,
    pub job_contract_addr: Address,
    pub chain_list: Mutex<Vec<RequestChainData>>,
    pub registered: Mutex<bool>,
    pub enclave_pub_key: Bytes,
    pub gateway_epoch_state: Arc<RwLock<BTreeMap<u64, BTreeMap<Bytes, GatewayData>>>>,
    pub epoch: u64,
    pub time_interval: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InjectKeyInfo {
    pub operator_secret: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RegisterEnclaveInfo {
    pub attestation: String,
    pub pcr_0: String,
    pub pcr_1: String,
    pub pcr_2: String,
    pub enclave_cpus: usize,
    pub enclave_memory: usize,
    pub timestamp: usize,
    pub stake_amount: usize,
    pub chain_list: Vec<u64>,
}
