use anyhow::{anyhow, Result};
use ethers::abi::FixedBytes;
use ethers::prelude::*;
use ethers::types::{Address, U256};
use ethers::utils::keccak256;
use k256::ecdsa::SigningKey;
use k256::elliptic_curve::generic_array::sequence::Lengthen;
use log::error;
use std::sync::Arc;
use tiny_keccak::{Hasher, Keccak};
use tokio::time;

use crate::constant::WAIT_BEFORE_CHECKING_NEXT_BLOCK;
use crate::HttpProvider;

pub async fn get_block_number_by_timestamp(
    provider: &Arc<HttpProvider>,
    target_timestamp: u64,
    from_block_number: u64,
) -> Option<u64> {
    let mut block_number: u64;
    if from_block_number == 0 {
        block_number = provider.get_block_number().await.unwrap().as_u64();
    } else {
        block_number = from_block_number;
    }
    'less_than_block_number: while block_number > 0 {
        let block = provider.get_block(block_number).await.unwrap().unwrap();
        // target_timestamp (the end bound of the interval) is excluded from the search
        if block.timestamp < U256::from(target_timestamp) {
            // Fetch the next block to confirm this is the latest block with timestamp < target_timestamp
            let next_block_number = block_number + 1;
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
                        block_number += 1;
                        continue 'less_than_block_number;
                    }
                    Ok(None) => {
                        // The next block does not exist.
                        // Wait for the next block to be created to be sure that
                        // the current block_number is the required block_number
                        time::sleep(time::Duration::from_secs(WAIT_BEFORE_CHECKING_NEXT_BLOCK))
                            .await;
                        continue 'next_block_check;
                    }
                    Err(_) => {
                        error!("Failed to fetch block number {}", next_block_number);
                        return None;
                    }
                }
            }
        }
        block_number -= 1;
    }
    None
}

pub fn pub_key_to_address(pub_key: &[u8]) -> Result<Address> {
    if pub_key.len() != 64 {
        return Err(anyhow!("Invalid public key length"));
    }

    let hash = keccak256(pub_key);
    let addr_bytes: [u8; 20] = hash[12..].try_into()?;
    Ok(Address::from_slice(&addr_bytes))
}

pub async fn get_key_for_job_id(job_id: U256, req_chain_id: u64) -> U256 {
    let req_chain_id = U256::from(req_chain_id);
    let hash = keccak256(format!("{}-{}", job_id, req_chain_id));
    U256::from_big_endian(&hash)
}

pub async fn sign_relay_job_response(
    signer_key: &SigningKey,
    job_id: U256,
    req_chain_id: U256,
    codehash: &FixedBytes,
    code_inputs: &Bytes,
    deadline: u64,
    job_owner: &Address,
    sequence_number: u8,
) -> Option<String> {
    let mut job_id_bytes = [0u8; 32];
    job_id.to_big_endian(&mut job_id_bytes);

    let mut req_chain_id_bytes = [0u8; 32];
    req_chain_id.to_big_endian(&mut req_chain_id_bytes);

    let mut hasher = Keccak::v256();
    hasher.update(b"|jobId|");
    hasher.update(&job_id_bytes);
    hasher.update(b"|chainId|");
    hasher.update(&req_chain_id_bytes);
    hasher.update(b"|codehash|");
    hasher.update(codehash);
    hasher.update(b"|codeInputs|");
    hasher.update(code_inputs);
    hasher.update(b"|deadline|");
    hasher.update(&deadline.to_be_bytes());
    hasher.update(b"|jobOwner|");
    hasher.update(job_owner.as_bytes());
    hasher.update(b"|retryNumber|");
    hasher.update(&sequence_number.to_be_bytes());

    let mut hash = [0u8; 32];
    hasher.finalize(&mut hash);

    let Ok((rs, v)) = signer_key.sign_prehash_recoverable(&hash).map_err(|err| {
        eprintln!("Failed to sign the response: {}", err);
        err
    }) else {
        return None;
    };

    Some(hex::encode(rs.to_bytes().append(27 + v.to_byte())))
}

pub async fn sign_reassign_gateway_relay_response(
    signer_key: &SigningKey,
    job_id: U256,
    gateway_operator_old: &Address,
) -> Option<String> {
    let mut job_id_bytes = [0u8; 32];
    job_id.to_big_endian(&mut job_id_bytes);

    let mut hasher = Keccak::v256();
    hasher.update(b"|jobId|");
    hasher.update(&job_id_bytes);
    hasher.update(b"|gatewayOperatorOld|");
    hasher.update(gateway_operator_old.as_bytes());

    let mut hash = [0u8; 32];
    hasher.finalize(&mut hash);

    let Ok((rs, v)) = signer_key.sign_prehash_recoverable(&hash).map_err(|err| {
        eprintln!("Failed to sign the response: {}", err);
        err
    }) else {
        return None;
    };

    Some(hex::encode(rs.to_bytes().append(27 + v.to_byte())))
}

pub async fn sign_job_response_response(
    signer_key: &SigningKey,
    job_id: U256,
    output: Bytes,
    total_time: U256,
    error_code: u8,
) -> Option<String> {
    let mut job_id_bytes = [0u8; 32];
    job_id.to_big_endian(&mut job_id_bytes);

    let mut total_time_bytes = [0u8; 32];
    total_time.to_big_endian(&mut total_time_bytes);

    let mut hasher = Keccak::v256();
    hasher.update(b"|jobId|");
    hasher.update(&job_id_bytes);
    hasher.update(b"|output|");
    hasher.update(&output);
    hasher.update(b"|totalTime|");
    hasher.update(&total_time_bytes);
    hasher.update(b"|errorCode|");
    hasher.update(&error_code.to_be_bytes());

    let mut hash = [0u8; 32];
    hasher.finalize(&mut hash);

    let Ok((rs, v)) = signer_key.sign_prehash_recoverable(&hash).map_err(|err| {
        eprintln!("Failed to sign the response: {}", err);
        err
    }) else {
        return None;
    };

    Some(hex::encode(rs.to_bytes().append(27 + v.to_byte())))
}
