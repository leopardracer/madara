use crate::errors::StarknetRpcApiError;
use crate::errors::StarknetRpcResult;
use crate::utils::tx_api_to_blockifier;
use crate::utils::ResultExt;
use crate::versions::user::v0_7_1::methods::trace::trace_transaction::EXECUTION_UNSUPPORTED_BELOW_VERSION;
use crate::Starknet;
use blockifier::transaction::account_transaction::ExecutionFlags;
use mc_exec::ExecutionContext;
use mp_block::BlockId;
use mp_rpc::{BroadcastedTxn, FeeEstimate, SimulationFlagForEstimateFee};
use mp_transactions::{IntoStarknetApiExt, ToBlockifierError};
use std::sync::Arc;

/// Estimate the fee associated with transaction
///
/// # Arguments
///
/// * `request` - starknet transaction request
/// * `block_id` - hash of the requested block, number (height), or tag
///
/// # Returns
///
/// * `fee_estimate` - fee estimate in gwei
pub async fn estimate_fee(
    starknet: &Starknet,
    request: Vec<BroadcastedTxn>,
    simulation_flags: Vec<SimulationFlagForEstimateFee>,
    block_id: BlockId,
) -> StarknetRpcResult<Vec<FeeEstimate>> {
    tracing::debug!("estimate fee on block_id {block_id:?}");
    let block_info = starknet.get_block_info(&block_id)?;
    let starknet_version = *block_info.protocol_version();

    if starknet_version < EXECUTION_UNSUPPORTED_BELOW_VERSION {
        return Err(StarknetRpcApiError::unsupported_txn_version());
    }

    let exec_context = ExecutionContext::new_at_block_end(Arc::clone(&starknet.backend), &block_info)?;
    let validate = !simulation_flags.contains(&SimulationFlagForEstimateFee::SkipValidate);

    let transactions = request
        .into_iter()
        .map(|tx| {
            let only_query = tx.is_query();
            let (api_tx, _) = tx.into_starknet_api(starknet.chain_id(), starknet_version)?;
            let execution_flags = ExecutionFlags { only_query, charge_fee: false, validate, strict_nonce_check: true };
            Ok(tx_api_to_blockifier(api_tx, execution_flags)?)
        })
        .collect::<Result<Vec<_>, ToBlockifierError>>()
        .or_internal_server_error("Failed to convert BroadcastedTransaction to AccountTransaction")?;

    let execution_results = exec_context.re_execute_transactions([], transactions)?;

    let fee_estimates = execution_results.iter().enumerate().try_fold(
        Vec::with_capacity(execution_results.len()),
        |mut acc, (index, result)| {
            if result.execution_info.is_reverted() {
                return Err(StarknetRpcApiError::TxnExecutionError {
                    tx_index: index,
                    error: result.execution_info.revert_error.as_ref().map(|e| e.to_string()).unwrap_or_default(),
                });
            }
            acc.push(exec_context.execution_result_to_fee_estimate(result));
            Ok(acc)
        },
    )?;

    Ok(fee_estimates)
}
