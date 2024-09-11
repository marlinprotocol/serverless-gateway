use ethers::{
    abi::{decode, ParamType},
    types::{BigEndianHash, Bytes, Log, U256},
    utils::keccak256,
};
use log::{error, info};
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::mpsc::{Receiver, Sender},
    time::{sleep_until, Instant},
};

use crate::{
    chain_util::LogsProvider,
    constant::{
        GATEWAY_BLOCK_STATES_TO_MAINTAIN, REQUEST_CHAIN_JOB_SUBSCRIPTION_JOB_PARAMS_UPDATED_EVENT,
        REQUEST_CHAIN_JOB_SUBSCRIPTION_STARTED_EVENT,
        REQUEST_CHAIN_JOB_SUBSCRIPTION_TERMINATION_PARAMS_UPDATED_EVENT,
    },
    error::ServerlessError,
    model::{
        ContractsClient, GatewayJobType, Job, JobMode, JobSubscriptionAction,
        JobSubscriptionChannelType, SubscriptionJob, SubscriptionJobHeap,
    },
};

fn unix_timestamp_to_instant(timestamp: u64) -> Instant {
    let duration = Duration::from_secs(timestamp);
    let system_time = UNIX_EPOCH + duration;
    Instant::now()
        + system_time
            .duration_since(SystemTime::now())
            .unwrap_or_default()
}

impl PartialEq for SubscriptionJobHeap {
    fn eq(&self, other: &Self) -> bool {
        self.next_trigger_time == other.next_trigger_time
    }
}

impl Eq for SubscriptionJobHeap {}

impl PartialOrd for SubscriptionJobHeap {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(
            self.next_trigger_time
                .cmp(&other.next_trigger_time)
                .reverse(),
        )
    }
}

impl Ord for SubscriptionJobHeap {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.next_trigger_time
            .cmp(&other.next_trigger_time)
            .reverse()
    }
}

pub async fn process_historic_job_subscriptions(
    contracts_client: &Arc<ContractsClient>,
    req_chain_tx: Sender<Job>,
    job_sub_tx: Sender<JobSubscriptionChannelType>,
) {
    info!("Processing Historic Job Subscriptions on Request Chains");

    for request_chain_id in contracts_client.request_chain_ids.clone() {
        let contracts_client_clone = contracts_client.clone();
        let req_chain_tx_clone = req_chain_tx.clone();

        let job_sub_tx_clone = job_sub_tx.clone();
        tokio::spawn(async move {
            process_historic_subscription_jobs_on_request_chain(
                &contracts_client_clone,
                request_chain_id,
                req_chain_tx_clone,
                job_sub_tx_clone,
            )
            .await;
        });
    }
}

pub async fn process_historic_subscription_jobs_on_request_chain(
    contracts_client: &Arc<ContractsClient>,
    request_chain_id: u64,
    req_chain_tx: Sender<Job>,
    job_sub_tx: Sender<JobSubscriptionChannelType>,
) {
    let logs = contracts_client
        .request_chain_historic_subscription_jobs(
            contracts_client
                .request_chain_clients
                .get(&request_chain_id)
                .unwrap(),
        )
        .await
        .unwrap();

    for log in logs {
        if log.topics[0] == keccak256(REQUEST_CHAIN_JOB_SUBSCRIPTION_STARTED_EVENT).into() {
            let sub_id = add_subscription_job(
                contracts_client,
                log,
                request_chain_id,
                req_chain_tx.clone(),
                true,
            )
            .unwrap();
            if sub_id == U256::zero() {
                continue;
            }
            job_sub_tx
                .send(JobSubscriptionChannelType {
                    subscription_action: JobSubscriptionAction::Add,
                    subscription_id: sub_id,
                })
                .await
                .unwrap();
        } else if log.topics[0]
            == keccak256(REQUEST_CHAIN_JOB_SUBSCRIPTION_JOB_PARAMS_UPDATED_EVENT).into()
        {
            let _ = update_subscription_job_params(contracts_client, log);
        } else if log.topics[0]
            == keccak256(REQUEST_CHAIN_JOB_SUBSCRIPTION_TERMINATION_PARAMS_UPDATED_EVENT).into()
        {
            let _ = update_subscription_job_termination_params(contracts_client, log);
        }
    }
}

pub async fn job_subscription_manager(
    contracts_client: Arc<ContractsClient>,
    mut rx: Receiver<JobSubscriptionChannelType>,
    req_chain_tx: Sender<Job>,
) {
    loop {
        let next_trigger_time: Option<u64>;
        // Scope for read lock on subscription_job_heap
        {
            let subscription_heap_guard = contracts_client.subscription_job_heap.read().unwrap();
            next_trigger_time = subscription_heap_guard.peek().map(|t| t.next_trigger_time);
        }

        tokio::select! {
            Some(job_subscription_channel_data) = rx.recv() => {
                match job_subscription_channel_data.subscription_action {
                    JobSubscriptionAction::Add => {
                        info!(
                            "Added new subscription JobSubscriptionId: {}",
                            job_subscription_channel_data.subscription_id
                        );
                    }
                }
            }
            _ = sleep_until(next_trigger_time.map(|t|
                unix_timestamp_to_instant(t)
            ).unwrap_or_else(Instant::now)), if next_trigger_time.is_some() => {
                let contracts_client_clone = contracts_client.clone();
                let subscription: Option<SubscriptionJobHeap>;
                {
                    let mut subscription_job_heap = contracts_client.subscription_job_heap
                        .write()
                        .unwrap();
                    subscription = subscription_job_heap.pop();
                }

                if subscription.is_none() {
                    error!("Subscription Job Triggered but no subscription found");
                    continue;
                }
                let subscription = subscription.unwrap();

                let req_chain_tx_clone = req_chain_tx.clone();

                let subscription_job: Option<SubscriptionJob>;
                // Scope for read lock on subscription_jobs
                {
                    let subscription_jobs_guard = contracts_client.subscription_jobs.read().unwrap();
                    subscription_job = subscription_jobs_guard
                        .get(&subscription.subscription_id)
                        .cloned();
                }

                if subscription_job.is_none() {
                    info!(
                        "Job No longer active for Subscription - Subscription ID: {}",
                        subscription.subscription_id
                    );
                    return;
                }

                tokio::spawn(async move {
                    trigger_subscription_job(
                        subscription_job.unwrap(),
                        subscription.next_trigger_time,
                        contracts_client_clone,
                        req_chain_tx_clone
                    ).await;
                });
                add_next_trigger_time_to_heap(
                    &contracts_client,
                    subscription.subscription_id.clone(),
                    subscription.next_trigger_time,
                    false,
                );
            }
            else => {
                info!("Awaiting");
                // do nothing
            }
        }
    }
}

pub fn add_subscription_job(
    contracts_client: &Arc<ContractsClient>,
    subscription_log: Log,
    request_chain_id: u64,
    req_chain_tx: Sender<Job>,
    is_historic_log: bool,
) -> Result<U256, ServerlessError> {
    let types = vec![
        ParamType::Uint(256),
        ParamType::Uint(256),
        ParamType::Uint(256),
        ParamType::Uint(256),
        ParamType::Address,
        ParamType::FixedBytes(32),
        ParamType::Bytes,
        ParamType::Uint(256),
    ];

    let decoded = decode(&types, &subscription_log.data.0);
    let decoded = match decoded {
        Ok(decoded) => decoded,
        Err(e) => {
            error!("Failed to decode subscription log: {}", e);
            return Err(ServerlessError::LogDecodeFailure);
        }
    };

    let subscription_job = SubscriptionJob {
        subscription_id: subscription_log.topics[1].into_uint(),
        request_chain_id,
        subscriber: subscription_log.topics[2].into(),
        interval: decoded[0].clone().into_uint().unwrap().into(),
        termination_time: decoded[2].clone().into_uint().unwrap().into(),
        user_timeout: decoded[3].clone().into_uint().unwrap().into(),
        tx_hash: decoded[5].clone().into_fixed_bytes().unwrap(),
        code_input: decoded[6].clone().into_bytes().unwrap().into(),
        starttime: decoded[7].clone().into_uint().unwrap().into(),
    };

    let current_timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    if subscription_job.termination_time.as_u64() < current_timestamp {
        info!(
            "Subscription Job has reached termination time - Subscription ID: {}",
            subscription_job.subscription_id
        );
        return Ok(0.into());
    }

    // Scope for write lock on subscription_jobs
    {
        let mut subscription_jobs = contracts_client.subscription_jobs.write().unwrap();
        subscription_jobs.insert(subscription_job.subscription_id, subscription_job.clone());
    }

    let mut to_trigger_first_instance = true;
    if is_historic_log {
        let minimum_timestamp_for_job = current_timestamp
            - ((GATEWAY_BLOCK_STATES_TO_MAINTAIN + 1) * contracts_client.time_interval)
            - contracts_client.offset_for_epoch;

        if subscription_job.starttime.as_u64() < minimum_timestamp_for_job {
            to_trigger_first_instance = false;
        }
    }
    if to_trigger_first_instance {
        let contracts_client_clone = contracts_client.clone();
        let subscription_job_clone = subscription_job.clone();
        tokio::spawn(async move {
            trigger_subscription_job(
                subscription_job_clone,
                subscription_job.starttime.as_u64(),
                contracts_client_clone,
                req_chain_tx,
            )
        });
    }

    add_next_trigger_time_to_heap(
        &contracts_client,
        subscription_job.subscription_id,
        subscription_job.starttime.as_u64(),
        is_historic_log,
    );
    Ok(subscription_job.subscription_id)
}

fn add_next_trigger_time_to_heap(
    contracts_client: &Arc<ContractsClient>,
    subscription_id: U256,
    previous_trigger_time: u64,
    is_historic_log: bool,
) {
    let subscription_job = contracts_client
        .subscription_jobs
        .read()
        .unwrap()
        .get(&subscription_id)
        .cloned();

    if subscription_job.is_none() {
        error!(
            "Subscription Job not found for Subscription ID: {}",
            subscription_id
        );
        return;
    }

    let subscription_job = subscription_job.unwrap();

    let mut next_trigger_time = previous_trigger_time + subscription_job.interval.as_u64();

    if next_trigger_time > subscription_job.termination_time.as_u64() {
        info!(
            "Subscription Job has reached termination time - Subscription ID: {}",
            subscription_job.subscription_id
        );
        // Scope for write lock on subscription_jobs
        {
            let mut subscription_jobs = contracts_client.subscription_jobs.write().unwrap();
            subscription_jobs.remove(&subscription_id);
        }
        return;
    }

    if is_historic_log {
        let current_timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let minimum_timestamp_for_job = current_timestamp
            - ((GATEWAY_BLOCK_STATES_TO_MAINTAIN + 1) * contracts_client.time_interval)
            - contracts_client.offset_for_epoch;

        if next_trigger_time < minimum_timestamp_for_job {
            let instance_count = ((minimum_timestamp_for_job
                - subscription_job.starttime.as_u64())
                / subscription_job.interval.as_u64())
                + 1;

            next_trigger_time = subscription_job.starttime.as_u64()
                + instance_count * subscription_job.interval.as_u64();
        }
    }

    // Scope for write lock on subscription_job_heap
    {
        let mut subscription_job_heap = contracts_client.subscription_job_heap.write().unwrap();
        subscription_job_heap.push(SubscriptionJobHeap {
            subscription_id: subscription_job.subscription_id,
            next_trigger_time,
        });
    }
}

async fn trigger_subscription_job(
    subscription_job: SubscriptionJob,
    trigger_timestamp: u64,
    contracts_client: Arc<ContractsClient>,
    req_chain_tx: Sender<Job>,
) {
    info!(
        "Triggering subscription job with ID: {}",
        subscription_job.subscription_id
    );

    let job = subscription_job_to_relay_job(subscription_job, trigger_timestamp);

    contracts_client
        .job_relayed_handler(job, req_chain_tx)
        .await;
}

fn subscription_job_to_relay_job(subscription_job: SubscriptionJob, trigger_timestamp: u64) -> Job {
    let instance_count =
        (U256::from(trigger_timestamp) - subscription_job.starttime) / subscription_job.interval;
    let job_id = subscription_job.subscription_id + instance_count;
    let instance_starttime =
        subscription_job.starttime + instance_count * subscription_job.interval;

    Job {
        job_id,
        request_chain_id: subscription_job.request_chain_id,
        tx_hash: subscription_job.tx_hash,
        code_input: subscription_job.code_input,
        user_timeout: subscription_job.user_timeout,
        starttime: instance_starttime,
        job_owner: subscription_job.subscriber,
        job_type: GatewayJobType::JobRelay,
        sequence_number: 1,
        gateway_address: None,
        job_mode: JobMode::Subscription,
    }
}

pub fn update_subscription_job_params(
    contracts_client: &Arc<ContractsClient>,
    subscription_log: Log,
) -> Result<(), ServerlessError> {
    let types = vec![ParamType::FixedBytes(32), ParamType::Bytes];

    let decoded = decode(&types, &subscription_log.data.0);
    let decoded = match decoded {
        Ok(decoded) => decoded,
        Err(e) => {
            error!("Failed to decode subscription log: {}", e);
            return Err(ServerlessError::LogDecodeFailure);
        }
    };

    let subscription_id = subscription_log.topics[1].into_uint();

    let subscription_job = contracts_client
        .subscription_jobs
        .read()
        .unwrap()
        .get(&subscription_id)
        .cloned();

    if subscription_job.is_none() {
        error!(
            "Subscription Job not found for Subscription ID: {}",
            subscription_id
        );
        return Err(ServerlessError::NoSubscriptionJobFound(subscription_id));
    }

    let new_tx_hash = decoded[0].clone().into_fixed_bytes().unwrap();
    let new_code_input: Bytes = decoded[1].clone().into_bytes().unwrap().into();

    // Update the subscription job
    // Scope for write lock on subscription_jobs
    {
        let mut subscription_jobs = contracts_client.subscription_jobs.write().unwrap();
        let subscription_job = subscription_jobs.get_mut(&subscription_id).unwrap();
        subscription_job.tx_hash = new_tx_hash;
        subscription_job.code_input = new_code_input;
    }

    Ok(())
}

pub fn update_subscription_job_termination_params(
    contracts_client: &Arc<ContractsClient>,
    subscription_log: Log,
) -> Result<(), ServerlessError> {
    let types = vec![ParamType::Uint(256)];

    let decoded = decode(&types, &subscription_log.data.0);
    let decoded = match decoded {
        Ok(decoded) => decoded,
        Err(e) => {
            error!("Failed to decode subscription log: {}", e);
            return Err(ServerlessError::LogDecodeFailure);
        }
    };

    let subscription_id = subscription_log.topics[1].into_uint();

    let subscription_job = contracts_client
        .subscription_jobs
        .read()
        .unwrap()
        .get(&subscription_id)
        .cloned();

    if subscription_job.is_none() {
        error!(
            "Subscription Job not found for Subscription ID: {}",
            subscription_id
        );
        return Err(ServerlessError::NoSubscriptionJobFound(subscription_id));
    }

    let new_termination_time = decoded[0].clone().into_uint().unwrap();

    // Update the subscription job
    // Scope for write lock on subscription_jobs
    {
        let mut subscription_jobs = contracts_client.subscription_jobs.write().unwrap();
        let subscription_job = subscription_jobs.get_mut(&subscription_id).unwrap();
        subscription_job.termination_time = new_termination_time;
    }

    Ok(())
}
