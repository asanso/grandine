use core::time::Duration;
use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use builder_api::BuilderConfig;
use eth1_api::AuthOptions;
use features::Feature;
use http_api::HttpApiConfig;
use itertools::Itertools as _;
use log::info;
use p2p::NetworkConfig;
use reqwest::Url;
use runtime::{MetricsConfig, StorageConfig};
use signer::Web3SignerConfig;
use types::{
    config::Config as ChainConfig,
    phase0::primitives::{ExecutionAddress, ExecutionBlockNumber, Slot, H256},
};

use crate::{
    commands::GrandineCommand, predefined_network::PredefinedNetwork, validators::Validators,
};

// False positive. The `bool`s are independent.
#[allow(clippy::struct_excessive_bools)]
#[cfg_attr(test, derive(Debug))]
pub struct GrandineConfig {
    pub predefined_network: Option<PredefinedNetwork>,
    pub chain_config: Arc<ChainConfig>,
    pub deposit_contract_starting_block: Option<ExecutionBlockNumber>,
    pub genesis_state_file: Option<PathBuf>,
    pub checkpoint_sync_url: Option<Url>,
    pub force_checkpoint_sync: bool,
    pub back_sync: bool,
    pub eth1_rpc_urls: Vec<Url>,
    pub data_dir: PathBuf,
    pub validators: Validators,
    pub keystore_storage_password_file: Option<PathBuf>,
    pub graffiti: Vec<H256>,
    pub max_empty_slots: u64,
    pub suggested_fee_recipient: ExecutionAddress,
    pub network_config: NetworkConfig,
    pub storage_config: StorageConfig,
    pub unfinalized_states_in_memory: u64,
    pub request_timeout: Duration,
    pub command: Option<GrandineCommand>,
    pub slashing_enabled: bool,
    pub slashing_history_limit: u64,
    pub features: Vec<Feature>,
    pub state_slot: Option<Slot>,
    pub auth_options: AuthOptions,
    pub builder_config: Option<BuilderConfig>,
    pub web3signer_config: Web3SignerConfig,
    pub http_api_config: HttpApiConfig,
    pub metrics_config: MetricsConfig,
    pub track_liveness: bool,
    pub use_validator_key_cache: bool,
    pub slashing_protection_history_limit: u64,
    pub in_memory: bool,
}

impl GrandineConfig {
    #[allow(clippy::cognitive_complexity)]
    pub fn report(&self) {
        let Self {
            predefined_network,
            chain_config,
            back_sync,
            eth1_rpc_urls,
            data_dir,
            graffiti,
            suggested_fee_recipient,
            network_config,
            storage_config,
            slashing_enabled,
            slashing_history_limit,
            state_slot,
            builder_config,
            web3signer_config,
            http_api_config,
            metrics_config,
            checkpoint_sync_url,
            use_validator_key_cache,
            ..
        } = self;

        let StorageConfig {
            db_size,
            archival_epoch_interval,
            ..
        } = storage_config;

        match predefined_network {
            Some(network) => info!("network: {network}"),
            None => info!(
                "network: custom with {} preset and {} configuration",
                chain_config.preset_base, chain_config.config_name,
            ),
        }

        info!("data directory: {data_dir:?}");
        info!("Eth2 database upper limit: {}", db_size.to_string_as(true));

        info!(
            "Eth1 database upper limit: {}",
            storage_config.eth1_db_size.to_string_as(true),
        );

        info!("Eth1 RPC URLs: [{}]", eth1_rpc_urls.iter().format(", "));
        info!("graffiti: {graffiti:?}");
        info!("HTTP API address: {}", http_api_config.address);

        if let Some(metrics_server_config) = &metrics_config.metrics_server_config {
            info!(
                "Metrics server address: {}",
                SocketAddr::from(metrics_server_config),
            );
        }

        info!("archival interval: {archival_epoch_interval} epochs");
        info!("slasher enabled: {slashing_enabled}");

        if let Some(client_version) = &network_config.identify_agent_version {
            info!("client version: {client_version}");
        }

        if let Some(slot) = state_slot {
            info!("force state slot: {slot}");
        }

        if let Some(builder_config) = builder_config {
            info!(
                "using external block builder (API URL: {})",
                builder_config.builder_api_url,
            );
        }

        if let Some(checkpoint_sync_url) = checkpoint_sync_url {
            info!("checkpoint sync url: {checkpoint_sync_url}");
        }

        if !web3signer_config.urls.is_empty() {
            info!(
                "using Web3Signer API to sign validator messages (API URLs: [{}])",
                web3signer_config.urls.iter().format(", "),
            );
        }

        if *slashing_enabled {
            info!("slasher history limit: {slashing_history_limit}");
        }

        info!("suggested fee recipient: {suggested_fee_recipient}");
        info!("back sync enabled: {back_sync}");

        if *use_validator_key_cache {
            info!("using validator key cache");
        }
    }
}
