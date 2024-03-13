use core::{fmt::Display, marker::PhantomData, num::NonZeroU64};
use std::{borrow::Cow, sync::Arc};

use anyhow::{bail, ensure, Context as _, Error as AnyhowError, Result};
use arithmetic::U64Ext as _;
use database::Database;
use derive_more::Display;
use fork_choice_store::{ChainLink, Store};
use genesis::GenesisProvider;
use helper_functions::{accessors, misc};
use itertools::Itertools as _;
use log::{debug, info, warn};
use nonzero_ext::nonzero;
use reqwest::{Client, Url};
use ssz::{Ssz, SszRead, SszReadDefault as _, SszWrite};
use std_ext::ArcExt as _;
use thiserror::Error;
use transition_functions::combined;
use types::{
    combined::{BeaconState, SignedBeaconBlock},
    config::Config,
    deneb::{
        containers::{BlobIdentifier, BlobSidecar},
        primitives::BlobIndex,
    },
    nonstandard::BlobSidecarWithId,
    phase0::{
        consts::GENESIS_SLOT,
        primitives::{Epoch, Slot, H256},
    },
    preset::Preset,
    traits::{BeaconState as _, SignedBeaconBlock as _},
};

use crate::checkpoint_sync::{self, FinalizedCheckpoint};

pub const DEFAULT_ARCHIVAL_EPOCH_INTERVAL: NonZeroU64 = nonzero!(32_u64);

pub enum StateLoadStrategy<P: Preset> {
    Auto {
        state_slot: Option<Slot>,
        checkpoint_sync_url: Option<Url>,
        genesis_provider: GenesisProvider<P>,
    },
    Remote {
        checkpoint_sync_url: Url,
    },
    Anchor {
        block: Arc<SignedBeaconBlock<P>>,
        state: Arc<BeaconState<P>>,
    },
}

#[allow(clippy::struct_field_names)]
pub struct Storage<P> {
    config: Arc<Config>,
    pub(crate) database: Database,
    pub(crate) archival_epoch_interval: NonZeroU64,
    prune_storage: bool,
    phantom: PhantomData<P>,
}

impl<P: Preset> Storage<P> {
    pub fn new(
        config: Arc<Config>,
        database: Database,
        archival_epoch_interval: NonZeroU64,
        prune_storage: bool,
    ) -> Self {
        Self {
            config,
            database,
            archival_epoch_interval,
            prune_storage,
            phantom: PhantomData,
        }
    }

    /// Returns an instance that uses an in-memory database.
    ///
    /// The trait-based dependency injection used elsewhere makes it harder to select
    /// implementations at runtime.
    #[must_use]
    pub fn in_memory(config: Arc<Config>) -> Self {
        // TODO(feature/in-memory-db): Use `Storage::new`?
        Self {
            config,
            database: Database::in_memory(),
            archival_epoch_interval: DEFAULT_ARCHIVAL_EPOCH_INTERVAL,
            prune_storage: false,
            phantom: PhantomData,
        }
    }

    #[must_use]
    pub(crate) const fn config(&self) -> &Arc<Config> {
        &self.config
    }

    pub async fn load(
        &self,
        client: &Client,
        state_load_strategy: StateLoadStrategy<P>,
    ) -> Result<(StateStorage<P>, bool)> {
        let anchor_block;
        let anchor_state;
        let unfinalized_blocks: UnfinalizedBlocks<P>;
        let loaded_from_remote;

        match state_load_strategy {
            StateLoadStrategy::Auto {
                state_slot,
                checkpoint_sync_url,
                genesis_provider,
            } => 'block: {
                // Attempt to load local state first: either latest or from specified slot.
                let local_state_storage = match state_slot {
                    Some(slot) => self.load_state_by_iteration(slot)?,
                    None => self.load_latest_state()?,
                };

                if let Some(url) = checkpoint_sync_url {
                    // Do checkpoint sync only if local state is not present.
                    if local_state_storage.is_none() {
                        let result =
                            checkpoint_sync::load_finalized_from_remote(&self.config, client, &url)
                                .await
                                .context(Error::CheckpointSyncFailed);

                        match result {
                            Ok(FinalizedCheckpoint { block, state }) => {
                                anchor_block = block;
                                anchor_state = state;
                                unfinalized_blocks = Box::new(core::iter::empty());
                                loaded_from_remote = true;
                                break 'block;
                            }
                            Err(error) => warn!("{error:#}"),
                        }
                    } else {
                        warn!(
                            "skipping checkpoint sync: existing database found; \
                             pass --force-checkpoint-sync to force checkpoint sync",
                        );
                    }
                }

                match local_state_storage {
                    OptionalStateStorage::Full(state_storage) => {
                        (anchor_state, anchor_block, unfinalized_blocks) = state_storage;
                    }
                    // State might not be found but unfinalized blocks could be present.
                    OptionalStateStorage::UnfinalizedOnly(local_unfinalized_blocks) => {
                        anchor_block = genesis_provider.block();
                        anchor_state = genesis_provider.state();
                        unfinalized_blocks = local_unfinalized_blocks;
                    }
                    OptionalStateStorage::None => {
                        anchor_block = genesis_provider.block();
                        anchor_state = genesis_provider.state();
                        unfinalized_blocks = Box::new(core::iter::empty());
                    }
                }

                loaded_from_remote = false;
            }
            StateLoadStrategy::Remote {
                checkpoint_sync_url,
            } => {
                let FinalizedCheckpoint { block, state } =
                    checkpoint_sync::load_finalized_from_remote(
                        &self.config,
                        client,
                        &checkpoint_sync_url,
                    )
                    .await
                    .context(Error::CheckpointSyncFailed)?;

                anchor_block = block;
                anchor_state = state;
                unfinalized_blocks = Box::new(core::iter::empty());
                loaded_from_remote = true;
            }
            StateLoadStrategy::Anchor { block, state } => {
                anchor_block = block;
                anchor_state = state;
                unfinalized_blocks = Box::new(core::iter::empty());
                loaded_from_remote = false;
            }
        }

        let anchor_slot = anchor_block.message().slot();
        let anchor_block_root = anchor_block.message().hash_tree_root();
        let anchor_state_root = anchor_block.message().state_root();

        info!("loaded state at slot {anchor_slot}");

        self.database.put_batch([
            serialize(FinalizedBlockByRoot(anchor_block_root), &anchor_block)?,
            serialize(BlockRootBySlot(anchor_slot), anchor_block_root)?,
            serialize(SlotByStateRoot(anchor_state_root), anchor_slot)?,
            serialize(StateByBlockRoot(anchor_block_root), &anchor_state)?,
        ])?;

        let state_storage = (anchor_state, anchor_block, unfinalized_blocks);

        Ok((state_storage, loaded_from_remote))
    }

    fn load_latest_state(&self) -> Result<OptionalStateStorage<P>> {
        if let Some((state, block, blocks)) = self.load_state_and_blocks_from_checkpoint()? {
            Ok(OptionalStateStorage::Full((state, block, blocks)))
        } else {
            info!(
                "latest state checkpoint was not found; \
                 attempting to find stored state by iteration",
            );

            self.load_state_by_iteration(Slot::MAX)
        }
    }

    pub(crate) fn append<'cl>(
        &self,
        unfinalized: impl Iterator<Item = &'cl ChainLink<P>>,
        finalized: impl DoubleEndedIterator<Item = &'cl ChainLink<P>>,
        store: &Store<P>,
    ) -> Result<AppendedBlockSlots> {
        let mut slots = AppendedBlockSlots::default();
        let mut store_head_slot = 0;
        let mut checkpoint_state_appended = false;
        let mut archival_state_appended = false;
        let mut batch = vec![];

        let unfinalized = unfinalized.zip(core::iter::repeat(false));
        let finalized = finalized.rev().zip(core::iter::repeat(true));

        let mut chain = unfinalized
            .chain(finalized)
            .filter(|(chain_link, is_finalized)| *is_finalized || chain_link.is_valid())
            .peekable();

        if let Some(StateCheckpoint { head_slot, .. }) = self.load_state_checkpoint()? {
            store_head_slot = head_slot;
        }

        if let Some((chain_link, _)) = chain.peek() {
            store_head_slot = chain_link.slot().max(store_head_slot);
        }

        debug!("saving store head slot: {store_head_slot}");

        for (chain_link, finalized) in chain {
            let block_root = chain_link.block_root;
            let block = &chain_link.block;
            let state = chain_link.state(store);
            let state_slot = chain_link.slot();

            if !self.prune_storage {
                if finalized {
                    slots.finalized.push(state_slot);
                    batch.push(serialize(FinalizedBlockByRoot(block_root), block)?);
                } else {
                    slots.unfinalized.push(state_slot);
                    batch.push(serialize(UnfinalizedBlockByRoot(block_root), block)?);
                }

                batch.push(serialize(BlockRootBySlot(state_slot), block_root)?);
            }

            if finalized {
                if !self.prune_storage {
                    batch.push(serialize(
                        SlotByStateRoot(block.message().state_root()),
                        state_slot,
                    )?);
                }

                if !checkpoint_state_appended {
                    let append_state = misc::is_epoch_start::<P>(state_slot);

                    if append_state {
                        info!("saving checkpoint block & state in slot {state_slot}");

                        batch.push(serialize(
                            BlockCheckpoint::<P>::KEY,
                            BlockCheckpoint {
                                block: block.clone_arc(),
                            },
                        )?);

                        batch.push(serialize(
                            StateCheckpoint::<P>::KEY,
                            StateCheckpoint {
                                block_root,
                                head_slot: store_head_slot,
                                state: state.clone_arc(),
                            },
                        )?);

                        checkpoint_state_appended = true;
                    }
                }

                if !(archival_state_appended || self.prune_storage) {
                    let state_epoch = Self::epoch_at_slot(state_slot);
                    let append_state = misc::is_epoch_start::<P>(state_slot)
                        && state_epoch.is_multiple_of(self.archival_epoch_interval);

                    if append_state {
                        info!("saving state in slot {state_slot}");

                        batch.push(serialize(StateByBlockRoot(block_root), state)?);

                        archival_state_appended = true;
                    }
                }
            }
        }

        self.database.put_batch(batch)?;

        Ok(slots)
    }

    pub(crate) fn append_blob_sidecars(
        &self,
        blob_sidecars: impl IntoIterator<Item = BlobSidecarWithId<P>>,
    ) -> Result<Vec<BlobIdentifier>> {
        let mut batch = vec![];
        let mut persisted_blob_ids = vec![];

        for blob_sidecar_with_id in blob_sidecars {
            let BlobSidecarWithId {
                blob_sidecar,
                blob_id,
            } = blob_sidecar_with_id;

            let BlobIdentifier { block_root, index } = blob_id;

            let slot = blob_sidecar.signed_block_header.message.slot;

            batch.push(serialize(
                BlobSidecarByBlobId(block_root, index),
                blob_sidecar,
            )?);

            batch.push(serialize(SlotBlobId(slot, block_root, index), blob_id)?);

            persisted_blob_ids.push(blob_id);
        }

        self.database.put_batch(batch)?;

        Ok(persisted_blob_ids)
    }

    pub(crate) fn blob_sidecar_by_id(
        &self,
        blob_id: BlobIdentifier,
    ) -> Result<Option<Arc<BlobSidecar<P>>>> {
        let BlobIdentifier { block_root, index } = blob_id;

        self.get(BlobSidecarByBlobId(block_root, index))
    }

    pub(crate) fn prune_old_blob_sidecars(&self, up_to_slot: Slot) -> Result<()> {
        let mut blobs_to_remove: Vec<BlobIdentifier> = vec![];
        let mut keys_to_remove = vec![];

        let results = self
            .database
            .iterator_descending(..=SlotBlobId(up_to_slot, H256::zero(), 0).to_string())?;

        for result in results {
            let (key_bytes, value_bytes) = result?;

            if !SlotBlobId::has_prefix(&key_bytes) {
                break;
            }

            // Deserialize-serialize BlobIdentifier as an additional measure
            // to prevent other types of data getting accidentally deleted.
            blobs_to_remove.push(BlobIdentifier::from_ssz_default(value_bytes)?);
            keys_to_remove.push(key_bytes);
        }

        for blob_id in blobs_to_remove {
            self.database.delete(blob_id.to_ssz()?)?;
        }

        for key in keys_to_remove {
            self.database.delete(key)?;
        }

        Ok(())
    }

    pub(crate) fn checkpoint_state_slot(&self) -> Result<Option<Slot>> {
        if let Some(StateCheckpoint { head_slot, .. }) = self.load_state_checkpoint()? {
            return Ok(Some(head_slot));
        }

        Ok(None)
    }

    pub(crate) fn genesis_block_root(&self, store: &Store<P>) -> Result<H256> {
        self.block_root_by_slot_with_store(store, GENESIS_SLOT)?
            .ok_or(Error::GenesisBlockRootNotFound)
            .map_err(Into::into)
    }

    pub(crate) fn contains_finalized_block(&self, block_root: H256) -> Result<bool> {
        self.contains_key(FinalizedBlockByRoot(block_root))
    }

    pub(crate) fn contains_unfinalized_block(&self, block_root: H256) -> Result<bool> {
        self.contains_key(UnfinalizedBlockByRoot(block_root))
    }

    pub(crate) fn finalized_block_by_root(
        &self,
        block_root: H256,
    ) -> Result<Option<Arc<SignedBeaconBlock<P>>>> {
        self.get(FinalizedBlockByRoot(block_root))
    }

    pub(crate) fn unfinalized_block_by_root(
        &self,
        block_root: H256,
    ) -> Result<Option<Arc<SignedBeaconBlock<P>>>> {
        self.get(UnfinalizedBlockByRoot(block_root))
    }

    pub(crate) fn block_root_by_slot(&self, slot: Slot) -> Result<Option<H256>> {
        self.get(BlockRootBySlot(slot))
    }

    fn state_by_block_root(&self, block_root: H256) -> Result<Option<Arc<BeaconState<P>>>> {
        self.get(StateByBlockRoot(block_root))
    }

    pub(crate) fn slot_by_state_root(&self, state_root: H256) -> Result<Option<Slot>> {
        self.get(SlotByStateRoot(state_root))
    }

    // Like `block_root_by_slot`, but looks for the root in `store` first.
    pub(crate) fn block_root_by_slot_with_store(
        &self,
        store: &Store<P>,
        slot: Slot,
    ) -> Result<Option<H256>> {
        if let Some(chain_link) = store.chain_link_before_or_at(slot) {
            let slot_matches = chain_link.slot() == slot;
            return Ok(slot_matches.then_some(chain_link.block_root));
        }

        self.block_root_by_slot(slot)
    }

    // TODO(feature/in-memory-db): This should look up unfinalized blocks too.
    pub(crate) fn block_by_slot(
        &self,
        slot: Slot,
    ) -> Result<Option<(Arc<SignedBeaconBlock<P>>, H256)>> {
        let Some(block_root) = self.block_root_by_slot(slot)? else {
            return Ok(None);
        };

        let Some(block) = self.finalized_block_by_root(block_root)? else {
            return Ok(None);
        };

        Ok(Some((block, block_root)))
    }

    pub(crate) fn stored_state(&self, slot: Slot) -> Result<Option<Arc<BeaconState<P>>>> {
        let (mut state, state_block, blocks) = match self.load_state_by_iteration(slot)? {
            OptionalStateStorage::None | OptionalStateStorage::UnfinalizedOnly(_) => {
                return Ok(None)
            }
            OptionalStateStorage::Full(state_storage) => state_storage,
        };

        state.set_cached_root(state_block.message().state_root());

        // State may be persisted only once in several epochs.
        // `blocks` here are needed to transition state closer to `slot`.
        for result in blocks.rev() {
            let block = result?;
            combined::trusted_state_transition(&self.config, state.make_mut(), &block)?;
        }

        if state.slot() < slot {
            combined::process_slots(&self.config, state.make_mut(), slot)?;
        }

        Ok(Some(state))
    }

    // TODO(feature/in-memory-db): Rename this or other methods to match.
    pub(crate) fn preprocessed_state_post_block(
        &self,
        mut block_root: H256,
        slot: Slot,
    ) -> Result<Option<Arc<BeaconState<P>>>> {
        let mut blocks = vec![];

        let mut state = loop {
            if let Some(state) = self.state_by_block_root(block_root)? {
                let slot = state.slot();

                ensure!(
                    misc::is_epoch_start::<P>(slot),
                    Error::PersistedSlotCannotContainAnchor { slot },
                );

                break state;
            }

            if let Some(block) = self.finalized_block_by_root(block_root)? {
                block_root = block.message().parent_root();
                blocks.push(block);
                continue;
            }

            if let Some(block) = self.unfinalized_block_by_root(block_root)? {
                block_root = block.message().parent_root();
                blocks.push(block);
                continue;
            }

            return Ok(None);
        };

        for block in blocks.into_iter().rev() {
            combined::trusted_state_transition(&self.config, state.make_mut(), &block)?;
        }

        // TODO(feature/in-memory-db): Limit slot processing to `max_empty_slots`.
        //                             Consider moving slot processing out of this method.
        if state.slot() < slot {
            combined::process_slots(&self.config, state.make_mut(), slot)?;
        }

        Ok(Some(state))
    }

    pub(crate) fn stored_state_by_state_root(
        &self,
        state_root: H256,
    ) -> Result<Option<Arc<BeaconState<P>>>> {
        if let Some(state_slot) = self.slot_by_state_root(state_root)? {
            return self.stored_state(state_slot);
        }

        Ok(None)
    }

    pub(crate) fn dependent_root(
        &self,
        store: &Store<P>,
        state: &BeaconState<P>,
        epoch: Epoch,
    ) -> Result<H256> {
        let start_slot = misc::compute_start_slot_at_epoch::<P>(epoch);

        match start_slot.checked_sub(1) {
            Some(root_slot) => accessors::get_block_root_at_slot(state, root_slot),
            None => self.genesis_block_root(store),
        }
        .context(Error::DependentRootLookupFailed)
    }

    fn load_state_and_blocks_from_checkpoint(&self) -> Result<Option<StateStorage<P>>> {
        if let Some(checkpoint) = self.load_state_checkpoint()? {
            let StateCheckpoint {
                block_root, state, ..
            } = checkpoint;

            let block = if let Some(block_checkpoint) = self.load_block_checkpoint()? {
                let BlockCheckpoint { block } = block_checkpoint;
                let requested = block_root;
                let computed = block.message().hash_tree_root();

                ensure!(
                    requested == computed,
                    Error::CheckpointBlockRootMismatch {
                        requested,
                        computed,
                    },
                );

                block
            } else {
                self.finalized_block_by_root(block_root)?
                    .ok_or(Error::BlockNotFound { block_root })?
            };

            ensure!(
                misc::is_epoch_start::<P>(state.slot()),
                Error::PersistedSlotCannotContainAnchor { slot: state.slot() },
            );

            let results = self
                .database
                .iterator_ascending(BlockRootBySlot(state.slot() + 1).to_string()..)?;

            let block_roots = itertools::process_results(results, |pairs| {
                pairs
                    .take_while(|(key_bytes, _)| BlockRootBySlot::has_prefix(key_bytes))
                    .map(|(_, value_bytes)| H256::from_ssz_default(value_bytes))
                    .try_collect()
            })??;

            let blocks = self.blocks_by_roots(block_roots);

            return Ok(Some((state, block, blocks)));
        }

        Ok(None)
    }

    fn load_state_by_iteration(&self, start_from_slot: Slot) -> Result<OptionalStateStorage<P>> {
        let results = self
            .database
            .iterator_descending(..=BlockRootBySlot(start_from_slot).to_string())?;

        let mut block_roots = vec![];

        for result in results {
            let (key_bytes, value_bytes) = result?;

            if !BlockRootBySlot::has_prefix(&key_bytes) {
                break;
            }

            let block_root = H256::from_ssz_default(value_bytes)?;

            if let Some(state) = self.state_by_block_root(block_root)? {
                let slot = state.slot();

                ensure!(
                    misc::is_epoch_start::<P>(slot),
                    Error::PersistedSlotCannotContainAnchor { slot },
                );

                let block = self
                    .finalized_block_by_root(block_root)?
                    .ok_or(Error::BlockNotFound { block_root })?;

                let blocks = self.blocks_by_roots(block_roots);

                return Ok(OptionalStateStorage::Full((state, block, blocks)));
            }

            block_roots.push(block_root);
        }

        if block_roots.is_empty() {
            return Ok(OptionalStateStorage::None);
        }

        Ok(OptionalStateStorage::UnfinalizedOnly(
            self.blocks_by_roots(block_roots),
        ))
    }

    fn load_block_checkpoint(&self) -> Result<Option<BlockCheckpoint<P>>> {
        self.get(BlockCheckpoint::<P>::KEY)
    }

    fn load_state_checkpoint(&self) -> Result<Option<StateCheckpoint<P>>> {
        self.get(StateCheckpoint::<P>::KEY)
    }

    fn contains_key(&self, key: impl Display) -> Result<bool> {
        let key_string = key.to_string();

        self.database.contains_key(key_string)
    }

    fn get<V: SszRead<Config>>(&self, key: impl Display) -> Result<Option<V>> {
        let key_string = key.to_string();

        if let Some(value_bytes) = self.database.get(key_string)? {
            let value = V::from_ssz(&self.config, value_bytes)?;
            return Ok(Some(value));
        }

        Ok(None)
    }

    fn blocks_by_roots(&self, block_roots: Vec<H256>) -> UnfinalizedBlocks<P> {
        Box::new(block_roots.into_iter().map(|block_root| {
            if let Some(block) = self.finalized_block_by_root(block_root)? {
                return Ok(block);
            }

            if let Some(block) = self.unfinalized_block_by_root(block_root)? {
                return Ok(block);
            }

            bail!(Error::BlockNotFound { block_root })
        }))
    }

    pub(crate) fn epoch_at_slot(slot: Slot) -> Epoch {
        misc::compute_epoch_at_slot::<P>(slot)
    }
}

#[cfg(test)]
impl<P: Preset> Storage<P> {
    pub fn finalized_block_count(&self) -> Result<usize> {
        let results = self
            .database
            .iterator_ascending(FinalizedBlockByRoot(H256::zero()).to_string()..)?;

        itertools::process_results(results, |pairs| {
            pairs
                .take_while(|(key_bytes, _)| FinalizedBlockByRoot::has_prefix(key_bytes))
                .count()
        })
    }
}

#[derive(Default, Debug)]
pub struct AppendedBlockSlots {
    pub finalized: Vec<Slot>,
    pub unfinalized: Vec<Slot>,
}

type UnfinalizedBlocks<'storage, P> =
    Box<dyn DoubleEndedIterator<Item = Result<Arc<SignedBeaconBlock<P>>>> + Send + 'storage>;

// Internal type for state storage that can be missing or have missing elements.
// E.g. non-finalized storage that has only unfinalized blocks stored.
enum OptionalStateStorage<'storage, P: Preset> {
    None,
    UnfinalizedOnly(UnfinalizedBlocks<'storage, P>),
    Full(StateStorage<'storage, P>),
}

impl<P: Preset> OptionalStateStorage<'_, P> {
    const fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

type StateStorage<'storage, P> = (
    Arc<BeaconState<P>>,
    Arc<SignedBeaconBlock<P>>,
    UnfinalizedBlocks<'storage, P>,
);

#[derive(Ssz)]
// A `bound_for_read` attribute like this must be added when deriving `SszRead` for any type that
// contains a block or state. The name of the `C` type parameter is hardcoded in `ssz_derive`.
#[ssz(bound_for_read = "BeaconState<P>: SszRead<C>", derive_hash = false)]
struct StateCheckpoint<P: Preset> {
    block_root: H256,
    head_slot: Slot,
    state: Arc<BeaconState<P>>,
}

impl<P: Preset> StateCheckpoint<P> {
    // This was renamed from `cstate` for compatibility with old schema versions.
    const KEY: &'static str = "cstate2";
}

#[derive(Ssz)]
// A `bound_for_read` attribute like this must be added when deriving `SszRead` for any type that
// contains a block or state. The name of the `C` type parameter is hardcoded in `ssz_derive`.
#[ssz(
    bound_for_read = "SignedBeaconBlock<P>: SszRead<C>",
    derive_hash = false,
    transparent
)]
struct BlockCheckpoint<P: Preset> {
    block: Arc<SignedBeaconBlock<P>>,
}

impl<P: Preset> BlockCheckpoint<P> {
    const KEY: &'static str = "cblock";
}

#[derive(Display)]
#[display(fmt = "{}{_0:020}", Self::PREFIX)]
pub struct BlockRootBySlot(pub Slot);

impl TryFrom<Cow<'_, [u8]>> for BlockRootBySlot {
    type Error = AnyhowError;

    fn try_from(bytes: Cow<[u8]>) -> Result<Self> {
        let payload =
            bytes
                .strip_prefix(Self::PREFIX.as_bytes())
                .ok_or_else(|| Error::IncorrectPrefix {
                    bytes: bytes.to_vec(),
                })?;

        let string = core::str::from_utf8(payload)?;
        let slot = string.parse()?;

        Ok(Self(slot))
    }
}

impl BlockRootBySlot {
    const PREFIX: &'static str = "r";

    fn has_prefix(bytes: &[u8]) -> bool {
        bytes.starts_with(Self::PREFIX.as_bytes())
    }
}

#[derive(Display)]
#[display(fmt = "{}{_0:x}", Self::PREFIX)]
pub struct FinalizedBlockByRoot(pub H256);

impl FinalizedBlockByRoot {
    const PREFIX: &'static str = "b";

    #[cfg(test)]
    fn has_prefix(bytes: &[u8]) -> bool {
        bytes.starts_with(Self::PREFIX.as_bytes())
    }
}

#[derive(Display)]
#[display(fmt = "{}{_0:x}", Self::PREFIX)]
pub struct UnfinalizedBlockByRoot(pub H256);

impl UnfinalizedBlockByRoot {
    const PREFIX: &'static str = "b_nf";
}

#[derive(Display)]
#[display(fmt = "{}{_0:x}", Self::PREFIX)]
pub struct StateByBlockRoot(pub H256);

impl StateByBlockRoot {
    const PREFIX: &'static str = "s";
}

#[derive(Display)]
#[display(fmt = "{}{_0:x}", Self::PREFIX)]
pub struct SlotByStateRoot(pub H256);

impl SlotByStateRoot {
    const PREFIX: &'static str = "t";
}

#[derive(Display)]
#[display(fmt = "{}{_0:x}{_1}", Self::PREFIX)]
pub struct BlobSidecarByBlobId(pub H256, pub BlobIndex);

impl BlobSidecarByBlobId {
    const PREFIX: &'static str = "o";
}

#[derive(Display)]
#[display(fmt = "{}{_0:020}{_1:x}{_2}", Self::PREFIX)]
pub struct SlotBlobId(pub Slot, pub H256, pub BlobIndex);

impl SlotBlobId {
    const PREFIX: &'static str = "i";

    fn has_prefix(bytes: &[u8]) -> bool {
        bytes.starts_with(Self::PREFIX.as_bytes())
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("checkpoint sync failed")]
    CheckpointSyncFailed,
    #[error("failed to look up dependent root")]
    DependentRootLookupFailed,
    #[error("genesis block root not found in storage")]
    GenesisBlockRootNotFound,
    #[error("block not found in storage: {block_root:?}")]
    BlockNotFound { block_root: H256 },
    #[error("state not found in storage: {state_slot}")]
    StateNotFound { state_slot: Slot },
    #[error(
        "checkpoint block root does not match state checkpoint \
         (requested: {requested:?}, computed: {computed:?})"
    )]
    CheckpointBlockRootMismatch { requested: H256, computed: H256 },
    #[error("persisted slot cannot contain anchor: {slot}")]
    PersistedSlotCannotContainAnchor { slot: Slot },
    #[error("storage key has incorrect prefix: {bytes:?}")]
    IncorrectPrefix { bytes: Vec<u8> },
}

pub fn serialize(key: impl Display, value: impl SszWrite) -> Result<(String, Vec<u8>)> {
    Ok((key.to_string(), value.to_ssz()?))
}
