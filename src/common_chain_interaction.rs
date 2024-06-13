use actix_web::web::Data;
use anyhow::{Context, Result};
use async_recursion::async_recursion;
use ethers::abi::{decode, Address, ParamType};
use ethers::prelude::*;
use ethers::providers::Provider;
use ethers::utils::keccak256;
use hex::FromHex;
use k256::ecdsa::SigningKey;
use log::{error, info};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error;
use std::sync::RwLock;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::{task, time};

use crate::chain_util::{
    sign_job_response_request, sign_reassign_gateway_relay_request, sign_relay_job_request,
    LogsProvider,
};
use crate::common_chain_gateway_state_service::gateway_epoch_state_service;
use crate::constant::{
    GATEWAY_BLOCK_STATES_TO_MAINTAIN, MAX_GATEWAY_RETRIES, MIN_GATEWAY_STAKE,
    OFFEST_FOR_GATEWAY_EPOCH_STATE_CYCLE, REQUEST_RELAY_TIMEOUT,
};
use crate::contract_abi::{GatewayJobsContract, GatewaysContract};
use crate::model::{
    AppState, ContractsClient, GatewayData, GatewayJobType, Job, RegisterType, RegisteredData,
    RequestChainClient, ResponseJob,
};
use crate::HttpProvider;

impl ContractsClient {
    pub async fn new(
        enclave_owner: H160,
        enclave_signer_key: SigningKey,
        enclave_address: H160,
        signer: LocalWallet,
        common_chain_ws_url: &String,
        common_chain_http_provider: Arc<HttpProvider>,
        gateways_contract_addr: &H160,
        gateway_jobs_contract_addr: &H160,
        gateway_epoch_state: Arc<RwLock<BTreeMap<u64, BTreeMap<Address, GatewayData>>>>,
        request_chain_ids: HashSet<u64>,
        request_chain_clients: HashMap<u64, Arc<RequestChainClient>>,
        epoch: u64,
        time_interval: u64,
        gateway_epoch_state_waitlist: Arc<RwLock<HashMap<u64, Vec<Job>>>>,
        common_chain_block_number: u64,
    ) -> Self {
        info!("Initializing Contracts Client...");
        let gateways_contract = GatewaysContract::new(
            gateways_contract_addr.clone(),
            common_chain_http_provider.clone(),
        );

        let gateway_jobs_contract = GatewayJobsContract::new(
            gateway_jobs_contract_addr.clone(),
            common_chain_http_provider.clone(),
        );

        info!("Gateway Data fetched. Contracts Client Initialized");

        let common_chain_ws_provider = Provider::<Ws>::connect_with_reconnects(
            common_chain_ws_url,
            5,
        )
        .await
        .context("Failed to connect to the chain websocket provider. Please check the chain url.")
        .unwrap();

        ContractsClient {
            enclave_owner,
            signer,
            enclave_signer_key,
            enclave_address,
            common_chain_ws_provider,
            common_chain_http_provider,
            gateway_jobs_contract_addr: *gateway_jobs_contract_addr,
            gateways_contract_addr: *gateways_contract_addr,
            gateways_contract,
            gateway_jobs_contract,
            request_chain_clients,
            gateway_epoch_state,
            request_chain_ids,
            active_jobs: Arc::new(RwLock::new(HashMap::new())),
            epoch,
            time_interval,
            gateway_epoch_state_waitlist,
            common_chain_start_block_number: Arc::new(Mutex::new(common_chain_block_number)),
        }
    }

    pub async fn wait_for_registration(self: Arc<Self>, app_state: Data<AppState>) {
        info!("Waiting for registration on the Common Chain and all Request Chains...");
        // create a channel to communicate with the main thread
        let (tx, mut rx) = channel::<(RegisteredData, Arc<ContractsClient>, Data<AppState>)>(100);

        let common_chain_block_number = *self.common_chain_start_block_number.lock().unwrap();

        let common_chain_registered_filter = Filter::new()
            .address(self.gateways_contract_addr)
            .select(common_chain_block_number..)
            .topic0(vec![keccak256(
                "GatewayRegistered(address,address,uint256[])",
            )])
            .topic1(self.enclave_address)
            .topic2(self.enclave_owner);

        let tx_clone = tx.clone();
        let self_clone = Arc::clone(&self);
        let app_state_clone = app_state.clone();
        task::spawn(async move {
            let mut common_chain_stream = self_clone
                .common_chain_ws_provider
                .subscribe_logs(&common_chain_registered_filter)
                .await
                .context("failed to subscribe to events on the Common Chain")
                .unwrap();

            while let Some(log) = common_chain_stream.next().await {
                if log.removed.unwrap_or(true) {
                    continue;
                }

                *self_clone.common_chain_start_block_number.lock().unwrap() = log
                    .block_number
                    .unwrap_or(common_chain_block_number.into())
                    .as_u64();

                let registered_data = RegisteredData {
                    register_type: RegisterType::CommonChain,
                    chain_id: None,
                };
                tx_clone
                    .send((registered_data, self_clone.clone(), app_state_clone))
                    .await
                    .unwrap();

                info!("Common Chain Registered");
                break;
            }
        });

        let request_chain_clients = self.request_chain_clients.clone();

        // listen to all the request chains for the GatewayRegistered event
        for request_chain_client in request_chain_clients.values().cloned() {
            let request_chain_registered_filter = Filter::new()
                .address(request_chain_client.contract_address)
                .select(request_chain_client.request_chain_start_block_number..)
                .topic0(vec![keccak256("GatewayRegistered(address,address)")])
                .topic1(self.enclave_owner)
                .topic2(self.enclave_address);

            let self_clone = Arc::clone(&self);
            let app_state_clone = app_state.clone();
            let tx_clone = tx.clone();
            let request_chain_client_clone = request_chain_client.clone();
            task::spawn(async move {
                let request_chain_ws_provider = Provider::<Ws>::connect_with_reconnects(
                    request_chain_client_clone.ws_rpc_url.clone(),
                    5,
                )
                .await
                .context(
                    "Failed to connect to the request chain websocket provider. Please check the chain url.",
                )
                .unwrap();

                let mut request_chain_stream = request_chain_ws_provider
                    .subscribe_logs(&request_chain_registered_filter)
                    .await
                    .context("failed to subscribe to events on the Request Chain")
                    .unwrap();

                while let Some(log) = request_chain_stream.next().await {
                    if log.removed.unwrap_or(true) {
                        continue;
                    }

                    let registered_data = RegisteredData {
                        register_type: RegisterType::RequestChain,
                        chain_id: Some(request_chain_client.chain_id),
                    };
                    tx_clone
                        .send((registered_data, self_clone, app_state_clone))
                        .await
                        .unwrap();
                    info!(
                        "Request Chain ID: {:?} Registered",
                        request_chain_client.chain_id
                    );
                    break;
                }
            });
        }

        let mut common_chain_registered = false;
        let mut req_chain_ids_not_registered: HashSet<u64> = self
            .request_chain_clients
            .clone()
            .keys()
            .cloned()
            .collect::<HashSet<u64>>();
        while let Some((registered_data, contracts_client, app_state)) = rx.recv().await {
            match registered_data.register_type {
                RegisterType::CommonChain => {
                    common_chain_registered = true;
                }
                RegisterType::RequestChain => {
                    req_chain_ids_not_registered.remove(&registered_data.chain_id.unwrap());
                }
            }

            if common_chain_registered && req_chain_ids_not_registered.is_empty() {
                // All registration completed on common chain and all request chains
                // Mark registered in the app state
                *app_state.registered.lock().unwrap() = true;
                // Start the ContractsClient service
                tokio::spawn(async move {
                    let _ = contracts_client.run().await;
                });
                break;
            }
        }
    }

    pub async fn run(self: Arc<Self>) -> Result<(), Box<dyn Error>> {
        // setup for the listening events on Request Chain and calling Common Chain functions
        let (req_chain_tx, com_chain_rx) = channel::<(Job, Arc<ContractsClient>)>(100);
        let self_clone = Arc::clone(&self);
        // Start the gateway epoch state service
        {
            let service_start_time = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let contract_client_clone = self.clone();
            let tx_clone = req_chain_tx.clone();
            let common_chain_http_provider_clone = self.common_chain_http_provider.clone();
            tokio::spawn(async move {
                gateway_epoch_state_service(
                    service_start_time,
                    &common_chain_http_provider_clone,
                    contract_client_clone,
                    tx_clone,
                )
                .await;
            });
        }
        tokio::spawn(async move {
            let _ = self_clone.txns_to_common_chain(com_chain_rx).await;
        });
        let self_clone = Arc::clone(&self);
        self_clone.handle_all_req_chain_events(req_chain_tx).await?;

        // setup for the listening events on Common Chain and calling Request Chain functions
        let (com_chain_tx, req_chain_rx) = channel::<(ResponseJob, Arc<ContractsClient>)>(100);
        let self_clone = Arc::clone(&self);
        tokio::spawn(async move {
            let _ = self_clone.txns_to_request_chain(req_chain_rx).await;
        });
        self.handle_all_com_chain_events(com_chain_tx).await?;
        Ok(())
    }

    async fn handle_all_req_chain_events(
        self: Arc<Self>,
        tx: Sender<(Job, Arc<ContractsClient>)>,
    ) -> Result<()> {
        info!("Initializing Request Chain Clients for all request chains...");
        let chains_ids = self.request_chain_ids.clone();

        for chain_id in chains_ids {
            let self_clone = Arc::clone(&self);
            let tx_clone = tx.clone();

            let req_chain_ws_client = Provider::<Ws>::connect_with_reconnects(
                    self_clone.request_chain_clients[&chain_id].ws_rpc_url.clone(),
                    5,
                ).await
                .context(
                    "Failed to connect to the request chain websocket provider. Please check the chain url.",
                )?;

            // Spawn a new task for each Request Chain Contract
            task::spawn(async move {
                let mut stream = self_clone
                    .req_chain_jobs(
                        &req_chain_ws_client,
                        &self_clone.request_chain_clients[&chain_id],
                    )
                    .await
                    .unwrap();

                while let Some(log) = stream.next().await {
                    let topics = log.topics.clone();

                    if let Some(is_removed) = log.removed {
                        if is_removed {
                            continue;
                        }
                    } else {
                        continue;
                    }

                    if topics[0]
                        == keccak256(
                            "JobRelayed(uint256,bytes32,bytes,uint256,uint256,uint256,uint256,address,address,uint256,uint256)",
                        )
                        .into()
                    {
                        info!(
                            "Request Chain ID: {:?}, JobPlace jobID: {:?}",
                            chain_id, log.topics[1]
                        );

                        let self_clone = Arc::clone(&self_clone);
                        let tx = tx_clone.clone();
                        task::spawn(async move {
                            // TODO: what to do in case of error? Let it panic or return None?
                            let job = self_clone.clone()
                                .get_job_from_job_relay_event(
                                    log,
                                    1 as u8,
                                    chain_id
                                )
                                .await
                                .context("Failed to get Job from Log")
                                .unwrap();
                            self_clone.job_placed_handler(
                                    job,
                                    tx.clone(),
                                )
                                .await;
                        });
                    } else if topics[0] == keccak256("JobCancelled(uint256)").into() {
                        info!(
                            "Request Chain ID: {:?}, JobCancelled jobID: {:?}",
                            chain_id, log.topics[1]
                        );

                        let self_clone = Arc::clone(&self_clone);
                        task::spawn(async move {
                            self_clone.cancel_job_with_job_id(
                                log.topics[1].into_uint(),
                            ).await;
                        });
                    } else {
                        error!(
                            "Request Chain ID: {:?}, Unknown event: {:?}",
                            chain_id, log
                        );
                    }
                }
            });
        }

        Ok(())
    }

    async fn get_job_from_job_relay_event(
        self: Arc<Self>,
        log: Log,
        sequence_number: u8,
        request_chain_id: u64,
    ) -> Result<Job> {
        let types = vec![
            ParamType::FixedBytes(32),
            ParamType::Bytes,
            ParamType::Uint(256),
            ParamType::Uint(256),
            ParamType::Uint(256),
            ParamType::Uint(256),
            ParamType::Address,
            ParamType::Address,
            ParamType::Uint(256),
            ParamType::Uint(256),
        ];

        let decoded = decode(&types, &log.data.0);
        let decoded = match decoded {
            Ok(decoded) => decoded,
            Err(err) => {
                error!("Error while decoding event: {}", err);
                return Err(anyhow::Error::msg("Error while decoding event"));
            }
        };

        let req_chain_client = self.request_chain_clients[&request_chain_id].clone();
        let job_id = log.topics[1].into_uint();

        Ok(Job {
            job_id,
            request_chain_id: req_chain_client.chain_id.clone(),
            tx_hash: decoded[0].clone().into_fixed_bytes().unwrap(),
            code_input: decoded[1].clone().into_bytes().unwrap().into(),
            user_timeout: decoded[2].clone().into_uint().unwrap(),
            starttime: decoded[8].clone().into_uint().unwrap(),
            job_owner: log.address,
            job_type: GatewayJobType::JobRelay,
            sequence_number,
            gateway_address: None,
        })
    }

    pub async fn job_placed_handler(
        self: Arc<Self>,
        mut job: Job,
        tx: Sender<(Job, Arc<ContractsClient>)>,
    ) {
        let req_chain_client = self.request_chain_clients[&job.request_chain_id].clone();

        let gateway_address = self
            .select_gateway_for_job_id(
                job.clone(),
                job.starttime.as_u64(), // TODO: Update seed
                job.sequence_number,
                req_chain_client,
            )
            .await;

        // if error message is returned, then the job is older than the maintained block states
        match gateway_address {
            Ok(gateway_address) => {
                job.gateway_address = Some(gateway_address);

                if gateway_address == Address::zero() {
                    return;
                }

                if gateway_address == self.enclave_address {
                    // scope for the write lock
                    {
                        self.active_jobs
                            .write()
                            .unwrap()
                            .insert(job.job_id, job.clone());
                    }
                    tx.send((job, self)).await.unwrap();
                } else {
                    task::spawn(async move {
                        self.job_relayed_slash_timer(job, None, tx).await;
                    });
                }
            }
            Err(err) => {
                error!("Error while selecting gateway: {}", err);
            }
        }
    }

    #[async_recursion]
    async fn job_relayed_slash_timer(
        self: Arc<Self>,
        mut job: Job,
        mut job_timeout: Option<u64>,
        tx: Sender<(Job, Arc<ContractsClient>)>,
    ) {
        if job_timeout.is_none() {
            job_timeout = Some(REQUEST_RELAY_TIMEOUT);
        }
        time::sleep(Duration::from_secs(job_timeout.unwrap())).await;

        // TODO: Issue with event logs -
        // get_logs might not provide the latest logs for the latest block
        // SOLUTION 1 - Wait for the next block.
        //          Problem: Extra time spent here waiting.
        let logs = self
            .gateways_job_relayed_logs(job.clone())
            .await
            .context("Failed to get logs")
            .unwrap();

        for log in logs {
            let topics = log.topics.clone();
            if topics[0] == keccak256("JobRelayed(uint256,uint256,address,address)").into() {
                let decoded = decode(
                    &vec![
                        ParamType::Uint(256),
                        ParamType::Uint(256),
                        ParamType::Address,
                        ParamType::Address,
                    ],
                    &log.data.0,
                )
                .unwrap();

                let job_id = log.topics[1].into_uint();
                let job_owner = decoded[1].clone().into_address().unwrap();
                let gateway_operator = decoded[2].clone().into_address().unwrap();

                if job_id == job.job_id
                    && job_owner == job.job_owner
                    && gateway_operator != Address::zero()
                    && gateway_operator != job.gateway_address.unwrap()
                {
                    info!(
                        "Job ID: {:?}, JobRelayed event triggered for job ID: {:?}",
                        job.job_id, job_id
                    );
                    return;
                }
            }
        }

        info!("Job ID: {:?}, JobRelayed event not triggered", job.job_id);

        // slash the previous gateway
        {
            let self_clone = self.clone();
            let mut job_clone = job.clone();
            job_clone.job_type = GatewayJobType::SlashGatewayJob;
            let tx_clone = tx.clone();
            tx_clone.send((job_clone, self_clone)).await.unwrap();
        }

        job.sequence_number += 1;
        if job.sequence_number > MAX_GATEWAY_RETRIES {
            info!("Job ID: {:?}, Max retries reached", job.job_id);
            return;
        }
        job.gateway_address = None;

        task::spawn(async move {
            self.job_placed_handler(job, tx).await;
        });
    }

    async fn select_gateway_for_job_id(
        &self,
        job: Job,
        seed: u64,
        skips: u8,
        req_chain_client: Arc<RequestChainClient>,
    ) -> Result<Address> {
        let job_cycle =
            (job.starttime.as_u64() - self.epoch - OFFEST_FOR_GATEWAY_EPOCH_STATE_CYCLE)
                / self.time_interval;

        let all_gateways_data: Vec<GatewayData>;

        {
            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let current_cycle =
                (ts - self.epoch - OFFEST_FOR_GATEWAY_EPOCH_STATE_CYCLE) / self.time_interval;
            if current_cycle >= GATEWAY_BLOCK_STATES_TO_MAINTAIN + job_cycle {
                return Err(anyhow::Error::msg(
                    "Job is older than the maintained block states",
                ));
            }
            let gateway_epoch_state_guard = self.gateway_epoch_state.read().unwrap();
            if let Some(gateway_epoch_state) = gateway_epoch_state_guard.get(&job_cycle) {
                all_gateways_data = gateway_epoch_state
                    .values()
                    .cloned()
                    .collect::<Vec<GatewayData>>();
            } else {
                let mut waitlist_handle = self.gateway_epoch_state_waitlist.write().unwrap();
                waitlist_handle
                    .entry(job_cycle)
                    .and_modify(|jobs| jobs.push(job.clone()))
                    .or_insert(vec![job]);
                return Ok(Address::zero());
            }
        }

        // create a weighted probability distribution for gateways based on stake amount
        // For example, if there are 3 gateways with stake amounts 100, 200, 300
        // then the distribution array will be [100, 300, 600]
        let mut stake_distribution: Vec<u64> = vec![];
        let mut total_stake: u64 = 0;
        let mut gateway_data_of_req_chain: Vec<GatewayData> = vec![];
        if all_gateways_data.is_empty() {
            return Err(anyhow::Error::msg("No Gateways Registered"));
        }
        for gateway_data in all_gateways_data.iter() {
            if gateway_data
                .req_chain_ids
                .contains(&req_chain_client.chain_id)
                && gateway_data.stake_amount.as_u64() > MIN_GATEWAY_STAKE
                && gateway_data.draining == false
            {
                gateway_data_of_req_chain.push(gateway_data.clone());
                total_stake += gateway_data.stake_amount.as_u64();
                stake_distribution.push(total_stake);
            }
        }

        // random number between 1 to total_stake from the eed for the weighted random selection.
        // use this seed in std_rng to generate a random number between 1 to total_stake
        // skipping skips numbers from the random number generated
        let mut rng = StdRng::seed_from_u64(seed);
        for _ in 0..skips - 1 {
            let _ = rng.gen_range(1..=total_stake);
        }
        let random_number = rng.gen_range(1..=total_stake);

        // select the gateway based on the random number
        let res = stake_distribution.binary_search_by(|&probe| probe.cmp(&random_number));

        let index = match res {
            Ok(index) => index,
            Err(index) => index,
        };
        let selected_gateway = &gateway_data_of_req_chain[index];

        info!(
            "Job ID: {:?}, Gateway Address: {:?}",
            job.job_id, selected_gateway.address
        );

        Ok(selected_gateway.address)
    }

    async fn cancel_job_with_job_id(self: Arc<Self>, job_id: U256) {
        info!("Remove the Job ID: {:} from the active jobs list", job_id);

        // scope for the write lock
        {
            self.active_jobs.write().unwrap().remove(&job_id);
        }
    }

    async fn txns_to_common_chain(
        self: Arc<Self>,
        mut rx: Receiver<(Job, Arc<ContractsClient>)>,
    ) -> Result<()> {
        while let Some((job, com_chain_client)) = rx.recv().await {
            match job.job_type {
                GatewayJobType::JobRelay => {
                    com_chain_client.relay_job_txn(job).await;
                }
                GatewayJobType::SlashGatewayJob => {
                    com_chain_client.reassign_gateway_relay_txn(job).await;
                }
                _ => {
                    error!("Unknown job type: {:?}", job.job_type);
                }
            }
        }
        Ok(())
    }

    async fn relay_job_txn(self: Arc<Self>, job: Job) {
        info!("Creating a transaction for relayJob");
        let (signature, sign_timestamp) = sign_relay_job_request(
            &self.enclave_signer_key,
            job.job_id,
            &job.tx_hash,
            &job.code_input,
            job.user_timeout,
            job.starttime,
            job.sequence_number,
            &job.job_owner,
        )
        .await
        .unwrap();
        let Ok(signature) = types::Bytes::from_hex(signature) else {
            error!("Failed to decode signature hex string");
            return;
        };
        let tx_hash: [u8; 32] = job.tx_hash[..].try_into().unwrap();

        let txn = self.gateway_jobs_contract.relay_job(
            signature,
            job.job_id,
            tx_hash,
            job.code_input,
            job.user_timeout,
            job.starttime,
            job.sequence_number,
            job.job_owner,
            sign_timestamp.into(),
        );

        let pending_txn = txn.send().await;
        let Ok(pending_txn) = pending_txn else {
            error!(
                "Failed to submit transaction {} for job relay to CommonChain",
                pending_txn.unwrap_err()
            );
            return;
        };

        let txn_hash = pending_txn.tx_hash();
        let Ok(Some(_)) = pending_txn.confirmations(1).await else {
            error!(
                "Failed to confirm transaction {} for job relay to CommonChain",
                txn_hash
            );
            return;
        };

        info!(
            "Transaction {} confirmed for job relay to CommonChain",
            txn_hash
        );
    }

    async fn reassign_gateway_relay_txn(self: Arc<Self>, job: Job) {
        info!("Creating a transaction for reassignGatewayRelay");
        let (signature, sign_timestamp) = sign_reassign_gateway_relay_request(
            &self.enclave_signer_key,
            job.job_id,
            job.gateway_address.as_ref().unwrap(),
            &job.job_owner,
            job.sequence_number,
            job.starttime,
        )
        .await
        .unwrap();
        let Ok(signature) = types::Bytes::from_hex(signature) else {
            error!("Failed to decode signature hex string");
            return;
        };

        let txn = self.gateway_jobs_contract.reassign_gateway_relay(
            job.gateway_address.unwrap(),
            job.job_id,
            signature,
            job.sequence_number,
            job.starttime,
            job.job_owner,
            sign_timestamp.into(),
        );

        let pending_txn = txn.send().await;
        let Ok(pending_txn) = pending_txn else {
            error!(
                "Failed to submit transaction {} for reassign gateway relay to CommonChain",
                pending_txn.unwrap_err()
            );
            return;
        };

        let txn_hash = pending_txn.tx_hash();
        let Ok(Some(_)) = pending_txn.confirmations(1).await else {
            error!(
                "Failed to confirm transaction {} for reassign gateway relay to CommonChain",
                txn_hash
            );
            return;
        };

        info!(
            "Transaction {} confirmed for reassign gateway relay to CommonChain",
            txn_hash
        );
    }

    async fn handle_all_com_chain_events(
        self: Arc<Self>,
        tx: Sender<(ResponseJob, Arc<ContractsClient>)>,
    ) -> Result<()> {
        let mut stream = self.common_chain_jobs().await.unwrap();

        while let Some(log) = stream.next().await {
            let topics = log.topics.clone();

            if topics[0] == keccak256("JobResponded(uint256,bytes,uint256,uint8)").into() {
                info!(
                    "JobResponded event triggered for job ID: {:?}",
                    log.topics[1]
                );
                let self_clone = Arc::clone(&self);
                let tx = tx.clone();
                task::spawn(async move {
                    let response_job = self_clone
                        .clone()
                        .get_job_from_job_responded_event(log)
                        .await
                        .context("Failed to decode event")
                        .unwrap();
                    self_clone.job_responded_handler(response_job, tx).await;
                });
            } else if topics[0] == keccak256("JobResourceUnavailable(uint256,address)").into() {
                info!("JobResourceUnavailable event triggered");
                let self_clone = Arc::clone(&self);
                task::spawn(async move {
                    self_clone.job_resource_unavailable_handler(log).await;
                });
            } else if topics[0]
                == keccak256("GatewayReassigned(uint256,address,address,uint8)").into()
            {
                info!(
                    "Request Chain ID: {:?}, GatewayReassigned jobID: {:?}",
                    log.topics[2], log.topics[1]
                );
                let self_clone = Arc::clone(&self);
                task::spawn(async move {
                    self_clone.gateway_reassigned_handler(log).await;
                });
            } else {
                error!("Unknown event: {:?}", log);
            }
        }

        Ok(())
    }

    async fn get_job_from_job_responded_event(self: Arc<Self>, log: Log) -> Result<ResponseJob> {
        let job_id = log.topics[1].into_uint();

        // Check if job belongs to the enclave
        let active_jobs = self.active_jobs.read().unwrap();
        let job = active_jobs.get(&job_id);
        if job.is_none() {
            return Err(anyhow::Error::msg("Job does not belong to the enclave"));
        }

        let job = job.unwrap();

        let types = vec![ParamType::Bytes, ParamType::Uint(256), ParamType::Uint(8)];

        let decoded = decode(&types, &log.data.0).unwrap();
        let request_chain_id = job.request_chain_id;

        Ok(ResponseJob {
            job_id,
            request_chain_id,
            output: decoded[0].clone().into_bytes().unwrap().into(),
            total_time: decoded[1].clone().into_uint().unwrap(),
            error_code: decoded[2].clone().into_uint().unwrap().low_u64() as u8,
            job_type: GatewayJobType::JobResponded,
            gateway_address: None,
            // sequence_number: 1,
        })
    }

    async fn job_responded_handler(
        self: Arc<Self>,
        mut response_job: ResponseJob,
        tx: Sender<(ResponseJob, Arc<ContractsClient>)>,
    ) {
        // let req_chain_client =
        //     self.request_chain_clients[&response_job.request_chain_id.to_string()].clone();

        let job: Option<Job>;
        // scope for the read lock
        {
            job = self
                .active_jobs
                .read()
                .unwrap()
                .get(&response_job.job_id)
                .cloned();
        }
        if job.is_some() {
            let job = job.unwrap();
            response_job.gateway_address = job.gateway_address;
            self.clone().remove_job(job).await;

            // Currently, slashing is not implemented for the JobResponded event
            // } else if response_job.sequence_number > 1 {
            //     let gateway_address: Address;
            //     // let seed be absolute difference between (job_id and request_chain_id) + total_time
            //     let seed = {
            //         let job_id_req_chain_id = match response_job
            //             .job_id
            //             .as_u64()
            //             .checked_sub(response_job.request_chain_id)
            //         {
            //             Some(val) => val,
            //             None => response_job.request_chain_id - response_job.job_id.as_u64(),
            //         };
            //         job_id_req_chain_id + response_job.total_time.as_u64()
            //     };
            //     gateway_address = self
            //         .select_gateway_for_job_id(
            //             response_job.job_id.clone(),
            //             seed,
            //             response_job.sequence_number,
            //             req_chain_client,
            //         )
            //         .await
            //         .context("Failed to select a gateway for the job")
            //         .unwrap();
            //     response_job.gateway_address = Some(gateway_address);
            // }
            // if response_job.gateway_address.unwrap() == self.enclave_address {
            tx.send((response_job, self.clone())).await.unwrap();
            // } else {
            //     self.job_responded_slash_timer(response_job.clone(), tx.clone())
            //         .await
            //         .unwrap();
        }
    }

    async fn remove_job(self: Arc<Self>, job: Job) {
        let mut active_jobs = self.active_jobs.write().unwrap();
        // The retry number check is to make sure we are removing the correct job from the active jobs list
        // In a case where this txn took longer than the REQUEST_RELAY_TIMEOUT, the job might have been retried
        // and the active_jobs list might have the same job_id with a different retry number.
        if active_jobs.contains_key(&job.job_id)
            && active_jobs[&job.job_id].sequence_number == job.sequence_number
        {
            active_jobs.remove(&job.job_id);
        }
    }

    // TODO: Discuss with the team about the implementation of slashing for the JobResponded event
    // Currently, slashing is not implemented for the JobResponded event
    // #[async_recursion]
    // async fn job_responded_slash_timer(
    //     self: Arc<Self>,
    //     mut response_job: ResponseJob,
    //     tx: Sender<(ResponseJob, Arc<ContractsClient>)>,
    // ) -> Result<()> {
    //     time::sleep(Duration::from_secs(RESPONSE_RELAY_TIMEOUT)).await;
    //     // get request chain client
    //     let req_chain_client =
    //         self.request_chain_clients[&response_job.request_chain_id.to_string()].clone();
    //     let onchain_response_job = req_chain_client
    //         .contract
    //         .jobs(response_job.job_id)
    //         .await
    //         .unwrap();
    //     let output_received: bool = onchain_response_job.8;
    //     let onchain_response_job: ResponseJob = ResponseJob {
    //         job_id: response_job.job_id,
    //         request_chain_id: response_job.request_chain_id,
    //         output: Bytes::default().into(),
    //         total_time: U256::zero(),
    //         error_code: 0,
    //         output_count: 0,
    //         job_type: GatewayJobType::JobResponded,
    //         gateway_address: Some(onchain_response_job.7),
    //         // depending on how the gateway is reassigned, the retry number might be different
    //         // can be added to event and a check below in the if condition
    //         // if retry number is added to the event,
    //         // remove_response_job needs to be updated accordingly
    //         sequence_number: 1,
    //     };
    //     // if output is received and the gateway is the same as the one assigned by the common chain
    //     // then the job is relayed
    //     // sequence_number check is missing
    //     if output_received && onchain_response_job.gateway_address.unwrap() != H160::zero() {
    //         info!(
    //             "Job ID: {:?}, JobResponded event triggered",
    //             response_job.job_id
    //         );
    //         return Ok(());
    //     }
    //     // TODO: how to slash the gateway now?
    //     // The same function used with the JobRelayed event won't work here.
    //     // For now, use the same function.
    //     {
    //         let self_clone = self.clone();
    //         let mut response_job_clone = response_job.clone();
    //         response_job_clone.job_type = GatewayJobType::SlashGatewayResponse;
    //         let tx_clone = tx.clone();
    //         tx_clone
    //             .send((response_job_clone, self_clone))
    //             .await
    //             .unwrap();
    //     }
    //     response_job.sequence_number += 1;
    //     if response_job.sequence_number > MAX_GATEWAY_RETRIES {
    //         info!("Job ID: {:?}, Max retries reached", response_job.job_id);
    //         return Ok(());
    //     }
    //     // If gateway is already set, job_responded_handler will reassign the gateway
    //     response_job.gateway_address = onchain_response_job.gateway_address;
    //     self.job_responded_handler(response_job, tx).await;
    //     Ok(())
    // }

    async fn job_resource_unavailable_handler(self: Arc<Self>, log: Log) {
        let job_id = log.topics[1].into_uint();

        let active_jobs_guard = self.active_jobs.read().unwrap();
        let job = active_jobs_guard.get(&job_id);
        if job.is_none() {
            return;
        }
        let job = job.unwrap().clone();
        drop(active_jobs_guard);

        if job.gateway_address.unwrap() != self.enclave_address {
            return;
        }

        // scope for the write lock
        {
            let job = self.active_jobs.write().unwrap().remove(&job_id);
            if job.is_some() {
                info!(
                    "Job ID: {:?} - removed from active jobs",
                    job.unwrap().job_id
                );
            } else {
                info!("Job ID: {:?} - not found in active jobs", job_id);
            }
        }
    }

    async fn gateway_reassigned_handler(self: Arc<Self>, log: Log) {
        let job_id = log.topics[1].into_uint();

        // Check if job belongs to the enclave
        let active_jobs_guard = self.active_jobs.read().unwrap();
        let job = active_jobs_guard.get(&job_id);
        if job.is_none() {
            return;
        }

        let job = job.unwrap().clone();
        drop(active_jobs_guard);

        let types = vec![ParamType::Address, ParamType::Address, ParamType::Uint(8)];
        let decoded = decode(&types, &log.data.0).unwrap();

        let old_gateway = decoded[0].clone().into_address().unwrap();
        let sequence_number = decoded[2].clone().into_uint().unwrap().low_u64() as u8;

        if old_gateway != self.enclave_address {
            return;
        }

        if job.sequence_number != sequence_number {
            return;
        }

        // scope for the write lock
        {
            self.active_jobs.write().unwrap().remove(&job_id);
        }
    }

    async fn txns_to_request_chain(
        self: Arc<Self>,
        mut rx: Receiver<(ResponseJob, Arc<ContractsClient>)>,
    ) -> Result<()> {
        while let Some((response_job, com_chain_client)) = rx.recv().await {
            match response_job.job_type {
                GatewayJobType::JobResponded => {
                    let com_chain_client_clone = com_chain_client.clone();
                    let response_job_clone = response_job.clone();
                    com_chain_client_clone
                        .job_response_txn(response_job_clone)
                        .await;
                    com_chain_client
                        .remove_response_job(response_job.job_id)
                        .await;
                }
                // Currently, slashing is not implemented for the JobResponded event
                // GatewayJobType::SlashGatewayResponse => {
                //     com_chain_client
                //         .reassign_gateway_response_txn(response_job)
                //         .await;
                // }

                // Ignore other types of jobs
                _ => {
                    error!("Unknown job type: {:?}", response_job.job_type);
                }
            }
        }
        Ok(())
    }

    async fn job_response_txn(self: Arc<Self>, response_job: ResponseJob) {
        info!("Creating a transaction for jobResponse");

        let req_chain_client = self.request_chain_clients[&response_job.request_chain_id].clone();

        let (signature, sign_timestamp) = sign_job_response_request(
            &self.enclave_signer_key,
            response_job.job_id,
            response_job.output.clone(),
            response_job.total_time,
            response_job.error_code,
        )
        .await
        .unwrap();
        let Ok(signature) = types::Bytes::from_hex(signature) else {
            error!("Failed to decode signature hex string");
            return;
        };

        let txn = req_chain_client.contract.job_response(
            signature,
            response_job.job_id,
            response_job.output,
            response_job.total_time,
            response_job.error_code,
            sign_timestamp.into(),
        );

        let pending_txn = txn.send().await;
        let Ok(pending_txn) = pending_txn else {
            error!(
                "Failed to submit transaction {} for job response to RequestChain",
                pending_txn.unwrap_err()
            );
            return;
        };

        let txn_hash = pending_txn.tx_hash();
        let Ok(Some(_)) = pending_txn.confirmations(1).await else {
            error!(
                "Failed to confirm transaction {} for job response to RequestChain",
                txn_hash
            );
            return;
        };

        info!(
            "Transaction {} confirmed for job response to RequestChain",
            txn_hash
        );
    }

    async fn remove_response_job(self: Arc<Self>, job_id: U256) {
        let mut active_jobs = self.active_jobs.write().unwrap();
        active_jobs.remove(&job_id);
    }
}

impl LogsProvider for ContractsClient {
    async fn common_chain_jobs<'a>(&'a self) -> Result<SubscriptionStream<'a, Ws, Log>> {
        info!("Subscribing to events for Common Chain");

        let common_chain_start_block_number =
            self.common_chain_start_block_number.lock().unwrap().clone();
        let event_filter: Filter = Filter::new()
            .address(self.gateway_jobs_contract_addr)
            .select(common_chain_start_block_number..)
            .topic0(vec![
                keccak256("JobResponded(uint256,bytes,uint256,uint8)"),
                keccak256("JobResourceUnavailable(uint256,address)"),
                keccak256("GatewayReassigned(uint256,address,address,uint8)"),
            ]);

        let stream = self
            .common_chain_ws_provider
            .subscribe_logs(&event_filter)
            .await
            .context("failed to subscribe to events on the Common Chain")
            .unwrap();

        Ok(stream)
    }

    async fn req_chain_jobs<'a>(
        &'a self,
        req_chain_ws_client: &'a Provider<Ws>,
        req_chain_client: &'a RequestChainClient,
    ) -> Result<SubscriptionStream<'a, Ws, Log>> {
        info!(
            "Subscribing to events for Req Chain chain_id: {}",
            req_chain_client.chain_id
        );

        let event_filter = Filter::new()
            .address(req_chain_client.contract_address)
            .select(req_chain_client.request_chain_start_block_number..)
            .topic0(vec![
                keccak256(
                    "JobRelayed(uint256,bytes32,bytes,uint256,uint256,uint256,uint256,address,address,uint256,uint256)",
                ),
                keccak256("JobCancelled(uint256)"),
            ]);

        // register subscription
        let stream = req_chain_ws_client
            .subscribe_logs(&event_filter)
            .await
            .context(format!(
                "failed to subscribe to events on Request Chain: {}",
                req_chain_client.chain_id
            ))
            .unwrap();

        Ok(stream)
    }

    #[cfg(not(test))]
    async fn gateways_job_relayed_logs<'a>(&'a self, job: Job) -> Result<Vec<Log>> {
        let common_chain_start_block_number =
            self.common_chain_start_block_number.lock().unwrap().clone();

        let job_relayed_event_filter = Filter::new()
            .address(self.gateway_jobs_contract_addr)
            .select(common_chain_start_block_number..)
            .topic0(vec![keccak256(
                "JobRelayed(uint256,uint256,address,address)",
            )])
            .topic1(job.job_id);

        let logs = self
            .common_chain_ws_provider
            .get_logs(&job_relayed_event_filter)
            .await
            .unwrap();

        Ok(logs)
    }

    #[cfg(test)]
    async fn gateways_job_relayed_logs<'a>(&'a self, job: Job) -> Result<Vec<Log>> {
        use ethers::abi::{encode, Token};
        use ethers::prelude::*;

        if job.job_id == U256::from(1) {
            Ok(vec![Log {
                address: self.gateway_jobs_contract_addr,
                topics: vec![
                    keccak256("JobRelayed(uint256,uint256,address,address)").into(),
                    H256::from_uint(&job.job_id),
                ],
                data: encode(&[
                    Token::Uint(job.job_id),
                    Token::Uint(U256::from(100)),
                    Token::Address(job.job_owner),
                    Token::Address(job.gateway_address.unwrap()),
                ])
                .into(),
                ..Default::default()
            }])
        } else {
            Ok(vec![Log {
                address: Address::default(),
                topics: vec![H256::default(), H256::default(), H256::default()],
                data: Bytes::default(),
                ..Default::default()
            }])
        }
    }
}

#[cfg(test)]
mod serverless_executor_test {
    use std::collections::BTreeSet;
    use std::str::FromStr;

    use abi::{encode, Token};
    use actix_web::{
        body::MessageBody,
        dev::{ServiceFactory, ServiceRequest, ServiceResponse},
        http, test, App, Error,
    };
    use ethers::types::{Address, Bytes as EthBytes, H160};
    use ethers::utils::public_key_to_address;
    use rand::rngs::OsRng;
    use serde_json::json;
    use tokio::time::sleep;

    use crate::{
        api_impl::{
            export_signed_registration_message, index, inject_immutable_config,
            inject_mutable_config,
        },
        contract_abi::RelayContract,
    };

    use super::*;

    // Testnet or Local blockchain (Hardhat) configurations
    const CHAIN_ID: u64 = 421614;
    const HTTP_RPC_URL: &str = "https://sepolia-rollup.arbitrum.io/rpc";
    const WS_URL: &str = "wss://arbitrum-sepolia.infura.io/ws/v3/cd72f20b9fd544f8a5b8da706441e01c";
    const GATEWAY_CONTRACT_ADDR: &str = "0x819d9b4087D88359B6d7fFcd16F17A13Ca79fd0E";
    const JOB_CONTRACT_ADDR: &str = "0xAc6Ae536203a3ec290ED4aA1d3137e6459f4A963";
    const RELAY_CONTRACT_ADDR: &str = "0xaF7E4CB6B3729C65c4a9a63d89Ae04e97C9093C4";
    const WALLET_PRIVATE_KEY: &str =
        "0x083f09e4d950da6eee7eac93ba7fa046d12eb3c8ca4e4ba92487ae3526e87bda";
    const REGISTER_ATTESTATION: &str = "0xcfa7554f87ba13620037695d62a381a2d876b74c2e1b435584fe5c02c53393ac1c5cd5a8b6f92e866f9a65af751e0462cfa7554f87ba13620037695d62a381a2d8";
    const REGISTER_PCR_0: &str = "0xcfa7554f87ba13620037695d62a381a2d876b74c2e1b435584fe5c02c53393ac1c5cd5a8b6f92e866f9a65af751e0462";
    const REGISTER_PCR_1: &str = "0xbcdf05fefccaa8e55bf2c8d6dee9e79bbff31e34bf28a99aa19e6b29c37ee80b214a414b7607236edf26fcb78654e63f";
    const REGISTER_PCR_2: &str = "0x20caae8a6a69d9b1aecdf01a0b9c5f3eafd1f06cb51892bf47cef476935bfe77b5b75714b68a69146d650683a217c5b3";
    const REGISTER_TIMESTAMP: usize = 1722134849000;
    const REGISTER_STAKE_AMOUNT: usize = 100;
    const EPOCH: u64 = 1713433800;
    const TIME_INTERVAL: u64 = 300;

    // Generate test app state
    async fn generate_app_state() -> Data<AppState> {
        // Initialize random 'secp256k1' signing key for the enclave
        let signer_key = SigningKey::random(&mut OsRng);

        Data::new(AppState {
            enclave_signer_key: signer_key.clone(),
            enclave_address: public_key_to_address(&signer_key.verifying_key()),
            wallet: None.into(),
            common_chain_id: CHAIN_ID,
            common_chain_http_url: HTTP_RPC_URL.to_owned(),
            common_chain_ws_url: WS_URL.to_owned(),
            gateways_contract_addr: GATEWAY_CONTRACT_ADDR.parse::<Address>().unwrap(),
            gateway_jobs_contract_addr: JOB_CONTRACT_ADDR.parse::<Address>().unwrap(),
            request_chain_ids: HashSet::new().into(),
            request_chain_data: vec![].into(),
            registered: false.into(),
            registration_events_listener_active: false.into(),
            epoch: EPOCH,
            time_interval: TIME_INTERVAL,
            enclave_owner: H160::zero().into(),
            immutable_params_injected: false.into(),
            mutable_params_injected: false.into(),
        })
    }

    // Return the actix server with the provided app state
    fn new_app(
        app_state: Data<AppState>,
    ) -> App<
        impl ServiceFactory<
            ServiceRequest,
            Response = ServiceResponse<impl MessageBody + std::fmt::Debug>,
            Config = (),
            InitError = (),
            Error = Error,
        >,
    > {
        App::new()
            .app_data(app_state)
            .service(index)
            .service(inject_immutable_config)
            .service(inject_mutable_config)
            .service(export_signed_registration_message)
    }

    // Test the various response cases for the 'inject_key' endpoint
    #[actix_web::test]
    async fn inject_immutable_config_test() {
        let app = test::init_service(new_app(generate_app_state().await)).await;

        // Inject invalid hex private key string
        let req = test::TestRequest::post()
            .uri("/inject-key")
            .set_json(&json!({
                "operator_secret": "0x32255"
            }))
            .to_request();

        let resp = test::call_service(&app, req).await;

        assert_eq!(resp.status(), http::StatusCode::BAD_REQUEST);
        assert_eq!(
            resp.into_body().try_into_bytes().unwrap(),
            "Failed to hex decode the key into 32 bytes: Odd number of digits".as_bytes()
        );

        // Inject invalid length private key
        let req = test::TestRequest::post()
            .uri("/inject-key")
            .set_json(&json!({
                "operator_secret": "0x322c322c322c332c352c35"
            }))
            .to_request();

        let resp = test::call_service(&app, req).await;

        assert_eq!(resp.status(), http::StatusCode::BAD_REQUEST);
        assert_eq!(
            resp.into_body().try_into_bytes().unwrap(),
            "Failed to hex decode the key into 32 bytes: Invalid string length"
        );

        // Inject invalid private(signing) key
        let req = test::TestRequest::post()
            .uri("/inject-key")
            .set_json(&json!({
                "operator_secret": "0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
            }))
            .to_request();

        let resp = test::call_service(&app, req).await;

        assert_eq!(resp.status(), http::StatusCode::BAD_REQUEST);
        assert_eq!(
            resp.into_body().try_into_bytes().unwrap(),
            "Invalid secret key provided: signature error"
        );

        // Inject a valid private key
        let req = test::TestRequest::post()
            .uri("/inject-key")
            .set_json(&json!({
                "operator_secret": WALLET_PRIVATE_KEY
            }))
            .to_request();

        let resp = test::call_service(&app, req).await;

        assert_eq!(resp.status(), http::StatusCode::OK);
        assert_eq!(
            resp.into_body().try_into_bytes().unwrap(),
            "Secret key injected successfully"
        );

        // Inject the valid private key again
        let req = test::TestRequest::post()
            .uri("/inject-key")
            .set_json(&json!({
                "operator_secret": WALLET_PRIVATE_KEY
            }))
            .to_request();

        let resp = test::call_service(&app, req).await;

        assert_eq!(resp.status(), http::StatusCode::BAD_REQUEST);
        assert_eq!(
            resp.into_body().try_into_bytes().unwrap(),
            "Secret key has already been injected"
        );
    }

    // Test the various response cases for the 'register_enclave' & 'deregister_enclave' endpoint
    #[actix_web::test]
    async fn register_deregister_enclave_test() {
        let app = test::init_service(new_app(generate_app_state().await)).await;

        // Register the executor without injecting the operator's private key
        let req = test::TestRequest::post()
            .uri("/register")
            .set_json(&json!({
                "attestation": REGISTER_ATTESTATION,
                "pcr_0": REGISTER_PCR_0,
                "pcr_1": REGISTER_PCR_1,
                "pcr_2": REGISTER_PCR_2,
                "timestamp": REGISTER_TIMESTAMP,
                "stake_amount": REGISTER_STAKE_AMOUNT,
                "request_chain_data": [CHAIN_ID]
            }))
            .to_request();

        let resp = test::call_service(&app, req).await;

        assert_eq!(resp.status(), http::StatusCode::BAD_REQUEST);
        assert_eq!(
            resp.into_body().try_into_bytes().unwrap(),
            "Operator secret key not injected yet!"
        );

        // Deregister the enclave without even injecting the private key
        let req = test::TestRequest::delete().uri("/deregister").to_request();

        let resp = test::call_service(&app, req).await;

        assert_eq!(resp.status(), http::StatusCode::BAD_REQUEST);
        assert_eq!(
            resp.into_body().try_into_bytes().unwrap(),
            "Enclave is not registered yet."
        );

        // Inject a valid private key into the enclave
        let req = test::TestRequest::post()
            .uri("/inject-key")
            .set_json(&json!({
                "operator_secret": WALLET_PRIVATE_KEY
            }))
            .to_request();

        let resp = test::call_service(&app, req).await;

        assert_eq!(resp.status(), http::StatusCode::OK);
        assert_eq!(
            resp.into_body().try_into_bytes().unwrap(),
            "Secret key injected successfully"
        );

        // Deregister the enclave before even registering it
        let req = test::TestRequest::delete().uri("/deregister").to_request();

        let resp = test::call_service(&app, req).await;

        assert_eq!(resp.status(), http::StatusCode::BAD_REQUEST);
        assert_eq!(
            resp.into_body().try_into_bytes().unwrap(),
            "Enclave is not registered yet."
        );

        // Register the enclave with an invalid attestation hex string
        let req = test::TestRequest::post()
            .uri("/register")
            .set_json(&json!({
                "attestation": "0x32255",
                "pcr_0": "0x",
                "pcr_1": "0x",
                "pcr_2": "0x",
                "timestamp": 2160,
                "stake_amount": 100,
                "request_chain_data": [CHAIN_ID]
            }))
            .to_request();

        let resp = test::call_service(&app, req).await;

        assert_eq!(resp.status(), http::StatusCode::BAD_REQUEST);
        assert_eq!(
            resp.into_body().try_into_bytes().unwrap(),
            "Invalid format of attestation."
        );

        // Register the enclave with valid data points
        let req = test::TestRequest::post()
            .uri("/register")
            .set_json(&json!({
                "attestation": REGISTER_ATTESTATION,
                "pcr_0": REGISTER_PCR_0,
                "pcr_1": REGISTER_PCR_1,
                "pcr_2": REGISTER_PCR_2,
                "timestamp": REGISTER_TIMESTAMP,
                "stake_amount": REGISTER_STAKE_AMOUNT,
                "request_chain_data": [CHAIN_ID]
            }))
            .to_request();

        let resp = test::call_service(&app, req).await;

        assert_eq!(resp.status(), http::StatusCode::OK);
        assert!(resp
            .into_body()
            .try_into_bytes()
            .unwrap()
            .starts_with("Enclave Node successfully registered on the common chain".as_bytes()));

        // Register the enclave again before deregistering
        let req = test::TestRequest::post()
            .uri("/register")
            .set_json(&json!({
                "attestation": REGISTER_ATTESTATION,
                "pcr_0": REGISTER_PCR_0,
                "pcr_1": REGISTER_PCR_1,
                "pcr_2": REGISTER_PCR_2,
                "timestamp": REGISTER_TIMESTAMP,
                "stake_amount": REGISTER_STAKE_AMOUNT,
                "request_chain_data": [CHAIN_ID]
            }))
            .to_request();

        let resp = test::call_service(&app, req).await;

        assert_eq!(resp.status(), http::StatusCode::BAD_REQUEST);
        assert_eq!(
            resp.into_body().try_into_bytes().unwrap(),
            "Enclave has already been registered."
        );

        sleep(Duration::from_secs(2)).await;
        // Deregister the enclave
        let req = test::TestRequest::delete().uri("/deregister").to_request();

        let resp = test::call_service(&app, req).await;

        assert_eq!(resp.status(), http::StatusCode::OK);
        assert!(resp.into_body().try_into_bytes().unwrap().starts_with(
            "Enclave Node successfully deregistered from the common chain".as_bytes()
        ));

        // Deregister the enclave again before registering it
        let req = test::TestRequest::delete().uri("/deregister").to_request();

        let resp = test::call_service(&app, req).await;

        assert_eq!(resp.status(), http::StatusCode::BAD_REQUEST);
        assert_eq!(
            resp.into_body().try_into_bytes().unwrap(),
            "Enclave is not registered yet."
        );
    }

    async fn generate_contracts_client() -> ContractsClient {
        let app_state = generate_app_state().await;
        let app = test::init_service(new_app(app_state.clone())).await;

        let req = test::TestRequest::post()
            .uri("/inject-key")
            .set_json(&json!({
                "operator_secret": WALLET_PRIVATE_KEY
            }))
            .to_request();

        test::call_service(&app, req).await;

        // Register the enclave again before deregistering
        let req = test::TestRequest::post()
            .uri("/register")
            .set_json(&json!({
                "attestation": REGISTER_ATTESTATION,
                "pcr_0": REGISTER_PCR_0,
                "pcr_1": REGISTER_PCR_1,
                "pcr_2": REGISTER_PCR_2,
                "timestamp": REGISTER_TIMESTAMP,
                "stake_amount": REGISTER_STAKE_AMOUNT,
                "request_chain_data": [CHAIN_ID]
            }))
            .to_request();

        test::call_service(&app, req).await;

        let enclave_owner = app_state.enclave_owner.lock().unwrap().clone();

        let wallet = app_state.wallet.lock().unwrap().clone().unwrap();
        let signer_wallet = wallet.clone().with_chain_id(app_state.common_chain_id);

        let signer_address = signer_wallet.address();
        let http_rpc_client = Provider::<Http>::try_connect(&app_state.common_chain_http_url)
            .await
            .unwrap();

        let http_rpc_client = Arc::new(
            http_rpc_client
                .with_signer(signer_wallet.clone())
                .nonce_manager(signer_address),
        );

        let gateway_epoch_state: Arc<RwLock<BTreeMap<u64, BTreeMap<Address, GatewayData>>>> =
            Arc::new(RwLock::new(BTreeMap::new()));
        let gateway_state_epoch_waitlist = Arc::new(RwLock::new(HashMap::new()));

        let mut request_chain_clients: HashMap<u64, Arc<RequestChainClient>> = HashMap::new();

        let contract = RelayContract::new(
            H160::from_str(RELAY_CONTRACT_ADDR).unwrap(),
            http_rpc_client.clone(),
        );

        let request_chain_client = Arc::from(RequestChainClient {
            chain_id: CHAIN_ID,
            contract_address: H160::from_str(RELAY_CONTRACT_ADDR).unwrap(),
            contract,
            ws_rpc_url: WS_URL.to_owned(),
            request_chain_start_block_number: 0,
        });
        request_chain_clients.insert(CHAIN_ID, request_chain_client);

        let contracts_client = ContractsClient::new(
            enclave_owner,
            app_state.enclave_signer_key.clone(),
            app_state.enclave_address,
            signer_wallet,
            &app_state.common_chain_ws_url,
            http_rpc_client.clone(),
            &app_state.gateways_contract_addr,
            &app_state.gateway_jobs_contract_addr,
            gateway_epoch_state,
            [CHAIN_ID].into(),
            request_chain_clients,
            app_state.epoch,
            app_state.time_interval,
            gateway_state_epoch_waitlist,
            0,
        )
        .await;

        contracts_client
    }

    async fn add_gateway_epoch_state(
        contracts_client: Arc<ContractsClient>,
        num: Option<u64>,
        add_self: Option<bool>,
    ) {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let current_cycle = (ts - contracts_client.epoch - OFFEST_FOR_GATEWAY_EPOCH_STATE_CYCLE)
            / contracts_client.time_interval;

        let add_self = add_self.unwrap_or(true);

        let mut gateway_epoch_state_guard = contracts_client.gateway_epoch_state.write().unwrap();

        let mut num = num.unwrap_or(1);

        if add_self {
            gateway_epoch_state_guard
                .entry(current_cycle)
                .or_insert(BTreeMap::new())
                .insert(
                    contracts_client.enclave_address,
                    GatewayData {
                        last_block_number: 5600 as u64,
                        address: contracts_client.enclave_address,
                        stake_amount: U256::from(100),
                        req_chain_ids: BTreeSet::from([CHAIN_ID]),
                        draining: false,
                    },
                );

            num -= 1;
        }

        for _ in 0..num {
            gateway_epoch_state_guard
                .entry(current_cycle)
                .or_insert(BTreeMap::new())
                .insert(
                    Address::random(),
                    GatewayData {
                        last_block_number: 5600 as u64,
                        address: Address::random(),
                        stake_amount: U256::from(100),
                        req_chain_ids: BTreeSet::from([CHAIN_ID]),
                        draining: false,
                    },
                );
        }
    }

    async fn generate_job_relayed_log(job_id: Option<U256>, job_starttime: u64) -> Log {
        let job_id = job_id.unwrap_or(U256::from(1));

        Log {
            address: H160::from_str(RELAY_CONTRACT_ADDR).unwrap(),
            topics: vec![
                keccak256(
                   "JobRelayed(uint256,bytes32,bytes,uint256,uint256,uint256,uint256,address,address,uint256,uint256)",
                )
                .into(),
                H256::from_uint(&job_id),
            ],
            data: encode(&[
                Token::FixedBytes(
                    hex::decode(
                        "9468bb6a8e85ed11e292c8cac0c1539df691c8d8ec62e7dbfa9f1bd7f504e46e"
                            .to_owned(),
                    )
                    .unwrap(),
                ),
                Token::Bytes(
                    serde_json::to_vec(&json!({
                        "num": 10
                    }))
                    .unwrap(),
                ),
                Token::Uint(2000.into()),
                Token::Uint(20.into()),
                Token::Uint(100.into()),
                Token::Uint(100.into()),
                Token::Uint(U256::from(job_starttime)),
            ])
            .into(),
            ..Default::default()
        }
    }

    async fn generate_job_responded_log(job_id: Option<U256>) -> Log {
        let job_id = job_id.unwrap_or(U256::from(1));

        Log {
            address: H160::from_str(JOB_CONTRACT_ADDR).unwrap(),
            topics: vec![
                keccak256("JobResponded(uint256,uint256,bytes,uint256,uint8,uint8").into(),
                H256::from_uint(&job_id),
                H256::from_uint(&CHAIN_ID.into()),
            ],
            data: encode(&[
                Token::Bytes([].into()),
                Token::Uint(U256::from(1000)),
                Token::Uint((0 as u8).into()),
                Token::Uint((1 as u8).into()),
            ])
            .into(),
            ..Default::default()
        }
    }

    async fn generate_generic_job(job_id: Option<U256>, job_starttime: Option<u64>) -> Job {
        let job_id = job_id.unwrap_or(U256::from(1));

        Job {
            job_id,
            request_chain_id: CHAIN_ID,
            tx_hash: hex::decode(
                "9468bb6a8e85ed11e292c8cac0c1539df691c8d8ec62e7dbfa9f1bd7f504e46e".to_owned(),
            )
            .unwrap(),
            code_input: serde_json::to_vec(&json!({
                "num": 10
            }))
            .unwrap()
            .into(),
            user_timeout: U256::from(2000),
            starttime: U256::from(
                job_starttime.unwrap_or(
                    SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap()
                        .as_secs(),
                ),
            ),
            job_owner: H160::from_str(RELAY_CONTRACT_ADDR).unwrap(),
            job_type: GatewayJobType::JobRelay,
            sequence_number: 1 as u8,
            gateway_address: None,
        }
    }

    async fn generate_generic_response_job(job_id: Option<U256>) -> ResponseJob {
        let job_id = job_id.unwrap_or(U256::from(1));

        ResponseJob {
            job_id,
            request_chain_id: CHAIN_ID,
            output: Bytes::default(),
            total_time: U256::from(1000),
            error_code: 0 as u8,
            job_type: GatewayJobType::JobResponded,
            gateway_address: None,
        }
    }

    #[actix_web::test]
    async fn test_get_job_from_job_relay_event() {
        let contracts_client = Arc::from(generate_contracts_client().await);

        let job_starttime = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let log = generate_job_relayed_log(None, job_starttime).await;

        let expected_job = generate_generic_job(None, Some(job_starttime)).await;

        let job = contracts_client
            .get_job_from_job_relay_event(log, 1 as u8, CHAIN_ID)
            .await
            .unwrap();

        assert_eq!(job, expected_job);
    }

    #[actix_web::test]
    async fn test_get_job_from_job_relay_event_invalid_log() {
        let contracts_client = Arc::from(generate_contracts_client().await);

        let log = Log {
            address: H160::from_str(RELAY_CONTRACT_ADDR).unwrap(),
            topics: vec![
                keccak256(
                    "JobRelayed(uint256,bytes32,bytes,uint256,uint256,uint256,uint256,address,address,uint256,uint256)",
                )
                .into(),
                H256::from_uint(&U256::from(1)),
            ],
            data: EthBytes::from(vec![0x00]),
            ..Default::default()
        };

        let job = contracts_client
            .get_job_from_job_relay_event(log, 1 as u8, CHAIN_ID)
            .await;

        // expect an error
        assert_eq!(job.err().unwrap().to_string(), "Error while decoding event");
    }

    #[actix_web::test]
    async fn test_select_gateway_for_job_id() {
        let contracts_client = Arc::from(generate_contracts_client().await);

        let job = generate_generic_job(None, None).await;

        add_gateway_epoch_state(contracts_client.clone(), None, None).await;

        let req_chain_client =
            contracts_client.request_chain_clients[&job.request_chain_id].clone();
        let gateway_address = contracts_client
            .select_gateway_for_job_id(
                job.clone(),
                job.starttime.as_u64(),
                job.sequence_number,
                req_chain_client,
            )
            .await
            .unwrap();

        assert_eq!(gateway_address, contracts_client.enclave_address);
    }

    #[actix_web::test]
    async fn test_select_gateway_for_job_id_no_cycle_state() {
        let contracts_client = Arc::from(generate_contracts_client().await);

        let job = generate_generic_job(None, None).await;

        let req_chain_client =
            contracts_client.request_chain_clients[&job.request_chain_id].clone();
        let gateway_address = contracts_client
            .select_gateway_for_job_id(
                job.clone(),
                job.starttime.as_u64(),
                job.sequence_number,
                req_chain_client,
            )
            .await
            .unwrap();

        assert_eq!(gateway_address, Address::zero());

        let waitlisted_jobs_hashmap = contracts_client
            .gateway_epoch_state_waitlist
            .read()
            .unwrap()
            .clone();

        let waitlisted_jobs: Vec<Vec<Job>> = waitlisted_jobs_hashmap.values().cloned().collect();

        assert_eq!(waitlisted_jobs.len(), 1);
        assert_eq!(waitlisted_jobs[0].len(), 1);
        assert_eq!(waitlisted_jobs[0][0], job);
    }

    #[actix_web::test]
    async fn test_select_gateway_for_job_id_multiple_gateways() {
        let contracts_client = Arc::from(generate_contracts_client().await);

        let job = generate_generic_job(None, None).await;

        add_gateway_epoch_state(contracts_client.clone(), Some(5), None).await;

        let req_chain_client =
            contracts_client.request_chain_clients[&job.request_chain_id].clone();
        let gateway_address = contracts_client
            .select_gateway_for_job_id(
                job.clone(),
                job.starttime.as_u64(),
                job.sequence_number,
                req_chain_client,
            )
            .await
            .unwrap();

        let total_stake = 100 * 5 as u64;
        let seed = job.starttime.as_u64();
        let mut rng = StdRng::seed_from_u64(seed);
        for _ in 0..job.sequence_number - 1 {
            let _ = rng.gen_range(1..=total_stake);
        }
        let random_number = rng.gen_range(1..=total_stake);
        let indx = random_number / 100;
        let expected_gateway_address = contracts_client
            .gateway_epoch_state
            .read()
            .unwrap()
            .values()
            .nth(0 as usize)
            .unwrap()
            .values()
            .nth(indx as usize)
            .unwrap()
            .address;

        assert_eq!(gateway_address, expected_gateway_address);
    }

    #[actix_web::test]
    async fn test_select_gateway_for_job_id_multiple_gateways_seq_number() {
        let contracts_client = Arc::from(generate_contracts_client().await);

        let mut job = generate_generic_job(None, None).await;
        job.sequence_number = 5;

        add_gateway_epoch_state(contracts_client.clone(), Some(5), None).await;

        let req_chain_client =
            contracts_client.request_chain_clients[&job.request_chain_id].clone();
        let gateway_address = contracts_client
            .select_gateway_for_job_id(
                job.clone(),
                job.starttime.as_u64(),
                job.sequence_number,
                req_chain_client,
            )
            .await
            .unwrap();

        let total_stake = 100 * 5 as u64;
        let seed = job.starttime.as_u64();
        let mut rng = StdRng::seed_from_u64(seed);
        for _ in 0..job.sequence_number - 1 {
            let _ = rng.gen_range(1..=total_stake);
        }
        let random_number = rng.gen_range(1..=total_stake);
        let indx = random_number / 100;
        let expected_gateway_address = contracts_client
            .gateway_epoch_state
            .read()
            .unwrap()
            .values()
            .nth(0 as usize)
            .unwrap()
            .values()
            .nth(indx as usize)
            .unwrap()
            .address;

        assert_eq!(gateway_address, expected_gateway_address);
    }

    #[actix_web::test]
    async fn test_job_placed_handler() {
        let contracts_client = Arc::from(generate_contracts_client().await);

        let mut job = generate_generic_job(None, None).await;

        add_gateway_epoch_state(contracts_client.clone(), None, None).await;

        let (req_chain_tx, mut com_chain_rx) = channel::<(Job, Arc<ContractsClient>)>(100);

        let job_clone = job.clone();
        let contracts_client_clone = contracts_client.clone();
        contracts_client_clone
            .job_placed_handler(job_clone, req_chain_tx.clone())
            .await;

        if let Some((rx_job, rx_contracts_client)) = com_chain_rx.recv().await {
            job.gateway_address = Some(contracts_client.enclave_address);
            assert_eq!(rx_job, job);

            assert_eq!(rx_contracts_client.active_jobs.read().unwrap().len(), 1);
            assert_eq!(
                rx_contracts_client
                    .active_jobs
                    .read()
                    .unwrap()
                    .get(&job.job_id),
                Some(&rx_job)
            );

            assert_eq!(
                rx_contracts_client
                    .gateway_epoch_state_waitlist
                    .read()
                    .unwrap()
                    .len(),
                0
            );
        } else {
            assert!(false);
        }

        assert!(com_chain_rx.recv().await.is_none());
    }

    #[actix_web::test]
    async fn test_job_placed_handler_selected_gateway_not_self() {
        let contracts_client = Arc::from(generate_contracts_client().await);

        let job = generate_generic_job(None, None).await;

        add_gateway_epoch_state(contracts_client.clone(), Some(4), Some(false)).await;

        let (req_chain_tx, mut com_chain_rx) = channel::<(Job, Arc<ContractsClient>)>(100);

        let job_clone = job.clone();
        let contracts_client_clone = contracts_client.clone();
        contracts_client_clone
            .job_placed_handler(job_clone, req_chain_tx.clone())
            .await;

        assert!(com_chain_rx.recv().await.is_none());

        assert_eq!(
            contracts_client
                .active_jobs
                .read()
                .unwrap()
                .get(&job.job_id),
            None
        );

        assert_eq!(
            contracts_client
                .gateway_epoch_state_waitlist
                .read()
                .unwrap()
                .len(),
            0
        );
    }

    #[actix_web::test]
    async fn test_job_placed_handler_no_cycle_state() {
        let contracts_client = Arc::from(generate_contracts_client().await);

        let job = generate_generic_job(None, None).await;

        let (req_chain_tx, _) = channel::<(Job, Arc<ContractsClient>)>(100);

        contracts_client
            .clone()
            .job_placed_handler(job.clone(), req_chain_tx.clone())
            .await;

        let waitlisted_jobs_hashmap = contracts_client
            .gateway_epoch_state_waitlist
            .read()
            .unwrap()
            .clone();

        let waitlisted_jobs: Vec<Vec<Job>> = waitlisted_jobs_hashmap.values().cloned().collect();

        assert_eq!(waitlisted_jobs.len(), 1);
        assert_eq!(waitlisted_jobs[0].len(), 1);
        assert_eq!(waitlisted_jobs[0][0], job);

        assert_eq!(
            contracts_client
                .active_jobs
                .read()
                .unwrap()
                .get(&job.job_id),
            None
        );
    }

    #[actix_web::test]
    async fn test_job_relayed_slash_timer_txn_success() {
        let contracts_client = Arc::from(generate_contracts_client().await);

        let mut job = generate_generic_job(None, None).await;

        add_gateway_epoch_state(contracts_client.clone(), Some(5), None).await;
        job.gateway_address = Some(
            contracts_client
                .gateway_epoch_state
                .read()
                .unwrap()
                .values()
                .nth(0 as usize)
                .unwrap()
                .values()
                .nth(1 as usize)
                .unwrap()
                .address,
        );

        let (req_chain_tx, mut com_chain_rx) = channel::<(Job, Arc<ContractsClient>)>(100);

        contracts_client
            .job_relayed_slash_timer(job.clone(), Some(1 as u64), req_chain_tx)
            .await;

        assert!(com_chain_rx.recv().await.is_none());
    }

    #[actix_web::test]
    async fn test_job_relayed_slash_timer_txn_fail_retry() {
        let contracts_client = Arc::from(generate_contracts_client().await);

        let mut job = generate_generic_job(Some(U256::from(2)), None).await;

        add_gateway_epoch_state(contracts_client.clone(), Some(5), None).await;
        job.gateway_address = Some(
            contracts_client
                .gateway_epoch_state
                .read()
                .unwrap()
                .values()
                .nth(0 as usize)
                .unwrap()
                .values()
                .nth(1 as usize)
                .unwrap()
                .address,
        );

        let (req_chain_tx, mut com_chain_rx) = channel::<(Job, Arc<ContractsClient>)>(100);

        contracts_client
            .clone()
            .job_relayed_slash_timer(job.clone(), Some(1 as u64), req_chain_tx)
            .await;

        if let Some((rx_job, _rx_com_chain_client)) = com_chain_rx.recv().await {
            job.job_type = GatewayJobType::SlashGatewayJob;
            assert_eq!(rx_job, job);
        } else {
            assert!(false);
        }

        assert!(com_chain_rx.recv().await.is_none());
    }

    #[actix_web::test]
    async fn test_job_relayed_slash_timer_txn_fail_max_retry() {
        let contracts_client = Arc::from(generate_contracts_client().await);

        let mut job = generate_generic_job(Some(U256::from(2)), None).await;

        add_gateway_epoch_state(contracts_client.clone(), Some(5), None).await;
        job.gateway_address = Some(
            contracts_client
                .gateway_epoch_state
                .read()
                .unwrap()
                .values()
                .nth(0 as usize)
                .unwrap()
                .values()
                .nth(1 as usize)
                .unwrap()
                .address,
        );
        job.sequence_number = MAX_GATEWAY_RETRIES;

        let (req_chain_tx, mut com_chain_rx) = channel::<(Job, Arc<ContractsClient>)>(100);

        contracts_client
            .clone()
            .job_relayed_slash_timer(job.clone(), Some(1 as u64), req_chain_tx)
            .await;

        if let Some((rx_job, _rx_com_chain_client)) = com_chain_rx.recv().await {
            job.job_type = GatewayJobType::SlashGatewayJob;
            assert_eq!(rx_job, job);
        } else {
            assert!(false);
        }

        assert!(com_chain_rx.recv().await.is_none());
    }

    #[actix_web::test]
    async fn test_cancel_job_with_job_id_single_active_job() {
        let contracts_client = Arc::from(generate_contracts_client().await);

        let job = generate_generic_job(None, None).await;
        contracts_client
            .active_jobs
            .write()
            .unwrap()
            .insert(job.job_id, job.clone());

        assert_eq!(contracts_client.active_jobs.read().unwrap().len(), 1);

        contracts_client
            .clone()
            .cancel_job_with_job_id(job.job_id)
            .await;

        assert_eq!(contracts_client.active_jobs.read().unwrap().len(), 0);
    }

    #[actix_web::test]
    async fn test_cancel_job_with_job_id_multiple_active_jobs() {
        let contracts_client = Arc::from(generate_contracts_client().await);

        let job = generate_generic_job(None, None).await;
        contracts_client
            .active_jobs
            .write()
            .unwrap()
            .insert(job.job_id, job.clone());

        let job2 = generate_generic_job(Some(U256::from(2)), None).await;
        contracts_client
            .active_jobs
            .write()
            .unwrap()
            .insert(job2.job_id, job2.clone());

        assert_eq!(contracts_client.active_jobs.read().unwrap().len(), 2);

        contracts_client
            .clone()
            .cancel_job_with_job_id(job.job_id)
            .await;

        assert_eq!(contracts_client.active_jobs.read().unwrap().len(), 1);
        assert_eq!(
            contracts_client
                .active_jobs
                .read()
                .unwrap()
                .get(&job2.job_id),
            Some(&job2)
        );
    }

    #[actix_web::test]
    async fn test_cancel_job_with_job_id_no_active_jobs() {
        let contracts_client = Arc::from(generate_contracts_client().await);

        let job = generate_generic_job(None, None).await;

        assert_eq!(contracts_client.active_jobs.read().unwrap().len(), 0);

        contracts_client
            .clone()
            .cancel_job_with_job_id(job.job_id)
            .await;

        assert_eq!(contracts_client.active_jobs.read().unwrap().len(), 0);
    }

    #[actix_web::test]
    async fn test_get_job_from_job_responded_event() {
        let contracts_client = Arc::from(generate_contracts_client().await);

        let log = generate_job_responded_log(None).await;

        let expected_job = generate_generic_response_job(None).await;

        let job = contracts_client
            .get_job_from_job_responded_event(log)
            .await
            .unwrap();

        assert_eq!(job, expected_job);
    }

    // TODO: tests for gateway_epoch_state_service
}
