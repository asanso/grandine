use std::sync::Arc;

use anyhow::{Context, Error, Result};
use bls::PublicKeyBytes;
use clock::{Tick, TickKind};
use dedicated_executor::DedicatedExecutor;
use eth1_api::ApiController;
use features::Feature;
use fork_choice_control::Wait;
use prometheus_metrics::Metrics;
use ssz::ContiguousList;
use std_ext::ArcExt as _;
use types::{
    combined::BeaconState,
    config::Config,
    phase0::{
        containers::{Attestation, AttestationData},
        primitives::{Epoch, H256},
    },
    preset::Preset,
};

use crate::{
    attestation_agg_pool::{
        pool::Pool,
        tasks::{
            BestProposableAttestationsTask, ComputeProposerIndicesTask, InsertAttestationTask,
            PackProposableAttestationsTask, SetRegisteredValidatorsTask,
        },
    },
    misc::PoolTask,
};

pub struct Manager<P: Preset, W: Wait> {
    controller: ApiController<P, W>,
    dedicated_executor: Arc<DedicatedExecutor>,
    metrics: Option<Arc<Metrics>>,
    pool: Arc<Pool<P>>,
}

impl<P: Preset, W: Wait> Manager<P, W> {
    #[must_use]
    pub fn new(
        controller: ApiController<P, W>,
        dedicated_executor: Arc<DedicatedExecutor>,
        metrics: Option<Arc<Metrics>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            controller,
            dedicated_executor,
            metrics,
            pool: Arc::new(Pool::default()),
        })
    }

    #[must_use]
    pub fn config(&self) -> &Arc<Config> {
        self.controller.chain_config()
    }

    pub async fn on_tick(&self, tick: Tick) {
        let Tick { slot, kind } = tick;

        match kind {
            TickKind::Propose => {
                self.pool.on_slot(slot).await;
            }
            TickKind::Attest => {
                self.pool.clear_best_proposable_attestations().await;
            }
            TickKind::AggregateFourth => {
                let next_slot = slot + 1;

                if Feature::AlwaysPrepackAttestations.is_enabled()
                    || self
                        .pool
                        .has_registered_validators_proposing_in_slots(next_slot..=next_slot)
                        .await
                {
                    self.pack_proposable_attestations();
                }
            }
            _ => {}
        }
    }

    pub async fn aggregate_attestations_by_epoch(&self, epoch: Epoch) -> Vec<Attestation<P>> {
        self.pool.aggregate_attestations_by_epoch(epoch).await
    }

    pub async fn best_aggregate_attestation(
        &self,
        data: AttestationData,
    ) -> Option<Attestation<P>> {
        self.pool.best_aggregate_attestation(data).await
    }

    pub async fn best_aggregate_attestation_by_data_root(
        &self,
        attestation_data_root: H256,
        epoch: Epoch,
    ) -> Option<Attestation<P>> {
        self.pool
            .best_aggregate_attestation_by_data_root(attestation_data_root, epoch)
            .await
    }

    pub async fn best_proposable_attestations(
        &self,
        beacon_state: Arc<BeaconState<P>>,
    ) -> Result<ContiguousList<Attestation<P>, P::MaxAttestations>> {
        self.spawn_task(BestProposableAttestationsTask {
            controller: self.controller.clone_arc(),
            pool: self.pool.clone_arc(),
            beacon_state,
        })
        .await
    }

    pub fn compute_proposer_indices(&self, beacon_state: Arc<BeaconState<P>>) {
        self.spawn_detached(ComputeProposerIndicesTask {
            pool: self.pool.clone_arc(),
            beacon_state,
        });
    }

    pub fn insert_attestation(&self, wait_group: W, attestation: Arc<Attestation<P>>) {
        self.spawn_detached(InsertAttestationTask {
            wait_group,
            pool: self.pool.clone_arc(),
            attestation,
            metrics: self.metrics.clone(),
        });
    }

    pub fn pack_proposable_attestations(&self) {
        self.spawn_detached(PackProposableAttestationsTask {
            pool: self.pool.clone_arc(),
            controller: self.controller.clone_arc(),
            metrics: self.metrics.clone(),
        });
    }

    pub fn set_registered_validators(&self, pubkeys: Vec<PublicKeyBytes>) {
        self.spawn_detached(SetRegisteredValidatorsTask {
            pool: self.pool.clone_arc(),
            controller: self.controller.clone_arc(),
            pubkeys,
        });
    }

    pub async fn singular_attestations_by_epoch(&self, epoch: Epoch) -> Vec<Arc<Attestation<P>>> {
        self.pool.singular_attestations_by_epoch(epoch).await
    }

    async fn spawn_task<T: PoolTask>(&self, task: T) -> Result<T::Output> {
        self.dedicated_executor
            .spawn(task.run())
            .await
            .map_err(Error::msg)
            .context("attestation aggregation pool task failed")?
    }

    fn spawn_detached(&self, task: impl PoolTask) {
        self.dedicated_executor.spawn(task.run()).detach()
    }
}
