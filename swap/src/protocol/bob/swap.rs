use crate::bitcoin::ExpiredTimelocks;
use crate::database::{Database, Swap};
use crate::env::Config;
use crate::protocol::bob;
use crate::protocol::bob::event_loop::EventLoopHandle;
use crate::protocol::bob::state::*;
use crate::{bitcoin, monero};
use anyhow::{bail, Context, Result};
use async_recursion::async_recursion;
use rand::rngs::OsRng;
use std::sync::Arc;
use tokio::select;
use tracing::trace;
use uuid::Uuid;

pub fn is_complete(state: &BobState) -> bool {
    matches!(
        state,
        BobState::BtcRefunded(..)
            | BobState::XmrRedeemed { .. }
            | BobState::BtcPunished { .. }
            | BobState::SafelyAborted
    )
}

#[allow(clippy::too_many_arguments)]
pub async fn run(swap: bob::Swap) -> Result<BobState> {
    run_until(swap, is_complete).await
}

pub async fn run_until(
    swap: bob::Swap,
    is_target_state: fn(&BobState) -> bool,
) -> Result<BobState> {
    run_until_internal(
        swap.state,
        is_target_state,
        swap.event_loop_handle,
        swap.db,
        swap.bitcoin_wallet,
        swap.monero_wallet,
        swap.swap_id,
        swap.env_config,
        swap.receive_monero_address,
    )
    .await
}

// State machine driver for swap execution
#[allow(clippy::too_many_arguments)]
#[async_recursion]
async fn run_until_internal(
    state: BobState,
    is_target_state: fn(&BobState) -> bool,
    mut event_loop_handle: EventLoopHandle,
    db: Database,
    bitcoin_wallet: Arc<bitcoin::Wallet>,
    monero_wallet: Arc<monero::Wallet>,
    swap_id: Uuid,
    env_config: Config,
    receive_monero_address: monero::Address,
) -> Result<BobState> {
    trace!("Current state: {}", state);
    if is_target_state(&state) {
        return Ok(state);
    }

    let new_state = match state {
        BobState::Started { btc_amount } => {
            let bitcoin_refund_address = bitcoin_wallet.new_address().await?;

            event_loop_handle.dial().await?;

            let state2 = request_price_and_setup(
                btc_amount,
                &mut event_loop_handle,
                env_config,
                bitcoin_refund_address,
            )
            .await?;

            BobState::ExecutionSetupDone(state2)
        }
        BobState::ExecutionSetupDone(state2) => {
            // Do not lock Bitcoin if not connected to Alice.
            event_loop_handle.dial().await?;
            // Alice and Bob have exchanged info
            let (state3, tx_lock) = state2.lock_btc().await?;
            let signed_tx = bitcoin_wallet
                .sign_and_finalize(tx_lock.clone().into())
                .await
                .context("Failed to sign Bitcoin lock transaction")?;
            let (..) = bitcoin_wallet.broadcast(signed_tx, "lock").await?;

            BobState::BtcLocked(state3)
        }
        // Bob has locked Btc
        // Watch for Alice to Lock Xmr or for cancel timelock to elapse
        BobState::BtcLocked(state3) => {
            if let ExpiredTimelocks::None = state3.current_epoch(bitcoin_wallet.as_ref()).await? {
                event_loop_handle.dial().await?;

                let transfer_proof_watcher = event_loop_handle.recv_transfer_proof();
                let cancel_timelock_expires =
                    state3.wait_for_cancel_timelock_to_expire(bitcoin_wallet.as_ref());

                // Record the current monero wallet block height so we don't have to scan from
                // block 0 once we create the redeem wallet.
                let monero_wallet_restore_blockheight = monero_wallet.block_height().await?;

                tracing::info!("Waiting for Alice to lock Monero");

                select! {
                    transfer_proof = transfer_proof_watcher => {
                        let transfer_proof = transfer_proof?.tx_lock_proof;

                        tracing::info!(txid = %transfer_proof.tx_hash(), "Alice locked Monero");

                        BobState::XmrLockProofReceived {
                            state: state3,
                            lock_transfer_proof: transfer_proof,
                            monero_wallet_restore_blockheight
                        }
                    },
                    _ = cancel_timelock_expires => {
                        tracing::info!("Alice took too long to lock Monero, cancelling the swap");

                        let state4 = state3.cancel();
                        BobState::CancelTimelockExpired(state4)
                    }
                }
            } else {
                let state4 = state3.cancel();
                BobState::CancelTimelockExpired(state4)
            }
        }
        BobState::XmrLockProofReceived {
            state,
            lock_transfer_proof,
            monero_wallet_restore_blockheight,
        } => {
            if let ExpiredTimelocks::None = state.current_epoch(bitcoin_wallet.as_ref()).await? {
                event_loop_handle.dial().await?;

                let watch_request = state.lock_xmr_watch_request(lock_transfer_proof);

                select! {
                    received_xmr = monero_wallet.watch_for_transfer(watch_request) => {
                        match received_xmr {
                            Ok(()) => BobState::XmrLocked(state.xmr_locked(monero_wallet_restore_blockheight)),
                            Err(e) => {
                                 tracing::warn!("Waiting for refund because insufficient Monero have been locked! {}", e);
                                 state.wait_for_cancel_timelock_to_expire(bitcoin_wallet.as_ref()).await?;

                                 BobState::CancelTimelockExpired(state.cancel())
                            },
                        }
                    }
                    _ = state.wait_for_cancel_timelock_to_expire(bitcoin_wallet.as_ref()) => {
                        BobState::CancelTimelockExpired(state.cancel())
                    }
                }
            } else {
                BobState::CancelTimelockExpired(state.cancel())
            }
        }
        BobState::XmrLocked(state) => {
            if let ExpiredTimelocks::None = state.expired_timelock(bitcoin_wallet.as_ref()).await? {
                event_loop_handle.dial().await?;
                // Alice has locked Xmr
                // Bob sends Alice his key

                select! {
                    _ = event_loop_handle.send_encrypted_signature(state.tx_redeem_encsig()) => {
                        BobState::EncSigSent(state)
                    },
                    _ = state.wait_for_cancel_timelock_to_expire(bitcoin_wallet.as_ref()) => {
                        BobState::CancelTimelockExpired(state.cancel())
                    }
                }
            } else {
                BobState::CancelTimelockExpired(state.cancel())
            }
        }
        BobState::EncSigSent(state) => {
            if let ExpiredTimelocks::None = state.expired_timelock(bitcoin_wallet.as_ref()).await? {
                select! {
                    state5 = state.watch_for_redeem_btc(bitcoin_wallet.as_ref()) => {
                        BobState::BtcRedeemed(state5?)
                    },
                    _ = state.wait_for_cancel_timelock_to_expire(bitcoin_wallet.as_ref()) => {
                        BobState::CancelTimelockExpired(state.cancel())
                    }
                }
            } else {
                BobState::CancelTimelockExpired(state.cancel())
            }
        }
        BobState::BtcRedeemed(state) => {
            // Bob redeems XMR using revealed s_a
            state.claim_xmr(monero_wallet.as_ref()).await?;

            // Ensure that the generated wallet is synced so we have a proper balance
            monero_wallet.refresh().await?;
            // Sweep (transfer all funds) to the given address
            let tx_hashes = monero_wallet.sweep_all(receive_monero_address).await?;

            for tx_hash in tx_hashes {
                tracing::info!("Sent XMR to {} in tx {}", receive_monero_address, tx_hash.0);
            }

            BobState::XmrRedeemed {
                tx_lock_id: state.tx_lock_id(),
            }
        }
        BobState::CancelTimelockExpired(state4) => {
            if state4
                .check_for_tx_cancel(bitcoin_wallet.as_ref())
                .await
                .is_err()
            {
                state4.submit_tx_cancel(bitcoin_wallet.as_ref()).await?;
            }

            BobState::BtcCancelled(state4)
        }
        BobState::BtcCancelled(state) => {
            // Bob has cancelled the swap
            match state.expired_timelock(bitcoin_wallet.as_ref()).await? {
                ExpiredTimelocks::None => {
                    bail!(
                        "Internal error: canceled state reached before cancel timelock was expired"
                    );
                }
                ExpiredTimelocks::Cancel => {
                    state.refund_btc(bitcoin_wallet.as_ref()).await?;
                    BobState::BtcRefunded(state)
                }
                ExpiredTimelocks::Punish => BobState::BtcPunished {
                    tx_lock_id: state.tx_lock_id(),
                },
            }
        }
        BobState::BtcRefunded(state4) => BobState::BtcRefunded(state4),
        BobState::BtcPunished { tx_lock_id } => BobState::BtcPunished { tx_lock_id },
        BobState::SafelyAborted => BobState::SafelyAborted,
        BobState::XmrRedeemed { tx_lock_id } => BobState::XmrRedeemed { tx_lock_id },
    };

    let db_state = new_state.clone().into();
    db.insert_latest_state(swap_id, Swap::Bob(db_state)).await?;
    run_until_internal(
        new_state,
        is_target_state,
        event_loop_handle,
        db,
        bitcoin_wallet,
        monero_wallet,
        swap_id,
        env_config,
        receive_monero_address,
    )
    .await
}

pub async fn request_price_and_setup(
    btc: bitcoin::Amount,
    event_loop_handle: &mut EventLoopHandle,
    env_config: Config,
    bitcoin_refund_address: bitcoin::Address,
) -> Result<bob::state::State2> {
    let xmr = event_loop_handle.request_spot_price(btc).await?;

    tracing::info!("Spot price for {} is {}", btc, xmr);

    let state0 = State0::new(
        &mut OsRng,
        btc,
        xmr,
        env_config.bitcoin_cancel_timelock,
        env_config.bitcoin_punish_timelock,
        bitcoin_refund_address,
        env_config.monero_finality_confirmations,
    );

    let state2 = event_loop_handle.execution_setup(state0).await?;

    Ok(state2)
}
