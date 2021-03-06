use crate::bitcoin::timelocks::BlockHeight;
use crate::bitcoin::{Address, Amount, Transaction};
use crate::env;
use ::bitcoin::util::psbt::PartiallySignedTransaction;
use ::bitcoin::Txid;
use anyhow::{anyhow, bail, Context, Result};
use bdk::blockchain::{noop_progress, Blockchain, ElectrumBlockchain};
use bdk::descriptor::Segwitv0;
use bdk::electrum_client::{self, ElectrumApi, GetHistoryRes};
use bdk::keys::DerivableKey;
use bdk::{FeeRate, KeychainKind};
use bitcoin::Script;
use reqwest::Url;
use std::collections::BTreeMap;
use std::convert::TryFrom;
use std::fmt;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const SLED_TREE_NAME: &str = "default_tree";

pub struct Wallet {
    client: Arc<Mutex<Client>>,
    wallet: Arc<Mutex<bdk::Wallet<ElectrumBlockchain, bdk::sled::Tree>>>,
    finality_confirmations: u32,
}

impl Wallet {
    pub async fn new(
        electrum_rpc_url: Url,
        wallet_dir: &Path,
        key: impl DerivableKey<Segwitv0> + Clone,
        env_config: env::Config,
    ) -> Result<Self> {
        // Workaround for https://github.com/bitcoindevkit/rust-electrum-client/issues/47.
        let config = electrum_client::ConfigBuilder::default().retry(2).build();

        let client =
            bdk::electrum_client::Client::from_config(electrum_rpc_url.as_str(), config.clone())
                .map_err(|e| anyhow!("Failed to init electrum rpc client: {:?}", e))?;

        let db = bdk::sled::open(wallet_dir)?.open_tree(SLED_TREE_NAME)?;

        let bdk_wallet = bdk::Wallet::new(
            bdk::template::BIP84(key.clone(), KeychainKind::External),
            Some(bdk::template::BIP84(key, KeychainKind::Internal)),
            env_config.bitcoin_network,
            db,
            ElectrumBlockchain::from(client),
        )?;

        let electrum = bdk::electrum_client::Client::from_config(electrum_rpc_url.as_str(), config)
            .map_err(|e| anyhow!("Failed to init electrum rpc client {:?}", e))?;

        Ok(Self {
            wallet: Arc::new(Mutex::new(bdk_wallet)),
            client: Arc::new(Mutex::new(Client::new(
                electrum,
                env_config.bitcoin_sync_interval(),
            )?)),
            finality_confirmations: env_config.bitcoin_finality_confirmations,
        })
    }

    pub async fn balance(&self) -> Result<Amount> {
        let balance = self
            .wallet
            .lock()
            .await
            .get_balance()
            .context("Failed to calculate Bitcoin balance")?;

        Ok(Amount::from_sat(balance))
    }

    pub async fn new_address(&self) -> Result<Address> {
        let address = self
            .wallet
            .lock()
            .await
            .get_new_address()
            .context("Failed to get new Bitcoin address")?;

        Ok(address)
    }

    pub async fn get_tx(&self, txid: Txid) -> Result<Option<Transaction>> {
        let tx = self.wallet.lock().await.client().get_tx(&txid)?;

        Ok(tx)
    }

    pub async fn transaction_fee(&self, txid: Txid) -> Result<Amount> {
        let fees = self
            .wallet
            .lock()
            .await
            .list_transactions(true)?
            .iter()
            .find(|tx| tx.txid == txid)
            .ok_or_else(|| {
                anyhow!("Could not find tx in bdk wallet when trying to determine fees")
            })?
            .fees;

        Ok(Amount::from_sat(fees))
    }

    pub async fn sync(&self) -> Result<()> {
        self.wallet
            .lock()
            .await
            .sync(noop_progress(), None)
            .context("Failed to sync balance of Bitcoin wallet")?;

        Ok(())
    }

    pub async fn send_to_address(
        &self,
        address: Address,
        amount: Amount,
    ) -> Result<PartiallySignedTransaction> {
        let wallet = self.wallet.lock().await;

        let mut tx_builder = wallet.build_tx();
        tx_builder.add_recipient(address.script_pubkey(), amount.as_sat());
        tx_builder.fee_rate(self.select_feerate());
        let (psbt, _details) = tx_builder.finish()?;

        Ok(psbt)
    }

    /// Calculates the maximum "giveable" amount of this wallet.
    ///
    /// We define this as the maximum amount we can pay to a single output,
    /// already accounting for the fees we need to spend to get the
    /// transaction confirmed.
    pub async fn max_giveable(&self, locking_script_size: usize) -> Result<Amount> {
        let wallet = self.wallet.lock().await;

        let mut tx_builder = wallet.build_tx();

        let dummy_script = Script::from(vec![0u8; locking_script_size]);
        tx_builder.set_single_recipient(dummy_script);
        tx_builder.drain_wallet();
        tx_builder.fee_rate(self.select_feerate());
        let (_, details) = tx_builder.finish().context("Failed to build transaction")?;

        let max_giveable = details.sent - details.fees;

        Ok(Amount::from_sat(max_giveable))
    }

    pub async fn get_network(&self) -> bitcoin::Network {
        self.wallet.lock().await.network()
    }

    /// Broadcast the given transaction to the network and emit a log statement
    /// if done so successfully.
    ///
    /// Returns the transaction ID and a future for when the transaction meets
    /// the configured finality confirmations.
    pub async fn broadcast(
        &self,
        transaction: Transaction,
        kind: &str,
    ) -> Result<(Txid, impl Future<Output = Result<()>> + '_)> {
        let txid = transaction.txid();

        // to watch for confirmations, watching a single output is enough
        let watcher = self.wait_for_transaction_finality(
            (txid, transaction.output[0].script_pubkey.clone()),
            kind.to_owned(),
        );

        self.wallet
            .lock()
            .await
            .broadcast(transaction)
            .with_context(|| {
                format!("Failed to broadcast Bitcoin {} transaction {}", kind, txid)
            })?;

        tracing::info!(%txid, "Published Bitcoin {} transaction", kind);

        Ok((txid, watcher))
    }

    pub async fn sign_and_finalize(&self, psbt: PartiallySignedTransaction) -> Result<Transaction> {
        let (signed_psbt, finalized) = self.wallet.lock().await.sign(psbt, None)?;

        if !finalized {
            bail!("PSBT is not finalized")
        }

        let tx = signed_psbt.extract_tx();

        Ok(tx)
    }

    pub async fn get_raw_transaction(&self, txid: Txid) -> Result<Transaction> {
        self.get_tx(txid)
            .await?
            .ok_or_else(|| anyhow!("Could not get raw tx with id: {}", txid))
    }

    pub async fn status_of_script<T>(&self, tx: &T) -> Result<ScriptStatus>
    where
        T: Watchable,
    {
        self.client.lock().await.status_of_script(tx)
    }

    pub async fn watch_until_status<T>(
        &self,
        tx: &T,
        mut status_fn: impl FnMut(ScriptStatus) -> bool,
    ) -> Result<()>
    where
        T: Watchable,
    {
        let txid = tx.id();

        let mut last_status = None;

        loop {
            let new_status = self.client.lock().await.status_of_script(tx)?;

            if Some(new_status) != last_status {
                tracing::debug!(%txid, "Transaction is {}", new_status);
            }
            last_status = Some(new_status);

            if status_fn(new_status) {
                break;
            }

            tokio::time::sleep(Duration::from_secs(5)).await;
        }

        Ok(())
    }

    async fn wait_for_transaction_finality<T>(&self, tx: T, kind: String) -> Result<()>
    where
        T: Watchable,
    {
        let conf_target = self.finality_confirmations;
        let txid = tx.id();

        tracing::info!(%txid, "Waiting for {} confirmation{} of Bitcoin {} transaction", conf_target, if conf_target > 1 { "s" } else { "" }, kind);

        let mut seen_confirmations = 0;

        self.watch_until_status(&tx, |status| match status {
            ScriptStatus::Confirmed(inner) => {
                let confirmations = inner.confirmations();

                if confirmations > seen_confirmations {
                    tracing::info!(%txid, "Bitcoin {} tx has {} out of {} confirmation{}", kind, confirmations, conf_target, if conf_target > 1 { "s" } else { "" });
                    seen_confirmations = confirmations;
                }

                inner.meets_target(conf_target)
            },
            _ => false
        })
        .await?;

        Ok(())
    }

    /// Selects an appropriate [`FeeRate`] to be used for getting transactions
    /// confirmed within a reasonable amount of time.
    fn select_feerate(&self) -> FeeRate {
        // TODO: This should obviously not be a const :)
        FeeRate::from_sat_per_vb(5.0)
    }
}

/// Defines a watchable transaction.
///
/// For a transaction to be watchable, we need to know two things: Its
/// transaction ID and the specific output script that is going to change.
/// A transaction can obviously have multiple outputs but our protocol purposes,
/// we are usually interested in a specific one.
pub trait Watchable {
    fn id(&self) -> Txid;
    fn script(&self) -> Script;
}

impl Watchable for (Txid, Script) {
    fn id(&self) -> Txid {
        self.0
    }

    fn script(&self) -> Script {
        self.1.clone()
    }
}

struct Client {
    electrum: bdk::electrum_client::Client,
    latest_block: BlockHeight,
    last_ping: Instant,
    interval: Duration,
    script_history: BTreeMap<Script, Vec<GetHistoryRes>>,
}

impl Client {
    fn new(electrum: bdk::electrum_client::Client, interval: Duration) -> Result<Self> {
        let latest_block = electrum.block_headers_subscribe().map_err(|e| {
            anyhow!(
                "Electrum client failed to subscribe to header notifications: {:?}",
                e
            )
        })?;

        Ok(Self {
            electrum,
            latest_block: BlockHeight::try_from(latest_block)?,
            last_ping: Instant::now(),
            interval,
            script_history: Default::default(),
        })
    }

    /// Ping the electrum server unless we already did within the set interval.
    ///
    /// Returns a boolean indicating whether we actually pinged the server.
    fn ping(&mut self) -> bool {
        if self.last_ping.elapsed() <= self.interval {
            return false;
        }

        match self.electrum.ping() {
            Ok(()) => {
                self.last_ping = Instant::now();

                true
            }
            Err(error) => {
                tracing::debug!(?error, "Failed to ping electrum server");

                false
            }
        }
    }

    fn drain_notifications(&mut self) -> Result<()> {
        let pinged = self.ping();

        if !pinged {
            return Ok(());
        }

        self.drain_blockheight_notifications()?;
        self.update_script_histories()?;

        Ok(())
    }

    fn status_of_script<T>(&mut self, tx: &T) -> Result<ScriptStatus>
    where
        T: Watchable,
    {
        let txid = tx.id();
        let script = tx.script();

        if !self.script_history.contains_key(&script) {
            self.script_history.insert(script.clone(), vec![]);
        }

        self.drain_notifications()?;

        let history = self.script_history.entry(script).or_default();

        let history_of_tx = history
            .iter()
            .filter(|entry| entry.tx_hash == txid)
            .collect::<Vec<_>>();

        match history_of_tx.as_slice() {
            [] => Ok(ScriptStatus::Unseen),
            [remaining @ .., last] => {
                if !remaining.is_empty() {
                    tracing::warn!("Found more than a single history entry for script. This is highly unexpected and those history entries will be ignored.")
                }

                if last.height <= 0 {
                    Ok(ScriptStatus::InMempool)
                } else {
                    Ok(ScriptStatus::Confirmed(
                        Confirmed::from_inclusion_and_latest_block(
                            u32::try_from(last.height)?,
                            u32::from(self.latest_block),
                        ),
                    ))
                }
            }
        }
    }

    fn drain_blockheight_notifications(&mut self) -> Result<()> {
        let latest_block = std::iter::from_fn(|| self.electrum.block_headers_pop().transpose())
            .last()
            .transpose()
            .map_err(|e| anyhow!("Failed to pop header notification: {:?}", e))?;

        if let Some(new_block) = latest_block {
            tracing::debug!(
                "Got notification for new block at height {}",
                new_block.height
            );
            self.latest_block = BlockHeight::try_from(new_block)?;
        }

        Ok(())
    }

    fn update_script_histories(&mut self) -> Result<()> {
        let histories = self
            .electrum
            .batch_script_get_history(self.script_history.keys())
            .map_err(|e| anyhow!("Failed to get script histories {:?}", e))?;

        if histories.len() != self.script_history.len() {
            bail!(
                "Expected {} history entries, received {}",
                self.script_history.len(),
                histories.len()
            );
        }

        let scripts = self.script_history.keys().cloned();
        let histories = histories.into_iter();

        self.script_history = scripts.zip(histories).collect::<BTreeMap<_, _>>();

        Ok(())
    }
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum ScriptStatus {
    Unseen,
    InMempool,
    Confirmed(Confirmed),
}

impl ScriptStatus {
    pub fn from_confirmations(confirmations: u32) -> Self {
        match confirmations {
            0 => Self::InMempool,
            confirmations => Self::Confirmed(Confirmed::new(confirmations - 1)),
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct Confirmed {
    /// The depth of this transaction within the blockchain.
    ///
    /// Will be zero if the transaction is included in the latest block.
    depth: u32,
}

impl Confirmed {
    pub fn new(depth: u32) -> Self {
        Self { depth }
    }

    /// Compute the depth of a transaction based on its inclusion height and the
    /// latest known block.
    ///
    /// Our information about the latest block might be outdated. To avoid an
    /// overflow, we make sure the depth is 0 in case the inclusion height
    /// exceeds our latest known block,
    pub fn from_inclusion_and_latest_block(inclusion_height: u32, latest_block: u32) -> Self {
        let depth = latest_block.saturating_sub(inclusion_height);

        Self { depth }
    }

    pub fn confirmations(&self) -> u32 {
        self.depth + 1
    }

    pub fn meets_target<T>(&self, target: T) -> bool
    where
        u32: PartialOrd<T>,
    {
        self.confirmations() >= target
    }
}

impl ScriptStatus {
    /// Check if the script has any confirmations.
    pub fn is_confirmed(&self) -> bool {
        matches!(self, ScriptStatus::Confirmed(_))
    }

    /// Check if the script has met the given confirmation target.
    pub fn is_confirmed_with<T>(&self, target: T) -> bool
    where
        u32: PartialOrd<T>,
    {
        match self {
            ScriptStatus::Confirmed(inner) => inner.meets_target(target),
            _ => false,
        }
    }

    pub fn has_been_seen(&self) -> bool {
        matches!(self, ScriptStatus::InMempool | ScriptStatus::Confirmed(_))
    }
}

impl fmt::Display for ScriptStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScriptStatus::Unseen => write!(f, "unseen"),
            ScriptStatus::InMempool => write!(f, "in mempool"),
            ScriptStatus::Confirmed(inner) => {
                write!(f, "confirmed with {} blocks", inner.confirmations())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_depth_0_should_meet_confirmation_target_one() {
        let script = ScriptStatus::Confirmed(Confirmed { depth: 0 });

        let confirmed = script.is_confirmed_with(1);

        assert!(confirmed)
    }

    #[test]
    fn given_confirmations_1_should_meet_confirmation_target_one() {
        let script = ScriptStatus::from_confirmations(1);

        let confirmed = script.is_confirmed_with(1);

        assert!(confirmed)
    }

    #[test]
    fn given_inclusion_after_lastest_known_block_at_least_depth_0() {
        let included_in = 10;
        let latest_block = 9;

        let confirmed = Confirmed::from_inclusion_and_latest_block(included_in, latest_block);

        assert_eq!(confirmed.depth, 0)
    }
}
