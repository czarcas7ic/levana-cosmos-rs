use anyhow::Result;

use crate::{Cosmos, CosmosBuilder, CosmosNetwork};

/// Command line options for connecting to a Cosmos network
#[derive(clap::Parser, Clone, Debug)]
pub struct CosmosOpt {
    /// Which blockchain to connect to for grabbing blocks
    #[clap(long, env = "COSMOS_NETWORK")]
    pub network: CosmosNetwork,
    /// Optional gRPC endpoint override
    #[clap(long, env = "COSMOS_GRPC", global = true)]
    pub cosmos_grpc: Option<String>,
    /// Optional chain ID override
    #[clap(long, env = "COSMOS_CHAIN_ID", global = true)]
    pub chain_id: Option<String>,
    /// Optional gas multiplier override
    #[clap(long, env = "COSMOS_GAS_MULTIPLIER", global = true)]
    pub gas_multiplier: Option<f64>,
    /// Referer header
    #[clap(long, short, global = true, env = "COSMOS_REFERER_HEADER")]
    referer_header: Option<String>,
}

impl CosmosOpt {
    pub async fn builder(&self) -> Result<CosmosBuilder> {
        self.clone().into_builder().await
    }

    pub async fn into_builder(self) -> Result<CosmosBuilder> {
        let CosmosOpt {
            network,
            cosmos_grpc,
            chain_id,
            gas_multiplier,
            referer_header,
        } = self;

        let mut builder = network.builder().await?;
        if let Some(grpc) = cosmos_grpc {
            builder.grpc_url = grpc;
        }
        if let Some(chain_id) = chain_id {
            builder.chain_id = chain_id;
        }

        if let Some(gas_multiplier) = gas_multiplier {
            builder.config.gas_estimate_multiplier = gas_multiplier;
        }

        if let Some(referer_header) = referer_header {
            builder.set_referer_header(referer_header);
        }

        Ok(builder)
    }

    pub async fn build(&self) -> Result<Cosmos> {
        self.builder().await?.build().await
    }

    pub async fn build_lazy(&self) -> Result<Cosmos> {
        Ok(self.builder().await?.build_lazy())
    }
}