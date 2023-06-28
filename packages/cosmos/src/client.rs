mod jsonrpc;

use std::{fmt::Display, str::FromStr, sync::Arc};

use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use cosmos_sdk_proto::{
    cosmos::{
        auth::v1beta1::{BaseAccount, QueryAccountRequest},
        bank::v1beta1::{MsgSend, QueryAllBalancesRequest},
        base::{
            abci::v1beta1::TxResponse,
            query::v1beta1::PageRequest,
            tendermint::v1beta1::{GetBlockByHeightRequest, GetLatestBlockRequest},
            v1beta1::Coin,
        },
        tx::v1beta1::{
            AuthInfo, BroadcastMode, BroadcastTxRequest, Fee, GetTxRequest, GetTxsEventRequest,
            ModeInfo, OrderBy, SignDoc, SignerInfo, SimulateRequest, SimulateResponse, Tx, TxBody,
        },
    },
    cosmwasm::wasm::v1::{
        ContractInfo, MsgExecuteContract, MsgInstantiateContract, MsgMigrateContract, MsgStoreCode,
        MsgUpdateAdmin, QueryContractHistoryRequest, QueryContractHistoryResponse,
        QueryContractInfoRequest, QueryRawContractStateRequest, QuerySmartContractStateRequest,
    },
    traits::Message,
};
use deadpool::{async_trait, managed::RecycleResult};
use serde::{de::Visitor, Deserialize};
use tokio::sync::Mutex;
use tonic::{
    codegen::InterceptedService,
    service::Interceptor,
    transport::{Channel, ClientTlsConfig, Endpoint},
    Status,
};

use crate::{address::HasAddressType, Address, AddressType, HasAddress};

use self::jsonrpc::make_jsonrpc_request;

use super::Wallet;

#[derive(Clone)]
pub struct Cosmos {
    pool: deadpool::managed::Pool<CosmosBuilders>,
}

/// Multiple [CosmosBuilder]s to allow for automatically switching between nodes.
pub struct CosmosBuilders {
    builders: Vec<Arc<CosmosBuilder>>,
    next_index: parking_lot::Mutex<usize>,
}

impl CosmosBuilders {
    fn get_first_builder(&self) -> &Arc<CosmosBuilder> {
        self.builders
            .first()
            .expect("Cannot construct a CosmosBuilders with no CosmosBuilder")
    }

    pub fn add(&mut self, builder: impl Into<Arc<CosmosBuilder>>) {
        self.builders.push(builder.into());
    }
}

#[async_trait]
impl deadpool::managed::Manager for CosmosBuilders {
    type Type = CosmosInner;

    type Error = anyhow::Error;

    async fn create(&self) -> Result<Self::Type> {
        self.get_next_builder().build_inner().await
    }

    async fn recycle(&self, _: &mut CosmosInner) -> RecycleResult<anyhow::Error> {
        Ok(())
    }
}

impl CosmosBuilders {
    fn get_next_builder(&self) -> Arc<CosmosBuilder> {
        let mut guard = self.next_index.lock();
        let res = self
            .builders
            .get(*guard)
            .expect("Impossible. get_next_builders failed")
            .clone();

        *guard += 1;
        if *guard >= self.builders.len() {
            *guard = 0;
        }

        res
    }
}

impl Cosmos {
    pub(crate) async fn inner(&self) -> Result<deadpool::managed::Object<CosmosBuilders>> {
        self.pool.get().await.map_err(|e| {
            anyhow::anyhow!("Unable to get internal CosmosInner value from pool: {e:?}")
        })
    }

    pub fn get_first_builder(&self) -> Arc<CosmosBuilder> {
        self.pool.manager().get_first_builder().clone()
    }
}

impl HasAddressType for Cosmos {
    fn get_address_type(&self) -> AddressType {
        self.pool.manager().get_first_builder().address_type
    }
}

pub struct CosmosInterceptor(Option<String>);

impl Interceptor for CosmosInterceptor {
    fn call(&mut self, mut request: tonic::Request<()>) -> Result<tonic::Request<()>, Status> {
        let req = request.metadata_mut();
        if let Some(value) = &self.0 {
            let value = FromStr::from_str(value);
            if let Ok(header_value) = value {
                req.insert("referer", header_value);
            }
        }
        Ok(request)
    }
}

/// Internal data structure containing gRPC clients.
pub struct CosmosInner {
    pub(crate) builder: Arc<CosmosBuilder>,
    pub(crate) rpc_info: Option<RpcInfo>,
    auth_query_client: Mutex<
        cosmos_sdk_proto::cosmos::auth::v1beta1::query_client::QueryClient<
            InterceptedService<Channel, CosmosInterceptor>,
        >,
    >,
    bank_query_client: Mutex<
        cosmos_sdk_proto::cosmos::bank::v1beta1::query_client::QueryClient<
            InterceptedService<Channel, CosmosInterceptor>,
        >,
    >,
    tx_service_client: Mutex<
        cosmos_sdk_proto::cosmos::tx::v1beta1::service_client::ServiceClient<
            InterceptedService<Channel, CosmosInterceptor>,
        >,
    >,
    wasm_query_client: Mutex<
        cosmos_sdk_proto::cosmwasm::wasm::v1::query_client::QueryClient<
            InterceptedService<Channel, CosmosInterceptor>,
        >,
    >,
    tendermint_client: Mutex<
        cosmos_sdk_proto::cosmos::base::tendermint::v1beta1::service_client::ServiceClient<
            InterceptedService<Channel, CosmosInterceptor>,
        >,
    >,
    pub(crate) authz_query_client: Mutex<
        cosmos_sdk_proto::cosmos::authz::v1beta1::query_client::QueryClient<
            InterceptedService<Channel, CosmosInterceptor>,
        >,
    >,
}

pub(crate) struct RpcInfo {
    client: reqwest::Client,
    endpoint: String,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum CosmosNetwork {
    JunoTestnet,
    JunoMainnet,
    JunoLocal,
    OsmosisMainnet,
    OsmosisTestnet,
    OsmosisLocal,
    Dragonfire,
    WasmdLocal,
    SeiMainnet,
    SeiTestnet,
    StargazeTestnet,
    StargazeMainnet,
}

/// Build a connection
#[derive(Clone)]
pub struct CosmosBuilder {
    pub grpc_url: String,
    pub chain_id: String,
    pub gas_coin: String,
    pub address_type: AddressType,
    pub config: CosmosConfig,
}

/// Optional config values.
#[derive(Clone, Debug)]
pub struct CosmosConfig {
    /// Override RPC endpoint to use instead of gRPC.
    ///
    /// NOTE: This feature is experimental and not recommended for anything but
    /// testing purposes.
    pub rpc_url: Option<String>,

    /// Client used for RPC connections. If not provided, creates a new one.
    pub client: Option<reqwest::Client>,

    // Add a multiplier to the gas estimate to account for any gas fluctuations
    pub gas_estimate_multiplier: f64,

    /// Amount of gas coin to send per unit of gas, at the low end.
    pub gas_price_low: f64,

    /// Amount of gas coin to send per unit of gas, at the high end.
    pub gas_price_high: f64,

    /// How many retries at different gas prices should we try before using high
    ///
    /// If this is 0, we'll always go straight to high. 1 means we'll try the
    /// low and the high. 2 means we'll try low, midpoint, and high. And so on
    /// from there.
    pub gas_price_retry_attempts: u64,

    /// How many attempts to give a transaction before giving up
    pub transaction_attempts: usize,

    /// Referrer header that can be set
    referer_header: Option<String>,
}

impl Default for CosmosConfig {
    fn default() -> Self {
        // same amount that CosmosJS uses:  https://github.com/cosmos/cosmjs/blob/e8e65aa0c145616ccb58625c32bffe08b46ff574/packages/cosmwasm-stargate/src/signingcosmwasmclient.ts#L550
        // and OsmoJS too: https://github.com/osmosis-labs/osmojs/blob/bacb2fc322abc3d438581f5dce049f5ae467059d/packages/osmojs/src/utils/gas/estimation.ts#L10
        const DEFAULT_GAS_ESTIMATE_MULTIPLIER: f64 = 1.3;
        Self {
            rpc_url: None,
            client: None,
            gas_estimate_multiplier: DEFAULT_GAS_ESTIMATE_MULTIPLIER,
            gas_price_low: 0.02,
            gas_price_high: 0.03,
            gas_price_retry_attempts: 3,
            transaction_attempts: 30,
            referer_header: None,
        }
    }
}

impl CosmosBuilder {
    pub async fn build(self) -> Result<Cosmos> {
        let cosmos = self.build_lazy();
        // Force strict connection
        std::mem::drop(cosmos.inner().await?);
        Ok(cosmos)
    }

    pub fn build_lazy(self) -> Cosmos {
        CosmosBuilders::from(self).build_lazy()
    }
}

impl From<CosmosBuilder> for CosmosBuilders {
    fn from(c: CosmosBuilder) -> Self {
        CosmosBuilders {
            builders: vec![c.into()],
            next_index: parking_lot::Mutex::new(0),
        }
    }
}

impl CosmosBuilders {
    pub fn build_lazy(self) -> Cosmos {
        Cosmos {
            pool: deadpool::managed::Pool::builder(self)
                .build()
                .expect("Unexpected pool build error"),
        }
    }
}

impl serde::Serialize for CosmosNetwork {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for CosmosNetwork {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_str(CosmosNetworkVisitor)
    }
}

struct CosmosNetworkVisitor;

impl<'de> Visitor<'de> for CosmosNetworkVisitor {
    type Value = CosmosNetwork;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("CosmosNetwork")
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        CosmosNetwork::from_str(v).map_err(E::custom)
    }
}

impl CosmosNetwork {
    fn as_str(self) -> &'static str {
        match self {
            CosmosNetwork::JunoTestnet => "juno-testnet",
            CosmosNetwork::JunoMainnet => "juno-mainnet",
            CosmosNetwork::JunoLocal => "juno-local",
            CosmosNetwork::OsmosisMainnet => "osmosis-mainnet",
            CosmosNetwork::OsmosisTestnet => "osmosis-testnet",
            CosmosNetwork::OsmosisLocal => "osmosis-local",
            CosmosNetwork::Dragonfire => "dragonfire",
            CosmosNetwork::WasmdLocal => "wasmd-local",
            CosmosNetwork::SeiMainnet => "sei-mainnet",
            CosmosNetwork::SeiTestnet => "sei-testnet",
            CosmosNetwork::StargazeTestnet => "stargaze-testnet",
            CosmosNetwork::StargazeMainnet => "stargaze-mainnet",
        }
    }
}

impl Display for CosmosNetwork {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for CosmosNetwork {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "juno-testnet" => Ok(CosmosNetwork::JunoTestnet),
            "juno-mainnet" => Ok(CosmosNetwork::JunoMainnet),
            "juno-local" => Ok(CosmosNetwork::JunoLocal),
            "osmosis-mainnet" => Ok(CosmosNetwork::OsmosisMainnet),
            "osmosis-testnet" => Ok(CosmosNetwork::OsmosisTestnet),
            "osmosis-local" => Ok(CosmosNetwork::OsmosisLocal),
            "dragonfire" => Ok(CosmosNetwork::Dragonfire),
            "wasmd-local" => Ok(CosmosNetwork::WasmdLocal),
            "sei-mainnet" => Ok(CosmosNetwork::SeiMainnet),
            "sei-testnet" => Ok(CosmosNetwork::SeiTestnet),
            "stargaze-testnet" => Ok(CosmosNetwork::StargazeTestnet),
            "stargaze-mainnet" => Ok(CosmosNetwork::StargazeMainnet),
            _ => Err(anyhow::anyhow!("Unknown network: {s}")),
        }
    }
}

impl CosmosNetwork {
    pub async fn connect(self) -> Result<Cosmos> {
        self.builder().await?.build().await
    }

    pub async fn builder(self) -> Result<CosmosBuilder> {
        Ok(match self {
            CosmosNetwork::JunoTestnet => CosmosBuilder::new_juno_testnet(),
            CosmosNetwork::JunoMainnet => CosmosBuilder::new_juno_mainnet(),
            CosmosNetwork::JunoLocal => CosmosBuilder::new_juno_local(),
            CosmosNetwork::OsmosisMainnet => CosmosBuilder::new_osmosis_mainnet(),
            CosmosNetwork::OsmosisTestnet => CosmosBuilder::new_osmosis_testnet(),
            CosmosNetwork::OsmosisLocal => CosmosBuilder::new_osmosis_local(),
            CosmosNetwork::Dragonfire => CosmosBuilder::new_dragonfire(),
            CosmosNetwork::WasmdLocal => CosmosBuilder::new_wasmd_local(),
            CosmosNetwork::SeiMainnet => CosmosBuilder::new_sei_mainnet(),
            CosmosNetwork::SeiTestnet => CosmosBuilder::new_sei_testnet().await?,
            CosmosNetwork::StargazeTestnet => CosmosBuilder::new_stargaze_testnet(),
            CosmosNetwork::StargazeMainnet => CosmosBuilder::new_stargaze_mainnet(),
        })
    }
}

impl CosmosBuilder {
    async fn build_inner(self: Arc<Self>) -> Result<CosmosInner> {
        let grpc_url = &self.grpc_url;
        let grpc_endpoint = grpc_url.parse::<Endpoint>()?;
        let grpc_endpoint = if grpc_url.starts_with("https://") {
            grpc_endpoint.tls_config(ClientTlsConfig::new())?
        } else {
            grpc_endpoint
        };
        let grpc_channel = match tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            grpc_endpoint.connect(),
        )
        .await
        {
            Ok(grpc_channel) => grpc_channel
                .with_context(|| format!("Error establishing gRPC connection to {grpc_url}"))?,
            Err(_) => anyhow::bail!("Timed out while connecting to {grpc_url}"),
        };
        let rpc_info = self.config.rpc_url.as_ref().map(|endpoint| RpcInfo {
            client: self
                .config
                .client
                .as_ref()
                .map_or_else(reqwest::Client::new, |x| x.clone()),
            endpoint: endpoint.clone(),
        });

        let referer_header = self.config.referer_header.clone();

        Ok(CosmosInner {
            builder: self,
            auth_query_client: Mutex::new(
                cosmos_sdk_proto::cosmos::auth::v1beta1::query_client::QueryClient::with_interceptor(
                    grpc_channel.clone(), CosmosInterceptor(referer_header.clone())
                ),
            ),
            bank_query_client: Mutex::new(
                cosmos_sdk_proto::cosmos::bank::v1beta1::query_client::QueryClient::with_interceptor(
                    grpc_channel.clone(),CosmosInterceptor(referer_header.clone())
                ),
            ),
            tx_service_client: Mutex::new(
                cosmos_sdk_proto::cosmos::tx::v1beta1::service_client::ServiceClient::with_interceptor(
                    grpc_channel.clone(),CosmosInterceptor(referer_header.clone())
                ),
            ),
            wasm_query_client: Mutex::new(
                cosmos_sdk_proto::cosmwasm::wasm::v1::query_client::QueryClient::with_interceptor(grpc_channel.clone(), CosmosInterceptor(referer_header.clone()))
            ),
            tendermint_client: Mutex::new(
                cosmos_sdk_proto::cosmos::base::tendermint::v1beta1::service_client::ServiceClient::with_interceptor(grpc_channel.clone(), CosmosInterceptor(referer_header.clone()))
            ),
            authz_query_client: Mutex::new(
                cosmos_sdk_proto::cosmos::authz::v1beta1::query_client::QueryClient::with_interceptor(grpc_channel, CosmosInterceptor(referer_header))
            ),
            rpc_info,
        })
    }
}

impl Cosmos {
    pub fn get_config(&self) -> &CosmosConfig {
        &self.pool.manager().get_first_builder().config
    }

    pub async fn get_base_account(&self, address: impl Into<String>) -> Result<BaseAccount> {
        let inner = self.inner().await?;
        let req = QueryAccountRequest {
            address: address.into(),
        };
        let res = match &inner.rpc_info {
            Some(RpcInfo { client, endpoint }) => {
                make_jsonrpc_request(client, endpoint, req, "/cosmos.auth.v1beta1.Query/Account")
                    .await?
            }
            None => inner
                .auth_query_client
                .lock()
                .await
                .account(req)
                .await?
                .into_inner(),
        };

        Ok(prost::Message::decode(
            res.account.context("no account found")?.value.as_ref(),
        )?)
    }

    pub async fn all_balances(&self, address: impl Into<String>) -> Result<Vec<Coin>> {
        let address = address.into();
        let mut coins = Vec::new();
        let mut pagination = None;
        loop {
            let mut res = self
                .inner()
                .await?
                .bank_query_client
                .lock()
                .await
                .all_balances(QueryAllBalancesRequest {
                    address: address.clone(),
                    pagination: pagination.take(),
                })
                .await?
                .into_inner();
            coins.append(&mut res.balances);
            match res.pagination {
                Some(x) if !x.next_key.is_empty() => {
                    pagination = Some(PageRequest {
                        key: x.next_key,
                        offset: 0,
                        limit: 0,
                        count_total: false,
                        reverse: false,
                    })
                }
                _ => break Ok(coins),
            }
        }
    }

    pub async fn wasm_query(
        &self,
        address: impl Into<String>,
        query_data: impl Into<Vec<u8>>,
    ) -> Result<Vec<u8>> {
        let inner = self.inner().await?;
        let proto_req = QuerySmartContractStateRequest {
            address: address.into(),
            query_data: query_data.into(),
        };
        let res = match &inner.rpc_info {
            Some(RpcInfo { client, endpoint }) => {
                make_jsonrpc_request(
                    client,
                    endpoint,
                    proto_req,
                    "/cosmwasm.wasm.v1.Query/SmartContractState",
                )
                .await?
            }
            None => {
                let mut query_client = inner.wasm_query_client.lock().await;
                query_client
                    .smart_contract_state(proto_req)
                    .await?
                    .into_inner()
            }
        };
        Ok(res.data)
    }

    pub async fn wasm_query_at_height(
        &self,
        address: impl Into<String>,
        query_data: impl Into<Vec<u8>>,
        height: u64,
    ) -> Result<Vec<u8>> {
        // https://docs.cosmos.network/v0.47/run-node/interact-node#query-for-historical-state-using-rest
        let mut request = tonic::Request::new(QuerySmartContractStateRequest {
            address: address.into(),
            query_data: query_data.into(),
        });

        let metadata = request.metadata_mut();
        metadata.insert("x-cosmos-block-height", height.into());

        Ok(self
            .inner()
            .await?
            .wasm_query_client
            .lock()
            .await
            .smart_contract_state(request)
            .await?
            .into_inner()
            .data)
    }

    pub async fn wasm_raw_query(
        &self,
        address: impl Into<String>,
        key: impl Into<Vec<u8>>,
    ) -> Result<Vec<u8>> {
        Ok(self
            .inner()
            .await?
            .wasm_query_client
            .lock()
            .await
            .raw_contract_state(QueryRawContractStateRequest {
                address: address.into(),
                query_data: key.into(),
            })
            .await?
            .into_inner()
            .data)
    }

    pub async fn wasm_raw_query_at_height(
        &self,
        address: impl Into<String>,
        key: impl Into<Vec<u8>>,
        height: u64,
    ) -> Result<Vec<u8>> {
        // https://docs.cosmos.network/v0.47/run-node/interact-node#query-for-historical-state-using-rest
        let mut request = tonic::Request::new(QueryRawContractStateRequest {
            address: address.into(),
            query_data: key.into(),
        });
        let metadata = request.metadata_mut();
        metadata.insert("x-cosmos-block-height", height.into());
        Ok(self
            .inner()
            .await?
            .wasm_query_client
            .lock()
            .await
            .raw_contract_state(request)
            .await?
            .into_inner()
            .data)
    }

    pub async fn wait_for_transaction(&self, txhash: impl Into<String>) -> Result<TxResponse> {
        self.wait_for_transaction_body(txhash).await.map(|x| x.1)
    }

    pub async fn wait_for_transaction_body(
        &self,
        txhash: impl Into<String>,
    ) -> Result<(TxBody, TxResponse)> {
        const DELAY_SECONDS: u64 = 2;
        let txhash = txhash.into();
        let inner = self.inner().await?;
        for attempt in 1..=inner.builder.config.transaction_attempts {
            let mut client = inner.tx_service_client.lock().await;
            let txres = client
                .get_tx(GetTxRequest {
                    hash: txhash.clone(),
                })
                .await;
            match txres {
                Ok(txres) => {
                    let txres = txres.into_inner();
                    return Ok((
                        txres
                            .tx
                            .with_context(|| format!("Missing tx for transaction {txhash}"))?
                            .body
                            .with_context(|| format!("Missing body for transaction {txhash}"))?,
                        txres.tx_response.with_context(|| {
                            format!("Missing tx_response for transaction {txhash}")
                        })?,
                    ));
                }
                Err(e) => {
                    // For some reason, it looks like Osmosis testnet isn't returning a NotFound. Ugly workaround...
                    if e.code() == tonic::Code::NotFound || e.message().contains("not found") {
                        log::debug!(
                            "Transaction {txhash} not ready, attempt #{attempt}/{}",
                            inner.builder.config.transaction_attempts
                        );
                        tokio::time::sleep(tokio::time::Duration::from_secs(DELAY_SECONDS)).await;
                    } else {
                        return Err(e.into());
                    }
                }
            }
        }
        Err(anyhow::anyhow!(
            "Timed out waiting for {txhash} to be ready"
        ))
    }

    pub async fn list_transactions_for(
        &self,
        address: Address,
        limit: Option<u64>,
        offset: Option<u64>,
    ) -> Result<Vec<String>> {
        let x = self
            .inner()
            .await?
            .tx_service_client
            .lock()
            .await
            .get_txs_event(GetTxsEventRequest {
                events: vec![format!("message.sender='{address}'")],
                pagination: Some(PageRequest {
                    key: vec![],
                    offset: offset.unwrap_or_default(),
                    limit: limit.unwrap_or(10),
                    count_total: false,
                    reverse: false,
                }),
                order_by: OrderBy::Asc as i32,
            })
            .await?;
        Ok(x.into_inner()
            .tx_responses
            .into_iter()
            .map(|x| x.txhash)
            .collect())
    }

    pub fn get_gas_coin(&self) -> &String {
        &self.pool.manager().get_first_builder().gas_coin
    }

    /// attempt_number starts at 0
    fn gas_to_coins(&self, gas: u64, attempt_number: u64) -> u64 {
        let config = &self.pool.manager().get_first_builder().config;
        let low = config.gas_price_low;
        let high = config.gas_price_high;
        let attempts = config.gas_price_retry_attempts;

        let gas_price = if attempt_number >= attempts {
            high
        } else {
            assert!(attempts > 0);
            let step = (high - low) / attempts as f64;
            low + step * attempt_number as f64
        };

        (gas as f64 * gas_price) as u64
    }

    pub fn get_gas_multiplier(&self) -> f64 {
        self.pool
            .manager()
            .get_first_builder()
            .config
            .gas_estimate_multiplier
    }

    pub async fn contract_info(&self, address: impl Into<String>) -> Result<ContractInfo> {
        self.inner()
            .await?
            .wasm_query_client
            .lock()
            .await
            .contract_info(QueryContractInfoRequest {
                address: address.into(),
            })
            .await?
            .into_inner()
            .contract_info
            .context("contract_info: missing contract_info (ironic...)")
    }

    pub async fn contract_history(
        &self,
        address: impl Into<String>,
    ) -> Result<QueryContractHistoryResponse> {
        Ok(self
            .inner()
            .await?
            .wasm_query_client
            .lock()
            .await
            .contract_history(QueryContractHistoryRequest {
                address: address.into(),
                pagination: None,
            })
            .await?
            .into_inner())
    }

    pub async fn get_block_info(&self, height: i64) -> Result<BlockInfo> {
        let res = self
            .inner()
            .await?
            .tendermint_client
            .lock()
            .await
            .get_block_by_height(GetBlockByHeightRequest { height })
            .await?
            .into_inner();
        let block_id = res.block_id.context("get_block_info: block_id is None")?;
        let block = res.block.context("get_block_info: block is None")?;
        let header = block.header.context("get_block_info: header is None")?;
        let time = header.time.context("get_block_info: time is None")?;
        let data = block.data.context("get_block_info: data is None")?;
        anyhow::ensure!(
            height == header.height,
            "Mismatched height from blockchain. Got {}, expected {height}",
            header.height
        );
        let mut txhashes = vec![];
        for tx in data.txs {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(tx);
            let digest = hasher.finalize();
            txhashes.push(hex::encode_upper(digest));
        }
        Ok(BlockInfo {
            height: header.height,
            block_hash: hex::encode_upper(block_id.hash),
            timestamp: Utc.timestamp_nanos(time.seconds * 1_000_000_000 + i64::from(time.nanos)),
            txhashes,
        })
    }

    pub async fn get_earliest_block_info(&self) -> Result<BlockInfo> {
        // Really hacky, there must be a better way
        let err = match self.get_block_info(1).await {
            Ok(x) => return Ok(x),
            Err(err) => err,
        };
        if let Some(height) = err.downcast_ref::<tonic::Status>().and_then(|status| {
            let per_needle = |needle: &str| {
                let trimmed = status.message().split(needle).nth(1)?.trim();
                let stripped = trimmed.strip_suffix(')').unwrap_or(trimmed);
                stripped.parse().ok()
            };
            for needle in ["lowest height is", "base height: "] {
                if let Some(x) = per_needle(needle) {
                    return Some(x);
                }
            }
            None
        }) {
            self.get_block_info(height).await
        } else {
            Err(err)
        }
    }

    pub async fn get_latest_block_info(&self) -> Result<BlockInfo> {
        let res = self
            .inner()
            .await?
            .tendermint_client
            .lock()
            .await
            .get_latest_block(GetLatestBlockRequest {})
            .await?
            .into_inner();
        let block_id = res.block_id.context("get_block_info: block_id is None")?;
        let block = res.block.context("get_block_info: block is None")?;
        let header = block.header.context("get_block_info: header is None")?;
        let time = header.time.context("get_block_info: time is None")?;
        let data = block.data.context("get_block_info: data is None")?;
        let mut txhashes = vec![];
        for tx in data.txs {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(tx);
            let digest = hasher.finalize();
            txhashes.push(hex::encode_upper(digest));
        }
        Ok(BlockInfo {
            height: header.height,
            block_hash: hex::encode_upper(block_id.hash),
            timestamp: Utc.timestamp_nanos(time.seconds * 1_000_000_000 + i64::from(time.nanos)),
            txhashes,
        })
    }
}

impl CosmosBuilder {
    pub fn set_referer_header(&mut self, value: String) {
        self.config.referer_header = Some(value);
    }

    fn new_juno_testnet() -> CosmosBuilder {
        CosmosBuilder {
            grpc_url: "http://juno-testnet-grpc.polkachu.com:12690".to_owned(),
            chain_id: "uni-6".to_owned(),
            gas_coin: "ujunox".to_owned(),
            address_type: AddressType::Juno,
            config: CosmosConfig::default(),
        }
    }

    fn new_juno_local() -> CosmosBuilder {
        CosmosBuilder {
            grpc_url: "http://localhost:9090".to_owned(),
            chain_id: "testing".to_owned(),
            gas_coin: "ujunox".to_owned(),
            address_type: AddressType::Juno,
            config: CosmosConfig {
                transaction_attempts: 3, // fail faster during testing
                ..CosmosConfig::default()
            },
        }
    }

    fn new_juno_mainnet() -> CosmosBuilder {
        // Found at: https://cosmos.directory/juno/nodes
        CosmosBuilder {
            grpc_url: "http://juno-grpc.polkachu.com:12690".to_owned(),
            chain_id: "juno-1".to_owned(),
            gas_coin: "ujuno".to_owned(),
            address_type: AddressType::Juno,
            config: CosmosConfig::default(),
        }
    }

    fn new_osmosis_mainnet() -> CosmosBuilder {
        // Found at: https://docs.osmosis.zone/networks/
        CosmosBuilder {
            grpc_url: "http://grpc.osmosis.zone:9090".to_owned(),
            chain_id: "osmosis-1".to_owned(),
            gas_coin: "uosmo".to_owned(),
            address_type: AddressType::Osmo,
            config: CosmosConfig::default(),
        }
    }

    fn new_osmosis_testnet() -> CosmosBuilder {
        // Others available at: https://docs.osmosis.zone/networks/
        CosmosBuilder {
            grpc_url: "https://grpc.osmotest5.osmosis.zone".to_owned(),
            chain_id: "osmo-test-5".to_owned(),
            gas_coin: "uosmo".to_owned(),
            address_type: AddressType::Osmo,
            config: CosmosConfig::default(),
        }
    }

    fn new_osmosis_local() -> CosmosBuilder {
        CosmosBuilder {
            grpc_url: "http://localhost:9090".to_owned(),
            chain_id: "localosmosis".to_owned(),
            gas_coin: "uosmo".to_owned(),
            address_type: AddressType::Osmo,
            config: CosmosConfig::default(),
        }
    }

    fn new_dragonfire() -> CosmosBuilder {
        CosmosBuilder {
            grpc_url: "https://grpc-v4-udb8dydv.dragonfire.sandbox.levana.finance:443".to_owned(),
            chain_id: "dragonfire-4".to_owned(),
            gas_coin: "udragonfire".to_owned(),
            address_type: AddressType::Levana,
            config: CosmosConfig::default(),
        }
    }

    fn new_wasmd_local() -> CosmosBuilder {
        CosmosBuilder {
            grpc_url: "http://localhost:9090".to_owned(),
            chain_id: "localwasmd".to_owned(),
            gas_coin: "uwasm".to_owned(),
            address_type: AddressType::Wasm,
            config: CosmosConfig::default(),
        }
    }
    fn new_sei_mainnet() -> CosmosBuilder {
        CosmosBuilder {
            grpc_url: "https://not-yet-launched/".to_owned(),
            chain_id: "not-yet-launched".to_owned(),
            gas_coin: "usei".to_owned(),
            address_type: AddressType::Sei,
            config: CosmosConfig {
                // https://github.com/sei-protocol/testnet-registry/blob/master/gas.json
                gas_price_low: 0.012,
                gas_price_retry_attempts: 6,
                ..CosmosConfig::default()
            },
        }
    }
    async fn new_sei_testnet() -> Result<CosmosBuilder> {
        // use reqwest to fetch the data from https://github.com/sei-protocol/testnet-registry/blob/master/gas.json

        #[derive(Deserialize)]
        struct SeiGasConfig {
            #[serde(rename = "atlantic-2")]
            pub atlantic_2: SeiGasConfigItem,
        }
        #[derive(Deserialize)]
        struct SeiGasConfigItem {
            pub min_gas_price: f64,
        }

        let url = "https://raw.githubusercontent.com/sei-protocol/testnet-registry/master/gas.json";
        let resp = reqwest::get(url).await?;
        let gas_config: SeiGasConfig = resp.json().await?;

        Ok(CosmosBuilder {
            grpc_url: "https://sei-grpc.kingnodes.com".to_owned(),
            chain_id: "atlantic-2".to_owned(),
            gas_coin: "usei".to_owned(),
            address_type: AddressType::Sei,
            config: CosmosConfig {
                gas_price_low: gas_config.atlantic_2.min_gas_price,
                gas_price_high: gas_config.atlantic_2.min_gas_price * 2.0,
                gas_price_retry_attempts: 6,
                ..CosmosConfig::default()
            },
        })
    }

    fn new_stargaze_testnet() -> CosmosBuilder {
        // https://github.com/cosmos/chain-registry/blob/master/testnets/stargazetestnet/chain.json
        CosmosBuilder {
            grpc_url: "http://grpc-1.elgafar-1.stargaze-apis.com:26660".to_owned(),
            chain_id: "elgafar-1".to_owned(),
            // https://github.com/cosmos/chain-registry/blob/master/testnets/stargazetestnet/assetlist.json
            gas_coin: "ustars".to_owned(),
            address_type: AddressType::Stargaze,
            config: CosmosConfig::default(),
        }
    }

    fn new_stargaze_mainnet() -> CosmosBuilder {
        // https://github.com/cosmos/chain-registry/blob/master/stargaze/chain.json
        CosmosBuilder {
            grpc_url: "http://stargaze-grpc.polkachu.com:13790".to_owned(),
            chain_id: "stargaze-1".to_owned(),
            // https://github.com/cosmos/chain-registry/blob/master/stargaze/assetlist.json
            gas_coin: "ustars".to_owned(),
            address_type: AddressType::Stargaze,
            config: CosmosConfig::default(),
        }
    }
}

#[derive(Debug)]
pub struct BlockInfo {
    pub height: i64,
    pub block_hash: String,
    pub timestamp: DateTime<Utc>,
    pub txhashes: Vec<String>,
}

#[derive(Default)]
pub struct TxBuilder {
    messages: Vec<cosmos_sdk_proto::Any>,
    memo: Option<String>,
    skip_code_check: bool,
}

impl TxBuilder {
    pub fn add_message(mut self, msg: impl Into<TypedMessage>) -> Self {
        self.messages.push(msg.into().0);
        self
    }

    pub fn add_message_mut(&mut self, msg: impl Into<TypedMessage>) {
        self.messages.push(msg.into().0);
    }

    pub fn add_update_contract_admin(
        mut self,
        contract: impl HasAddress,
        wallet: impl HasAddress,
        new_admin: impl HasAddress,
    ) -> Self {
        self.add_update_contract_admin_mut(contract, wallet, new_admin);
        self
    }

    pub fn add_update_contract_admin_mut(
        &mut self,
        contract: impl HasAddress,
        wallet: impl HasAddress,
        new_admin: impl HasAddress,
    ) {
        self.add_message_mut(MsgUpdateAdmin {
            sender: wallet.get_address_string(),
            new_admin: new_admin.get_address_string(),
            contract: contract.get_address_string(),
        });
    }

    pub fn add_execute_message(
        mut self,
        contract: impl HasAddress,
        wallet: impl HasAddress,
        funds: Vec<Coin>,
        msg: impl serde::Serialize,
    ) -> Result<Self> {
        self.add_execute_message_mut(contract, wallet, funds, msg)?;
        Ok(self)
    }

    pub fn add_execute_message_mut(
        &mut self,
        contract: impl HasAddress,
        wallet: impl HasAddress,
        funds: Vec<Coin>,
        msg: impl serde::Serialize,
    ) -> Result<()> {
        self.add_message_mut(MsgExecuteContract {
            sender: wallet.get_address_string(),
            contract: contract.get_address_string(),
            msg: serde_json::to_vec(&msg)?,
            funds,
        });
        Ok(())
    }

    pub fn set_memo(mut self, memo: impl Into<String>) -> Self {
        self.memo = Some(memo.into());
        self
    }

    pub fn set_optional_memo(mut self, memo: impl Into<Option<String>>) -> Self {
        self.memo = memo.into();
        self
    }

    /// When calling [TxBuilder::sign_and_broadcast], skip the check of whether the code is 0
    pub fn skip_code_check(mut self, skip_code_check: bool) -> Self {
        self.skip_code_check = skip_code_check;
        self
    }

    /// Simulate the amount of gas needed to run a transaction.
    pub async fn simulate(&self, cosmos: &Cosmos, wallet: &Wallet) -> Result<FullSimulateResponse> {
        let base_account = cosmos.get_base_account(wallet.address()).await?;

        // Deal with account sequence errors, overall relevant issue is: https://phobosfinance.atlassian.net/browse/PERP-283
        //
        // There may be a bug in Cosmos where simulating expects the wrong
        // sequence number. So: we simulate, trying out the suggested sequence
        // number if necessary, and then we broadcast, again trying the sequence
        // number they recommend if necessary.
        //
        // See: https://github.com/cosmos/cosmos-sdk/issues/11597

        Ok(
            match self
                .simulate_inner(cosmos, wallet, base_account.sequence)
                .await
            {
                Ok(pair) => pair,
                Err(ExpectedSequenceError::RealError(e)) => return Err(e),
                Err(ExpectedSequenceError::NewNumber(x, e)) => {
                    log::warn!("Received an account sequence error while simulating a transaction, retrying with new number {x}: {e:?}");
                    self.simulate_inner(cosmos, wallet, x).await?
                }
            },
        )
    }

    /// Sign transaction, broadcast, wait for it to complete, confirm that it was successful
    /// the gas amount is determined automatically by running a simulation first and padding by a multiplier
    /// the multiplier can by adjusted by calling [Cosmos::set_gas_multiplier]
    pub async fn sign_and_broadcast(&self, cosmos: &Cosmos, wallet: &Wallet) -> Result<TxResponse> {
        let simres = self.simulate(cosmos, wallet).await?;
        self.inner_sign_and_broadcast(
            cosmos,
            wallet,
            simres.body,
            // Gas estimation is not perfect, so we need to adjust it by a multiplier to account for drift
            // Since we're already estimating and padding, the loss of precision from f64 to u64 is negligible
            (simres.gas_used as f64 * cosmos.get_gas_multiplier()) as u64,
        )
        .await
    }

    /// Sign transaction, broadcast, wait for it to complete, confirm that it was successful
    /// unlike sign_and_broadcast(), the gas amount is explicit here and therefore no simulation is run
    pub async fn sign_and_broadcast_with_gas(
        &self,
        cosmos: &Cosmos,
        wallet: &Wallet,
        gas_to_request: u64,
    ) -> Result<TxResponse> {
        self.inner_sign_and_broadcast(cosmos, wallet, self.make_tx_body(), gas_to_request)
            .await
    }

    async fn inner_sign_and_broadcast(
        &self,
        cosmos: &Cosmos,
        wallet: &Wallet,
        body: TxBody,
        gas_to_request: u64,
    ) -> Result<TxResponse> {
        let base_account = cosmos.get_base_account(wallet.address()).await?;

        match self
            .sign_and_broadcast_with(
                cosmos,
                wallet,
                base_account.account_number,
                base_account.sequence,
                body.clone(),
                gas_to_request,
            )
            .await
        {
            Ok(res) => Ok(res),
            Err(ExpectedSequenceError::RealError(e)) => Err(e),
            Err(ExpectedSequenceError::NewNumber(x, e)) => {
                log::warn!("Received an account sequence error while broadcasting a transaction, retrying with new number {x}: {e:?}");
                self.sign_and_broadcast_with(
                    cosmos,
                    wallet,
                    base_account.account_number,
                    x,
                    body,
                    gas_to_request,
                )
                .await
                .map_err(|x| x.into())
            }
        }
    }

    fn make_signer_infos(&self, wallet: &Wallet, sequence: u64) -> Vec<SignerInfo> {
        vec![SignerInfo {
            public_key: Some(cosmos_sdk_proto::Any {
                type_url: "/cosmos.crypto.secp256k1.PubKey".to_owned(),
                value: cosmos_sdk_proto::tendermint::crypto::PublicKey {
                    sum: Some(
                        cosmos_sdk_proto::tendermint::crypto::public_key::Sum::Ed25519(
                            wallet.public_key_bytes().to_owned(),
                        ),
                    ),
                }
                .encode_to_vec(),
            }),
            mode_info: Some(ModeInfo {
                sum: Some(
                    cosmos_sdk_proto::cosmos::tx::v1beta1::mode_info::Sum::Single(
                        cosmos_sdk_proto::cosmos::tx::v1beta1::mode_info::Single { mode: 1 },
                    ),
                ),
            }),
            sequence,
        }]
    }

    /// Make a [TxBody] for this builder
    fn make_tx_body(&self) -> TxBody {
        TxBody {
            messages: self.messages.clone(),
            memo: self.memo.as_deref().unwrap_or_default().to_owned(),
            timeout_height: 0,
            extension_options: vec![],
            non_critical_extension_options: vec![],
        }
    }

    /// Simulate to calculate the gas costs
    async fn simulate_inner(
        &self,
        cosmos: &Cosmos,
        wallet: &Wallet,
        sequence: u64,
    ) -> Result<FullSimulateResponse, ExpectedSequenceError> {
        let body = self.make_tx_body();

        // First simulate the request with no signature and fake gas
        let simulate_tx = Tx {
            auth_info: Some(AuthInfo {
                fee: Some(Fee {
                    amount: vec![],
                    gas_limit: 0,
                    payer: "".to_owned(),
                    granter: "".to_owned(),
                }),
                signer_infos: self.make_signer_infos(wallet, sequence),
            }),
            signatures: vec![vec![]],
            body: Some(body.clone()),
        };

        #[allow(deprecated)]
        let simulate_req = SimulateRequest {
            tx: None,
            tx_bytes: simulate_tx.encode_to_vec(),
        };

        let simres = {
            let inner = cosmos.inner().await?;
            match &inner.rpc_info {
                Some(RpcInfo { client, endpoint }) => {
                    make_jsonrpc_request(
                        client,
                        endpoint,
                        simulate_req,
                        "/cosmos.tx.v1beta1.Service/Simulate",
                    )
                    .await?
                }
                None => {
                    let simres = inner
                        .tx_service_client
                        .lock()
                        .await
                        .simulate(simulate_req)
                        .await;
                    // PERP-283: detect account sequence mismatches
                    match simres {
                        Ok(simres) => simres.into_inner(),
                        Err(e) => {
                            let is_sequence = get_expected_sequence(e.message());
                            let e =
                                anyhow::Error::from(e).context("Unable to simulate transaction");
                            return match is_sequence {
                                None => Err(ExpectedSequenceError::RealError(e)),
                                Some(number) => Err(ExpectedSequenceError::NewNumber(number, e)),
                            };
                        }
                    }
                }
            }
        };

        let gas_used = simres
            .gas_info
            .as_ref()
            .context("Missing gas_info in SimulateResponse")?
            .gas_used;

        Ok(FullSimulateResponse {
            body,
            simres,
            gas_used,
        })
    }

    async fn sign_and_broadcast_with(
        &self,
        cosmos: &Cosmos,
        wallet: &Wallet,
        account_number: u64,
        sequence: u64,
        body: TxBody,
        gas_to_request: u64,
    ) -> Result<TxResponse, ExpectedSequenceError> {
        enum AttemptError {
            Inner(ExpectedSequenceError),
            InsufficientGas(anyhow::Error),
        }
        impl From<anyhow::Error> for AttemptError {
            fn from(e: anyhow::Error) -> Self {
                AttemptError::Inner(e.into())
            }
        }
        let body_ref = &body;
        let retry_with_price = |amount| async move {
            let auth_info = AuthInfo {
                signer_infos: self.make_signer_infos(wallet, sequence),
                fee: Some(Fee {
                    amount: vec![Coin {
                        denom: cosmos.pool.manager().get_first_builder().gas_coin.clone(),
                        amount,
                    }],
                    gas_limit: gas_to_request,
                    payer: "".to_owned(),
                    granter: "".to_owned(),
                }),
            };

            let sign_doc = SignDoc {
                body_bytes: body_ref.encode_to_vec(),
                auth_info_bytes: auth_info.encode_to_vec(),
                chain_id: cosmos.pool.manager().get_first_builder().chain_id.clone(),
                account_number,
            };
            let sign_doc_bytes = sign_doc.encode_to_vec();
            let signature = wallet.sign_bytes(&sign_doc_bytes);

            let tx = Tx {
                body: Some(body_ref.clone()),
                auth_info: Some(auth_info),
                signatures: vec![signature.serialize_compact().to_vec()],
            };

            let inner = cosmos.inner().await?;
            let req = BroadcastTxRequest {
                tx_bytes: tx.encode_to_vec(),
                mode: BroadcastMode::Sync as i32,
            };
            let res = match &inner.rpc_info {
                Some(RpcInfo { client, endpoint }) => {
                    make_jsonrpc_request(
                        client,
                        endpoint,
                        req,
                        "/cosmos.tx.v1beta1.Service/BroadcastTx",
                    )
                    .await?
                }
                None => inner
                    .tx_service_client
                    .lock()
                    .await
                    .broadcast_tx(req)
                    .await
                    .context("Unable to broadcast transaction")?
                    .into_inner(),
            }
            .tx_response
            .context("Missing inner tx_response")?;

            if !self.skip_code_check && res.code != 0 {
                let e = anyhow::anyhow!(
                    "Initial transaction broadcast failed with code {}. Raw log: {}",
                    res.code,
                    res.raw_log
                );
                if res.code == 13 {
                    return Err(AttemptError::InsufficientGas(e));
                }
                let is_sequence = get_expected_sequence(&res.raw_log);
                return Err(AttemptError::Inner(match is_sequence {
                    None => ExpectedSequenceError::RealError(e),
                    Some(number) => ExpectedSequenceError::NewNumber(number, e),
                }));
            };

            log::debug!("Initial BroadcastTxResponse: {res:?}");

            let res = cosmos.wait_for_transaction(res.txhash).await?;
            if !self.skip_code_check && res.code != 0 {
                // We don't do the account sequence mismatch hack work here, once a
                // transaction actually lands on the chain we don't want to ever
                // automatically retry.
                return Err(AttemptError::Inner(ExpectedSequenceError::RealError(
                    anyhow::anyhow!(
                        "Transaction failed with code {}. Raw log: {}",
                        res.code,
                        res.raw_log
                    ),
                )));
            };

            log::debug!("TxResponse: {res:?}");

            Ok(res)
        };

        let attempts = cosmos.get_first_builder().config.gas_price_retry_attempts;
        for attempt_number in 0..attempts {
            let amount = cosmos
                .gas_to_coins(gas_to_request, attempt_number)
                .to_string();
            match retry_with_price(amount).await {
                Ok(x) => return Ok(x),
                Err(AttemptError::InsufficientGas(e)) => {
                    log::debug!(
                        "Insufficient gas in attempt #{attempt_number}, retrying. Error: {e:?}"
                    );
                }
                Err(AttemptError::Inner(e)) => return Err(e),
            }
        }

        let amount = cosmos.gas_to_coins(gas_to_request, attempts).to_string();
        match retry_with_price(amount).await {
            Ok(x) => Ok(x),
            Err(AttemptError::InsufficientGas(e)) => Err(e.into()),
            Err(AttemptError::Inner(e)) => Err(e),
        }
    }
}

pub struct TypedMessage(cosmos_sdk_proto::Any);

impl TypedMessage {
    pub fn new(inner: cosmos_sdk_proto::Any) -> Self {
        TypedMessage(inner)
    }

    pub fn into_inner(self) -> cosmos_sdk_proto::Any {
        self.0
    }
}

impl From<MsgStoreCode> for TypedMessage {
    fn from(msg: MsgStoreCode) -> Self {
        TypedMessage(cosmos_sdk_proto::Any {
            type_url: "/cosmwasm.wasm.v1.MsgStoreCode".to_owned(),
            value: msg.encode_to_vec(),
        })
    }
}

impl From<MsgInstantiateContract> for TypedMessage {
    fn from(msg: MsgInstantiateContract) -> Self {
        TypedMessage(cosmos_sdk_proto::Any {
            type_url: "/cosmwasm.wasm.v1.MsgInstantiateContract".to_owned(),
            value: msg.encode_to_vec(),
        })
    }
}

impl From<MsgMigrateContract> for TypedMessage {
    fn from(msg: MsgMigrateContract) -> Self {
        TypedMessage(cosmos_sdk_proto::Any {
            type_url: "/cosmwasm.wasm.v1.MsgMigrateContract".to_owned(),
            value: msg.encode_to_vec(),
        })
    }
}

impl From<MsgExecuteContract> for TypedMessage {
    fn from(msg: MsgExecuteContract) -> Self {
        TypedMessage(cosmos_sdk_proto::Any {
            type_url: "/cosmwasm.wasm.v1.MsgExecuteContract".to_owned(),
            value: msg.encode_to_vec(),
        })
    }
}

impl From<MsgUpdateAdmin> for TypedMessage {
    fn from(msg: MsgUpdateAdmin) -> Self {
        TypedMessage(cosmos_sdk_proto::Any {
            type_url: "/cosmwasm.wasm.v1.MsgUpdateAdmin".to_owned(),
            value: msg.encode_to_vec(),
        })
    }
}

impl From<MsgSend> for TypedMessage {
    fn from(msg: MsgSend) -> Self {
        TypedMessage(cosmos_sdk_proto::Any {
            type_url: "/cosmos.bank.v1beta1.MsgSend".to_owned(),
            value: msg.encode_to_vec(),
        })
    }
}

pub trait HasCosmos {
    fn get_cosmos(&self) -> &Cosmos;
}

impl HasCosmos for Cosmos {
    fn get_cosmos(&self) -> &Cosmos {
        self
    }
}

impl<T: HasCosmos> HasCosmos for &T {
    fn get_cosmos(&self) -> &Cosmos {
        HasCosmos::get_cosmos(*self)
    }
}

/// Returned the expected account sequence mismatch based on an error message, if present
fn get_expected_sequence(message: &str) -> Option<u64> {
    for line in message.lines() {
        if let Some(x) = get_expected_sequence_single(line) {
            return Some(x);
        }
    }
    None
}

fn get_expected_sequence_single(message: &str) -> Option<u64> {
    let s = message.strip_prefix("account sequence mismatch, expected ")?;
    let comma = s.find(',')?;
    s[..comma].parse().ok()
}

/// Either a real error that should be propagated, or a new account sequence number to try
enum ExpectedSequenceError {
    RealError(anyhow::Error),
    NewNumber(u64, anyhow::Error),
}

impl From<anyhow::Error> for ExpectedSequenceError {
    fn from(e: anyhow::Error) -> Self {
        ExpectedSequenceError::RealError(e)
    }
}

impl From<ExpectedSequenceError> for anyhow::Error {
    fn from(e: ExpectedSequenceError) -> Self {
        match e {
            ExpectedSequenceError::RealError(e) => e,
            ExpectedSequenceError::NewNumber(_, e) => e,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_expected_sequence_good() {
        assert_eq!(
            get_expected_sequence("account sequence mismatch, expected 5, got 0"),
            Some(5)
        );
        assert_eq!(
            get_expected_sequence("account sequence mismatch, expected 2, got 7"),
            Some(2)
        );
        assert_eq!(
            get_expected_sequence("account sequence mismatch, expected 20000001, got 7"),
            Some(20000001)
        );
    }

    #[test]
    fn get_expected_sequence_extra_prelude() {
        assert_eq!(
            get_expected_sequence("blah blah blah\n\naccount sequence mismatch, expected 5, got 0"),
            Some(5)
        );
        assert_eq!(
            get_expected_sequence(
                "foajodifjaolkdfjas aiodjfaof\n\n\naccount sequence mismatch, expected 2, got 7"
            ),
            Some(2)
        );
        assert_eq!(
            get_expected_sequence(
                "iiiiiiiiiiiiii\n\naccount sequence mismatch, expected 20000001, got 7"
            ),
            Some(20000001)
        );
    }

    #[test]
    fn get_expected_sequence_bad() {
        assert_eq!(
            get_expected_sequence("Totally different error message"),
            None
        );
        assert_eq!(
            get_expected_sequence("account sequence mismatch, expected XXXXX, got 7"),
            None
        );
    }

    #[test]
    fn gas_estimate_multiplier() {
        let mut cosmos = CosmosBuilder::new_osmosis_testnet();

        // the same as sign_and_broadcast()
        let multiply_estimated_gas = |cosmos: &CosmosBuilder, gas_used: u64| -> u64 {
            (gas_used as f64 * cosmos.config.gas_estimate_multiplier) as u64
        };

        assert_eq!(multiply_estimated_gas(&cosmos, 1234), 1604);
        cosmos.config.gas_estimate_multiplier = 4.2;
        assert_eq!(multiply_estimated_gas(&cosmos, 1234), 5182);
    }
}

pub struct FullSimulateResponse {
    pub body: TxBody,
    pub simres: SimulateResponse,
    pub gas_used: u64,
}
