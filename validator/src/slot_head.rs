use core::fmt::Debug;
use std::{collections::HashSet, sync::Arc};

use anyhow::Result;
use bls::{CachedPublicKey, PublicKeyBytes, SignatureBytes};
use helper_functions::{
    accessors, misc, predicates,
    signing::{SignForSingleFork, SignForSingleForkAtSlot as _},
};
use log::warn;
use signer::{Signer, SigningMessage, SigningTriple};
use tokio::sync::RwLock;
use types::{
    altair::{
        containers::{SyncAggregatorSelectionData, SyncCommitteeMessage},
        primitives::SubcommitteeIndex,
    },
    cache::IndexSlice,
    combined::BeaconState,
    config::Config,
    nonstandard::{Phase, RelativeEpoch},
    phase0::primitives::{CommitteeIndex, Epoch, Slot, SubnetId, ValidatorIndex, H256},
    preset::Preset,
    traits::BeaconState as _,
};

pub struct SlotHead<P: Preset> {
    pub config: Arc<Config>,
    pub beacon_block_root: H256,
    pub beacon_state: Arc<BeaconState<P>>,
    pub optimistic: bool,
}

impl<P: Preset> SlotHead<P> {
    #[must_use]
    pub fn slot(&self) -> Slot {
        self.beacon_state.slot()
    }

    #[must_use]
    pub fn current_epoch(&self) -> Epoch {
        accessors::get_current_epoch(&self.beacon_state)
    }

    #[must_use]
    pub fn public_key(&self, validator_index: ValidatorIndex) -> &CachedPublicKey {
        &self
            .beacon_state
            .validators()
            .get(validator_index)
            .expect(
                "SlotHead::public_key should only be called with \
                 indices of validators in SlotHead.beacon_state",
            )
            .pubkey
    }

    #[must_use]
    pub fn is_validator_index_protected(
        &self,
        validator_index: ValidatorIndex,
        own_public_keys: &HashSet<PublicKeyBytes>,
    ) -> bool {
        own_public_keys.contains(&self.public_key(validator_index).to_bytes())
    }

    pub fn proposer_index(&self) -> Result<ValidatorIndex> {
        accessors::get_beacon_proposer_index(&self.beacon_state)
    }

    pub fn beacon_committee(&self, committee_index: CommitteeIndex) -> Result<IndexSlice> {
        accessors::beacon_committee(&self.beacon_state, self.slot(), committee_index)
    }

    pub fn beacon_committees(
        &self,
        slot: Slot,
    ) -> Result<impl Iterator<Item = (CommitteeIndex, IndexSlice)>> {
        let committees = accessors::beacon_committees(&self.beacon_state, slot)?;
        Ok((0..).zip(committees))
    }

    #[must_use]
    pub fn has_sync_committee(&self) -> bool {
        self.beacon_state.phase() >= Phase::Altair
    }

    pub fn subnet_id(&self, slot: Slot, committee_index: CommitteeIndex) -> Result<SubnetId> {
        let committees_per_slot =
            accessors::get_committee_count_per_slot(&self.beacon_state, RelativeEpoch::Current);

        misc::compute_subnet_for_attestation::<P>(committees_per_slot, slot, committee_index)
    }

    pub async fn selection_proofs<I>(
        &self,
        committee_indices_with_pubkeys: I,
        signer: &RwLock<Signer>,
    ) -> Result<Vec<Option<SignatureBytes>>>
    where
        I: IntoIterator<Item = (CommitteeIndex, PublicKeyBytes)> + Send,
    {
        let slot = self.slot();

        let (triples, committee_indices): (Vec<_>, Vec<_>) = committee_indices_with_pubkeys
            .into_iter()
            .map(|(committee_index, public_key)| {
                let triple = SigningTriple {
                    message: SigningMessage::AggregationSlot { slot },
                    signing_root: slot.signing_root(&self.config, &self.beacon_state),
                    public_key,
                };

                (triple, committee_index)
            })
            .unzip();

        signer
            .read()
            .await
            .sign_triples(triples, Some(self.beacon_state.as_ref().into()))
            .await?
            .zip(committee_indices)
            .map(|(signature, committee_index)| {
                let slot_signature = signature.into();

                let aggregator = predicates::is_aggregator(
                    &self.beacon_state,
                    self.slot(),
                    committee_index,
                    slot_signature,
                )?;

                Ok(aggregator.then_some(slot_signature))
            })
            .collect()
    }

    /// <https://github.com/ethereum/consensus-specs/blob/dc14b79a521fb621f0d2b9da9410f6e7ffaa7df5/specs/altair/validator.md#prepare-sync-committee-message>
    pub async fn sync_committee_messages<I>(
        &self,
        slot: Slot,
        validator_indices_with_pubkeys: I,
        signer: &RwLock<Signer>,
    ) -> Result<impl Iterator<Item = SyncCommitteeMessage> + '_>
    where
        I: IntoIterator<Item = (ValidatorIndex, PublicKeyBytes)> + Send,
    {
        let (triples, validator_indices): (Vec<_>, Vec<_>) = validator_indices_with_pubkeys
            .into_iter()
            .map(|(validator_index, public_key)| {
                let triple = SigningTriple {
                    message: SigningMessage::SyncCommitteeMessage {
                        beacon_block_root: self.beacon_block_root,
                        slot,
                    },
                    signing_root: self.beacon_block_root.signing_root(
                        &self.config,
                        &self.beacon_state,
                        self.slot(),
                    ),
                    public_key,
                };

                (triple, validator_index)
            })
            .unzip();

        let messages = signer
            .read()
            .await
            .sign_triples(triples, Some(self.beacon_state.as_ref().into()))
            .await?
            .zip(validator_indices)
            .map(move |(signature, validator_index)| SyncCommitteeMessage {
                slot,
                beacon_block_root: self.beacon_block_root,
                validator_index,
                signature: signature.into(),
            });

        Ok(messages)
    }

    /// <https://github.com/ethereum/consensus-specs/blob/dc14b79a521fb621f0d2b9da9410f6e7ffaa7df5/specs/altair/validator.md#aggregation-selection>
    pub async fn sync_committee_selection_proofs(
        &self,
        subcommittee_indices_with_pubkeys: impl Iterator<Item = (SubcommitteeIndex, PublicKeyBytes)>
            + Send,
        signer: &RwLock<Signer>,
    ) -> Result<Vec<Option<SignatureBytes>>> {
        let triples = subcommittee_indices_with_pubkeys.map(|(subcommittee_index, public_key)| {
            let selection_data = SyncAggregatorSelectionData {
                slot: self.slot(),
                subcommittee_index,
            };

            SigningTriple {
                message: SigningMessage::SyncAggregatorSelectionData(selection_data),
                signing_root: selection_data.signing_root(&self.config, &self.beacon_state),
                public_key,
            }
        });

        signer
            .read()
            .await
            .sign_triples(triples, Some(self.beacon_state.as_ref().into()))
            .await?
            .map(|signature| {
                let selection_proof = signature.into();
                let aggregator = predicates::is_sync_committee_aggregator::<P>(selection_proof);
                Ok(aggregator.then_some(selection_proof))
            })
            .collect()
    }

    pub async fn sign_beacon_block(
        &self,
        signer: &RwLock<Signer>,
        block: &(impl SignForSingleFork<P> + Debug + Send + Sync),
        message: SigningMessage<'_, P>,
        cached_public_key: &CachedPublicKey,
    ) -> Option<SignatureBytes> {
        let public_key = cached_public_key.to_bytes();

        match signer
            .read()
            .await
            .sign(
                message,
                block.signing_root(&self.config, &self.beacon_state),
                Some(self.beacon_state.as_ref().into()),
                public_key,
            )
            .await
        {
            Ok(signature) => Some(signature.into()),
            Err(error) => {
                warn!(
                    "failed to sign beacon block \
                     (error: {error:?}, block: {block:?}, public_key: {public_key:?})",
                );
                None
            }
        }
    }
}
