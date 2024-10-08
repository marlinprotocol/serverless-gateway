use abi::encode;
use anyhow::Result;
use ethers::abi::{encode_packed, FixedBytes, Token};
use ethers::prelude::*;
use ethers::types::{Address, U256};
use ethers::utils::keccak256;
use futures_core::stream::Stream;
use k256::ecdsa::SigningKey;
use k256::elliptic_curve::generic_array::sequence::Lengthen;
use log::{error, info};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::time;

use crate::constant::{
    MAX_RETRY_ON_PROVIDER_ERROR, MAX_TX_RECEIPT_RETRIES, WAIT_BEFORE_CHECKING_BLOCK,
};
use crate::error::ServerlessError;
use crate::model::{Job, JobMode, RequestChainClient};

pub trait LogsProvider {
    fn common_chain_jobs<'a>(
        &'a self,
        common_chain_ws_client: &'a Provider<Ws>,
    ) -> impl Future<Output = Result<SubscriptionStream<'a, Ws, Log>>>;

    fn req_chain_jobs<'a>(
        &'a self,
        req_chain_ws_client: &'a Provider<Ws>,
        req_chain_client: &'a RequestChainClient,
    ) -> impl Future<Output = Result<impl Stream<Item = Log> + Unpin>>;

    fn gateways_job_relayed_logs<'a, P: HttpProviderLogs>(
        &'a self,
        job: Job,
        common_chain_http_provider: &'a P,
    ) -> impl Future<Output = Result<Vec<Log>>>;

    fn request_chain_historic_subscription_jobs<'a, P: HttpProviderLogs>(
        &'a self,
        req_chain_client: &'a RequestChainClient,
        req_chain_http_provider: &'a P,
    ) -> impl Future<Output = Result<Vec<Log>>>;
}

pub trait HttpProviderLogs {
    async fn get_logs(&self, filter: &Filter) -> Result<Vec<Log>, ServerlessError>;
}

pub struct HttpProvider {
    pub url: String,
}

impl HttpProvider {
    pub fn new(url: String) -> Self {
        Self { url }
    }
}

impl HttpProviderLogs for HttpProvider {
    async fn get_logs(&self, filter: &Filter) -> Result<Vec<Log>, ServerlessError> {
        let provider = Provider::<Http>::try_from(&self.url).unwrap();
        let logs = provider.get_logs(filter).await.unwrap();
        Ok(logs)
    }
}

pub async fn get_block_number_by_timestamp(
    provider: &Provider<Http>,
    target_timestamp: u64,
) -> Option<u64> {
    let mut block_number: u64 = 0;
    for _ in 0..5 {
        let get_block_number_result = provider.get_block_number().await;

        if get_block_number_result.is_err() {
            error!(
                "Failed to fetch block number. Error: {:#?}",
                get_block_number_result.err()
            );
            time::sleep(time::Duration::from_millis(WAIT_BEFORE_CHECKING_BLOCK)).await;
            continue;
        }

        block_number = get_block_number_result.unwrap().as_u64();
        break;
    }

    if block_number == 0 {
        error!("Failed to fetch block number");
        return None;
    }

    // A conservative estimate of the block rate per second before it is actually calculated below.
    let mut block_rate_per_second: f64 = 3.0;
    let mut first_block_number = 0;
    let mut first_block_timestamp = 0;
    let mut earliest_block_number_after_target_ts = u64::MAX;

    'less_than_block_number: while block_number > 0 {
        let block = provider.get_block(block_number).await;
        if block.is_err() {
            error!(
                "Failed to fetch block number {}. Error: {:#?}",
                block_number,
                block.err()
            );
            continue;
        }
        let block = block.unwrap();
        if block.is_none() {
            continue;
        }
        let block = block.unwrap();

        // target_timestamp (the end bound of the interval) is excluded from the search
        if block.timestamp < U256::from(target_timestamp) {
            // Fetch the next block to confirm this is the latest block with timestamp < target_timestamp
            let next_block_number = block_number + 1;

            let mut retry_on_error = 0;
            'next_block_check: loop {
                let next_block_result = provider.get_block(next_block_number).await;

                match next_block_result {
                    Ok(Some(block)) => {
                        // next_block exists
                        if block.timestamp >= U256::from(target_timestamp) {
                            // The next block's timestamp is greater than or equal to the target timestamp,
                            // so return the current block number
                            return Some(block_number);
                        }
                        block_number = block_number
                            + ((target_timestamp - block.timestamp.as_u64()) as f64
                                * block_rate_per_second) as u64;

                        if block_number >= earliest_block_number_after_target_ts {
                            block_number = earliest_block_number_after_target_ts - 1;
                            earliest_block_number_after_target_ts -= 1;
                        }
                        continue 'less_than_block_number;
                    }
                    Ok(None) => {
                        // The next block does not exist.
                        // Wait for the next block to be created to be sure that
                        // the current block_number is the required block_number
                        time::sleep(time::Duration::from_millis(WAIT_BEFORE_CHECKING_BLOCK)).await;
                        continue 'next_block_check;
                    }
                    Err(err) => {
                        error!(
                            "Failed to fetch block number {}. Err: {}",
                            next_block_number, err
                        );
                        retry_on_error += 1;
                        if retry_on_error <= MAX_RETRY_ON_PROVIDER_ERROR {
                            continue 'next_block_check;
                        }
                        return None;
                    }
                }
            }
        } else {
            if block_number < earliest_block_number_after_target_ts {
                earliest_block_number_after_target_ts = block_number;
            }

            if first_block_timestamp == 0 {
                first_block_timestamp = block.timestamp.as_u64();
                first_block_number = block_number;
            }
            // Calculate the avg block rate per second using the first recorded block timestamp
            if first_block_timestamp > block.timestamp.as_u64() + 1 {
                block_rate_per_second = (first_block_number - block_number) as f64
                    / (first_block_timestamp - block.timestamp.as_u64()) as f64;
                info!("Block rate per second: {}", block_rate_per_second);

                let block_go_back = ((block.timestamp.as_u64() - target_timestamp) as f64
                    * block_rate_per_second) as u64;
                if block_go_back != 0 {
                    if block_number >= block_go_back {
                        block_number = block_number - block_go_back + 1;
                    } else {
                        block_number = 1;
                    }
                }
            }
        }
        block_number -= 1;
    }
    None
}

pub async fn sign_relay_job_request(
    signer_key: &SigningKey,
    job_id: U256,
    codehash: &FixedBytes,
    code_inputs: &Bytes,
    user_timeout: U256,
    job_start_time: U256,
    sequence_number: u8,
    job_owner: &Address,
    env: u8,
) -> Option<(String, u64)> {
    let sign_timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let relay_job_typehash = keccak256(
            "RelayJob(uint256 jobId,bytes32 codeHash,bytes codeInputs,uint256 deadline,uint256 jobRequestTimestamp,uint8 sequenceId,address jobOwner,uint8 env,uint256 signTimestamp)"
        );

    let code_inputs_hash = keccak256(code_inputs);

    let token_list = [
        Token::FixedBytes(relay_job_typehash.to_vec()),
        Token::Uint(job_id),
        Token::FixedBytes(codehash.clone()),
        Token::FixedBytes(code_inputs_hash.to_vec()),
        Token::Uint(user_timeout),
        Token::Uint(job_start_time),
        Token::Uint(sequence_number.into()),
        Token::Address(*job_owner),
        Token::Uint(env.into()),
        Token::Uint(sign_timestamp.into()),
    ];

    let hash_struct = keccak256(encode(&token_list));

    let gateway_jobs_domain_separator = keccak256(encode(&[
        Token::FixedBytes(keccak256("EIP712Domain(string name,string version)").to_vec()),
        Token::FixedBytes(keccak256("marlin.oyster.GatewayJobs").to_vec()),
        Token::FixedBytes(keccak256("1").to_vec()),
    ]));

    let digest = encode_packed(&[
        Token::String("\x19\x01".to_string()),
        Token::FixedBytes(gateway_jobs_domain_separator.to_vec()),
        Token::FixedBytes(hash_struct.to_vec()),
    ]);

    let Ok(digest) = digest else {
        eprintln!("Failed to encode the digest: {:#?}", digest.err());
        return None;
    };
    let digest = keccak256(digest);

    // Sign the digest using enclave key
    let sig = signer_key.sign_prehash_recoverable(&digest);
    let Ok((rs, v)) = sig else {
        eprintln!("Failed to sign the digest: {:#?}", sig.err());
        return None;
    };

    Some((
        hex::encode(rs.to_bytes().append(27 + v.to_byte()).to_vec()),
        sign_timestamp,
    ))
}

pub async fn sign_reassign_gateway_relay_request(
    signer_key: &SigningKey,
    job_id: U256,
    gateway_operator_old: &Address,
    job_owner: &Address,
    sequence_number: u8,
    job_start_time: U256,
) -> Option<(String, u64)> {
    let sign_timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let reassign_gateway_relay_typehash = keccak256(
            "ReassignGateway(uint256 jobId,address gatewayOld,address jobOwner,uint8 sequenceId,uint256 jobRequestTimestamp,uint256 signTimestamp)"
        );

    let token_list = [
        Token::FixedBytes(reassign_gateway_relay_typehash.to_vec()),
        Token::Uint(job_id),
        Token::Address(*gateway_operator_old),
        Token::Address(*job_owner),
        Token::Uint(sequence_number.into()),
        Token::Uint(job_start_time),
        Token::Uint(sign_timestamp.into()),
    ];

    let hash_struct = keccak256(encode(&token_list));

    let gateway_jobs_domain_separator = keccak256(encode(&[
        Token::FixedBytes(keccak256("EIP712Domain(string name,string version)").to_vec()),
        Token::FixedBytes(keccak256("marlin.oyster.GatewayJobs").to_vec()),
        Token::FixedBytes(keccak256("1").to_vec()),
    ]));

    let digest = encode_packed(&[
        Token::String("\x19\x01".to_string()),
        Token::FixedBytes(gateway_jobs_domain_separator.to_vec()),
        Token::FixedBytes(hash_struct.to_vec()),
    ]);

    let Ok(digest) = digest else {
        eprintln!("Failed to encode the digest: {:#?}", digest.err());
        return None;
    };
    let digest = keccak256(digest);

    // Sign the digest using enclave key
    let sig = signer_key.sign_prehash_recoverable(&digest);
    let Ok((rs, v)) = sig else {
        eprintln!("Failed to sign the digest: {:#?}", sig.err());
        return None;
    };

    Some((
        hex::encode(rs.to_bytes().append(27 + v.to_byte()).to_vec()),
        sign_timestamp,
    ))
}

pub async fn sign_job_response_request(
    signer_key: &SigningKey,
    job_id: U256,
    output: Bytes,
    total_time: U256,
    error_code: u8,
    job_mode: JobMode,
) -> Option<(String, u64)> {
    let sign_timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let job_response_typehash = keccak256(
        "JobResponse(uint256 jobId,bytes output,uint256 totalTime,uint8 errorCode,uint256 signTimestamp)"
    );

    let output_hash = keccak256(output);

    let token_list = [
        Token::FixedBytes(job_response_typehash.to_vec()),
        Token::Uint(job_id),
        Token::FixedBytes(output_hash.to_vec()),
        Token::Uint(total_time),
        Token::Uint(error_code.into()),
        Token::Uint(sign_timestamp.into()),
    ];

    let hash_struct = keccak256(encode(&token_list));

    let contract_name;
    if job_mode == JobMode::Once {
        contract_name = "marlin.oyster.Relay";
    } else {
        contract_name = "marlin.oyster.RelaySubscriptions";
    }

    let gateway_jobs_domain_separator = keccak256(encode(&[
        Token::FixedBytes(keccak256("EIP712Domain(string name,string version)").to_vec()),
        Token::FixedBytes(keccak256(contract_name).to_vec()),
        Token::FixedBytes(keccak256("1").to_vec()),
    ]));

    let digest = encode_packed(&[
        Token::String("\x19\x01".to_string()),
        Token::FixedBytes(gateway_jobs_domain_separator.to_vec()),
        Token::FixedBytes(hash_struct.to_vec()),
    ]);

    let Ok(digest) = digest else {
        eprintln!("Failed to encode the digest: {:#?}", digest.err());
        return None;
    };
    let digest = keccak256(digest);

    // Sign the digest using enclave key
    let sig = signer_key.sign_prehash_recoverable(&digest);
    let Ok((rs, v)) = sig else {
        eprintln!("Failed to sign the digest: {:#?}", sig.err());
        return None;
    };

    Some((
        hex::encode(rs.to_bytes().append(27 + v.to_byte()).to_vec()),
        sign_timestamp,
    ))
}

pub async fn confirm_event(
    mut log: Log,
    http_rpc_url: &String,
    confirmation_blocks: u64,
    last_seen_block: Arc<AtomicU64>,
) -> Log {
    let provider: Provider<Http> = Provider::<Http>::try_from(http_rpc_url).unwrap();

    let log_transaction_hash = log.transaction_hash.unwrap_or(H256::zero());
    // Verify transaction hash is of valid length and not 0
    if log_transaction_hash == H256::zero() {
        log.removed = Some(true);
        return log;
    }

    let mut retries = 0;
    let mut first_iteration = true;
    loop {
        if last_seen_block.load(Ordering::SeqCst)
            >= log.block_number.unwrap_or(U64::zero()).as_u64() + confirmation_blocks
        {
            match provider
                .get_transaction_receipt(log.transaction_hash.unwrap_or(H256::zero()))
                .await
            {
                Ok(Some(_)) => {
                    info!("Event Confirmed");
                    break;
                }
                Ok(None) => {
                    info!("Event reverted due to re-org");
                    log.removed = Some(true);
                    break;
                }
                Err(err) => {
                    error!("Failed to fetch transaction receipt. Error: {:#?}", err);
                    retries += 1;
                    if retries >= MAX_TX_RECEIPT_RETRIES {
                        error!("Max retries reached. Exiting");
                        log.removed = Some(true);
                        break;
                    }
                    time::sleep(time::Duration::from_millis(WAIT_BEFORE_CHECKING_BLOCK)).await;
                    continue;
                }
            };
        }

        if first_iteration {
            first_iteration = false;
        } else {
            time::sleep(time::Duration::from_millis(WAIT_BEFORE_CHECKING_BLOCK)).await;
        }

        let curr_block_number = match provider.get_block_number().await {
            Ok(block_number) => block_number,
            Err(err) => {
                error!("Failed to fetch block number. Error: {:#?}", err);
                time::sleep(time::Duration::from_millis(WAIT_BEFORE_CHECKING_BLOCK)).await;
                continue;
            }
        };
        last_seen_block.store(curr_block_number.as_u64(), Ordering::SeqCst);
    }
    log
}
