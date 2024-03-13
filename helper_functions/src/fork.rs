use core::ops::BitOrAssign as _;
use std::sync::Arc;

use anyhow::Result;
use itertools::Itertools as _;
use ssz::PersistentList;
use std_ext::ArcExt as _;
use types::{
    altair::beacon_state::BeaconState as AltairBeaconState,
    bellatrix::{
        beacon_state::BeaconState as BellatrixBeaconState,
        containers::ExecutionPayloadHeader as BellatrixExecutionPayloadHeader,
    },
    capella::{
        beacon_state::BeaconState as CapellaBeaconState,
        containers::ExecutionPayloadHeader as CapellaExecutionPayloadHeader,
    },
    config::Config,
    deneb::{
        beacon_state::BeaconState as DenebBeaconState,
        containers::ExecutionPayloadHeader as DenebExecutionPayloadHeader,
    },
    phase0::{
        beacon_state::BeaconState as Phase0BeaconState,
        containers::{Fork, PendingAttestation},
        primitives::H256,
    },
    preset::Preset,
};

use crate::accessors;

pub fn upgrade_to_altair<P: Preset>(
    config: &Config,
    pre: Phase0BeaconState<P>,
) -> Result<AltairBeaconState<P>> {
    let epoch = accessors::get_current_epoch(&pre);

    let Phase0BeaconState {
        genesis_time,
        genesis_validators_root,
        slot,
        fork,
        latest_block_header,
        block_roots,
        state_roots,
        historical_roots,
        eth1_data,
        eth1_data_votes,
        eth1_deposit_index,
        validators,
        balances,
        randao_mixes,
        slashings,
        previous_epoch_attestations,
        current_epoch_attestations: _,
        justification_bits,
        previous_justified_checkpoint,
        current_justified_checkpoint,
        finalized_checkpoint,
        cache,
    } = pre;

    let fork = Fork {
        previous_version: fork.previous_version,
        current_version: config.altair_fork_version,
        epoch,
    };

    let zero_participation = PersistentList::repeat_zero_with_length_of(&validators);
    let inactivity_scores = PersistentList::repeat_zero_with_length_of(&validators);

    let mut post = AltairBeaconState {
        // > Versioning
        genesis_time,
        genesis_validators_root,
        slot,
        fork,
        // > History
        latest_block_header,
        block_roots,
        state_roots,
        historical_roots,
        // > Eth1
        eth1_data,
        eth1_data_votes,
        eth1_deposit_index,
        // > Registry
        validators,
        balances,
        // > Randomness
        randao_mixes,
        // > Slashings
        slashings,
        // > Participation
        previous_epoch_participation: zero_participation.clone(),
        current_epoch_participation: zero_participation,
        // > Finality
        justification_bits,
        previous_justified_checkpoint,
        current_justified_checkpoint,
        finalized_checkpoint,
        // > Inactivity
        inactivity_scores,
        // Sync
        current_sync_committee: Arc::default(),
        next_sync_committee: Arc::default(),
        // Cache
        cache,
    };

    // > Fill in previous epoch participation from the pre state's pending attestations
    translate_participation(&mut post, &previous_epoch_attestations)?;

    // > Fill in sync committees
    // > Note: A duplicate committee is assigned for the current and next committee at the fork
    // >       boundary
    let sync_committee = accessors::get_next_sync_committee(&post)?;
    post.current_sync_committee = sync_committee.clone_arc();
    post.next_sync_committee = sync_committee;

    Ok(post)
}

fn translate_participation<'attestations, P: Preset>(
    state: &mut AltairBeaconState<P>,
    pending_attestations: impl IntoIterator<Item = &'attestations PendingAttestation<P>>,
) -> Result<()> {
    for attestation in pending_attestations {
        let PendingAttestation {
            ref aggregation_bits,
            data,
            inclusion_delay,
            ..
        } = *attestation;

        let attesting_indices =
            accessors::get_attesting_indices(state, data, aggregation_bits)?.collect_vec();

        // > Translate attestation inclusion info to flag indices
        let participation_flags =
            accessors::get_attestation_participation_flags(state, data, inclusion_delay)?;

        // > Apply flags to all attesting validators
        for attesting_index in attesting_indices {
            // Indexing here has a negligible effect on performance and only has to be done once.
            state
                .previous_epoch_participation
                .get_mut(attesting_index)?
                .bitor_assign(participation_flags);
        }
    }

    Ok(())
}

#[must_use]
pub fn upgrade_to_bellatrix<P: Preset>(
    config: &Config,
    pre: AltairBeaconState<P>,
) -> BellatrixBeaconState<P> {
    let epoch = accessors::get_current_epoch(&pre);

    let AltairBeaconState {
        genesis_time,
        genesis_validators_root,
        slot,
        fork,
        latest_block_header,
        block_roots,
        state_roots,
        historical_roots,
        eth1_data,
        eth1_data_votes,
        eth1_deposit_index,
        validators,
        balances,
        randao_mixes,
        slashings,
        previous_epoch_participation,
        current_epoch_participation,
        justification_bits,
        previous_justified_checkpoint,
        current_justified_checkpoint,
        finalized_checkpoint,
        inactivity_scores,
        current_sync_committee,
        next_sync_committee,
        cache,
    } = pre;

    let fork = Fork {
        previous_version: fork.current_version,
        current_version: config.bellatrix_fork_version,
        epoch,
    };

    BellatrixBeaconState {
        // > Versioning
        genesis_time,
        genesis_validators_root,
        slot,
        fork,
        // > History
        latest_block_header,
        block_roots,
        state_roots,
        historical_roots,
        // > Eth1
        eth1_data,
        eth1_data_votes,
        eth1_deposit_index,
        // > Registry
        validators,
        balances,
        // > Randomness
        randao_mixes,
        // > Slashings
        slashings,
        // > Participation
        previous_epoch_participation,
        current_epoch_participation,
        // > Finality
        justification_bits,
        previous_justified_checkpoint,
        current_justified_checkpoint,
        finalized_checkpoint,
        // > Inactivity
        inactivity_scores,
        // > Sync
        current_sync_committee,
        next_sync_committee,
        // > Execution-layer
        latest_execution_payload_header: BellatrixExecutionPayloadHeader::default(),
        // Cache
        cache,
    }
}

#[must_use]
pub fn upgrade_to_capella<P: Preset>(
    config: &Config,
    pre: BellatrixBeaconState<P>,
) -> CapellaBeaconState<P> {
    let epoch = accessors::get_current_epoch(&pre);

    let BellatrixBeaconState {
        genesis_time,
        genesis_validators_root,
        slot,
        fork,
        latest_block_header,
        block_roots,
        state_roots,
        historical_roots,
        eth1_data,
        eth1_data_votes,
        eth1_deposit_index,
        validators,
        balances,
        randao_mixes,
        slashings,
        previous_epoch_participation,
        current_epoch_participation,
        justification_bits,
        previous_justified_checkpoint,
        current_justified_checkpoint,
        finalized_checkpoint,
        inactivity_scores,
        current_sync_committee,
        next_sync_committee,
        latest_execution_payload_header,
        cache,
    } = pre;

    let fork = Fork {
        previous_version: fork.current_version,
        current_version: config.capella_fork_version,
        epoch,
    };

    let BellatrixExecutionPayloadHeader {
        parent_hash,
        fee_recipient,
        state_root,
        receipts_root,
        logs_bloom,
        prev_randao,
        block_number,
        gas_limit,
        gas_used,
        timestamp,
        extra_data,
        base_fee_per_gas,
        block_hash,
        transactions_root,
    } = latest_execution_payload_header;

    let latest_execution_payload_header = CapellaExecutionPayloadHeader {
        parent_hash,
        fee_recipient,
        state_root,
        receipts_root,
        logs_bloom,
        prev_randao,
        block_number,
        gas_limit,
        gas_used,
        timestamp,
        extra_data,
        base_fee_per_gas,
        block_hash,
        transactions_root,
        // > [New in Capella]
        withdrawals_root: H256::zero(),
    };

    CapellaBeaconState {
        // > Versioning
        genesis_time,
        genesis_validators_root,
        slot,
        fork,
        // > History
        latest_block_header,
        block_roots,
        state_roots,
        historical_roots,
        // > Eth1
        eth1_data,
        eth1_data_votes,
        eth1_deposit_index,
        // > Registry
        validators,
        balances,
        // > Randomness
        randao_mixes,
        // > Slashings
        slashings,
        // > Participation
        previous_epoch_participation,
        current_epoch_participation,
        // > Finality
        justification_bits,
        previous_justified_checkpoint,
        current_justified_checkpoint,
        finalized_checkpoint,
        // > Inactivity
        inactivity_scores,
        // > Sync
        current_sync_committee,
        next_sync_committee,
        // > Execution-layer
        latest_execution_payload_header,
        // > Withdrawals
        next_withdrawal_index: 0,
        next_withdrawal_validator_index: 0,
        // > Deep history valid from Capella onwards
        historical_summaries: PersistentList::default(),
        // Cache
        cache,
    }
}

#[must_use]
#[allow(clippy::too_many_lines)]
pub fn upgrade_to_deneb<P: Preset>(
    config: &Config,
    pre: CapellaBeaconState<P>,
) -> DenebBeaconState<P> {
    let epoch = accessors::get_current_epoch(&pre);

    let CapellaBeaconState {
        genesis_time,
        genesis_validators_root,
        slot,
        fork,
        latest_block_header,
        block_roots,
        state_roots,
        historical_roots,
        eth1_data,
        eth1_data_votes,
        eth1_deposit_index,
        validators,
        balances,
        randao_mixes,
        slashings,
        previous_epoch_participation,
        current_epoch_participation,
        justification_bits,
        previous_justified_checkpoint,
        current_justified_checkpoint,
        finalized_checkpoint,
        inactivity_scores,
        current_sync_committee,
        next_sync_committee,
        latest_execution_payload_header,
        next_withdrawal_index,
        next_withdrawal_validator_index,
        historical_summaries,
        cache,
    } = pre;

    let fork = Fork {
        previous_version: fork.current_version,
        current_version: config.deneb_fork_version,
        epoch,
    };

    let CapellaExecutionPayloadHeader {
        parent_hash,
        fee_recipient,
        state_root,
        receipts_root,
        logs_bloom,
        prev_randao,
        block_number,
        gas_limit,
        gas_used,
        timestamp,
        extra_data,
        base_fee_per_gas,
        block_hash,
        transactions_root,
        withdrawals_root,
    } = latest_execution_payload_header;

    let latest_execution_payload_header = DenebExecutionPayloadHeader {
        parent_hash,
        fee_recipient,
        state_root,
        receipts_root,
        logs_bloom,
        prev_randao,
        block_number,
        gas_limit,
        gas_used,
        timestamp,
        extra_data,
        base_fee_per_gas,
        block_hash,
        transactions_root,
        withdrawals_root,
        // > [New in Deneb:EIP4844]
        blob_gas_used: 0,
        excess_blob_gas: 0,
    };

    DenebBeaconState {
        // > Versioning
        genesis_time,
        genesis_validators_root,
        slot,
        fork,
        // > History
        latest_block_header,
        block_roots,
        state_roots,
        historical_roots,
        // > Eth1
        eth1_data,
        eth1_data_votes,
        eth1_deposit_index,
        // > Registry
        validators,
        balances,
        // > Randomness
        randao_mixes,
        // > Slashings
        slashings,
        // > Participation
        previous_epoch_participation,
        current_epoch_participation,
        // > Finality
        justification_bits,
        previous_justified_checkpoint,
        current_justified_checkpoint,
        finalized_checkpoint,
        // > Inactivity
        inactivity_scores,
        // > Sync
        current_sync_committee,
        next_sync_committee,
        // > Execution-layer
        latest_execution_payload_header,
        // > Withdrawals
        next_withdrawal_index,
        next_withdrawal_validator_index,
        // > Deep history valid from Capella onwards
        historical_summaries,
        // Cache
        cache,
    }
}

#[cfg(test)]
mod spec_tests {
    use spec_test_utils::Case;
    use test_generator::test_resources;
    use types::preset::{Mainnet, Minimal};

    use super::*;

    #[test_resources("consensus-spec-tests/tests/mainnet/altair/fork/*/*/*")]
    fn altair_mainnet(case: Case) {
        run_altair_case::<Mainnet>(case);
    }

    #[test_resources("consensus-spec-tests/tests/minimal/altair/fork/*/*/*")]
    fn altair_minimal(case: Case) {
        run_altair_case::<Minimal>(case);
    }

    #[test_resources("consensus-spec-tests/tests/mainnet/bellatrix/fork/*/*/*")]
    fn bellatrix_mainnet(case: Case) {
        run_bellatrix_case::<Mainnet>(case);
    }

    #[test_resources("consensus-spec-tests/tests/minimal/bellatrix/fork/*/*/*")]
    fn bellatrix_minimal(case: Case) {
        run_bellatrix_case::<Minimal>(case);
    }

    #[test_resources("consensus-spec-tests/tests/mainnet/capella/fork/*/*/*")]
    fn capella_mainnet(case: Case) {
        run_capella_case::<Mainnet>(case);
    }

    #[test_resources("consensus-spec-tests/tests/minimal/capella/fork/*/*/*")]
    fn capella_minimal(case: Case) {
        run_capella_case::<Minimal>(case);
    }

    #[test_resources("consensus-spec-tests/tests/mainnet/deneb/fork/*/*/*")]
    fn deneb_mainnet(case: Case) {
        run_deneb_case::<Mainnet>(case);
    }

    #[test_resources("consensus-spec-tests/tests/minimal/deneb/fork/*/*/*")]
    fn deneb_minimal(case: Case) {
        run_deneb_case::<Minimal>(case);
    }

    fn run_altair_case<P: Preset>(case: Case) {
        let pre = case.ssz_default("pre");
        let expected_post = case.ssz_default("post");

        let actual_post = upgrade_to_altair::<P>(&P::default_config(), pre)
            .expect("upgrade from Phase 0 to Altair to should succeed");

        assert_eq!(actual_post, expected_post);
    }

    fn run_bellatrix_case<P: Preset>(case: Case) {
        let pre = case.ssz_default("pre");
        let expected_post = case.ssz_default("post");

        let actual_post = upgrade_to_bellatrix::<P>(&P::default_config(), pre);

        assert_eq!(actual_post, expected_post);
    }

    fn run_capella_case<P: Preset>(case: Case) {
        let pre = case.ssz_default("pre");
        let expected_post = case.ssz_default("post");

        let actual_post = upgrade_to_capella::<P>(&P::default_config(), pre);

        assert_eq!(actual_post, expected_post);
    }

    fn run_deneb_case<P: Preset>(case: Case) {
        let pre = case.ssz_default("pre");
        let expected_post = case.ssz_default("post");

        let actual_post = upgrade_to_deneb::<P>(&P::default_config(), pre);

        assert_eq!(actual_post, expected_post);
    }
}
