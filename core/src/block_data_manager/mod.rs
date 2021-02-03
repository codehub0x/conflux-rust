// Copyright 2019 Conflux Foundation. All rights reserved.
// Conflux is free software and distributed under GNU General Public License.
// See http://www.gnu.org/licenses/

use crate::{
    cache_config::CacheConfig,
    cache_manager::{CacheId, CacheManager, CacheSize},
    ext_db::SystemDB,
    pow::{PowComputer, TargetDifficultyManager},
};
use cfx_storage::{
    state_manager::StateIndex, utils::guarded_value::*, StorageManager,
    StorageManagerTrait, StorageStateTrait,
};
use cfx_types::{Bloom, H256};
use malloc_size_of::{new_malloc_size_ops, MallocSizeOf, MallocSizeOfOps};
use malloc_size_of_derive::MallocSizeOf as DeriveMallocSizeOf;
use parking_lot::{Mutex, RwLock, RwLockReadGuard, RwLockUpgradableReadGuard};
use primitives::{
    block::CompactBlock,
    receipt::{
        BlockReceipts, TRANSACTION_OUTCOME_EXCEPTION_WITH_NONCE_BUMPING,
        TRANSACTION_OUTCOME_SUCCESS,
    },
    Block, BlockHeader, EpochId, SignedTransaction, TransactionIndex,
    TransactionWithSignature, NULL_EPOCH,
};
use rlp::DecoderError;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use threadpool::ThreadPool;
pub mod block_data_types;
pub mod db_gc_manager;
pub mod db_manager;
pub mod tx_data_manager;
use crate::{
    block_data_manager::{
        db_manager::DBManager, tx_data_manager::TransactionDataManager,
    },
    trace::trace::BlockExecTraces,
};
pub use block_data_types::*;
use cfx_internal_common::{
    EpochExecutionCommitment, StateAvailabilityBoundary, StateRootWithAuxInfo,
};
use db_gc_manager::GCProgress;
use metrics::{register_meter_with_group, Meter, MeterTimer};
use std::{hash::Hash, path::Path, time::Duration};

lazy_static! {
    static ref TX_POOL_RECOVER_TIMER: Arc<dyn Meter> =
        register_meter_with_group("timer", "tx_pool::recover_public");
}

pub const NULLU64: u64 = !0;

#[derive(DeriveMallocSizeOf)]
pub struct InvalidBlockSet {
    capacity: usize,
    invalid_block_hashes: HashSet<H256>,
}

impl InvalidBlockSet {
    pub fn new(capacity: usize) -> Self {
        InvalidBlockSet {
            capacity,
            invalid_block_hashes: HashSet::new(),
        }
    }

    pub fn insert(&mut self, value: H256) {
        if !self.invalid_block_hashes.contains(&value) {
            if self.invalid_block_hashes.len() < self.capacity {
                self.invalid_block_hashes.insert(value);
                return;
            }

            let mut iter = self.invalid_block_hashes.iter();
            let the_evicted = iter.next().map(|e| e.clone());
            if let Some(evicted) = the_evicted {
                self.invalid_block_hashes.remove(&evicted);
            }
            self.invalid_block_hashes.insert(value);
        }
    }

    pub fn contains(&self, value: &H256) -> bool {
        self.invalid_block_hashes.contains(value)
    }
}

#[derive(DeriveMallocSizeOf)]
pub struct BlockDataManager {
    block_headers: RwLock<HashMap<H256, Arc<BlockHeader>>>,
    blocks: RwLock<HashMap<H256, Arc<Block>>>,
    compact_blocks: RwLock<HashMap<H256, CompactBlock>>,
    block_receipts: RwLock<HashMap<H256, BlockReceiptsInfo>>,
    block_rewards: RwLock<HashMap<H256, BlockRewardResult>>,
    block_traces: RwLock<HashMap<H256, BlockExecTraces>>,
    transaction_indices: RwLock<HashMap<H256, TransactionIndex>>,
    local_block_info: RwLock<HashMap<H256, LocalBlockInfo>>,
    blamed_header_verified_roots:
        RwLock<HashMap<u64, BlamedHeaderVerifiedRoots>>,
    /// Caching for receipts_root and logs_bloom for epochs after
    /// cur_era_genesis. It is not deferred, i.e., indexed by the hash of
    /// the pivot block that produces the result when executed.
    /// It is also used for checking whether an epoch has been executed.
    /// It can be updated, i.e., adding new items, in the following cases:
    /// 1) When a new epoch gets executed in normal execution;
    /// 2) After syncing snapshot, we need to update execution commitment
    ///    for pivot blocks around snapshot block based on blaming information;
    /// 3) After recovering block graph from db, update execution commitment
    ///    from db;
    /// 4) In BlockDataManager::new(), update execution commitment
    ///    of true_genesis_block.
    epoch_execution_commitments:
        RwLock<HashMap<H256, EpochExecutionCommitment>>,
    epoch_execution_contexts: RwLock<HashMap<H256, EpochExecutionContext>>,

    invalid_block_set: RwLock<InvalidBlockSet>,
    cur_consensus_era_genesis_hash: RwLock<H256>,
    cur_consensus_era_stable_hash: RwLock<H256>,
    instance_id: Mutex<u64>,

    config: DataManagerConfiguration,

    tx_data_manager: TransactionDataManager,
    pub db_manager: DBManager,

    // TODO Add MallocSizeOf.
    #[ignore_malloc_size_of = "Add later"]
    pub pow: Arc<PowComputer>,

    /// This is the original genesis block.
    pub true_genesis: Arc<Block>,
    pub storage_manager: Arc<StorageManager>,
    cache_man: Arc<Mutex<CacheManager<CacheId>>>,
    pub target_difficulty_manager: TargetDifficultyManager,
    gc_progress: Arc<Mutex<GCProgress>>,

    /// This maintains the boundary height of available state and commitments
    /// (executed but not deleted or in `ExecutionTaskQueue`).
    /// The upper bound always equal to latest executed epoch height.
    /// As for the lower bound:
    ///   1. For archive node, it always equals `cur_era_stable_height`.
    ///   2. For full node, it equals the height of remotely synchronized
    ///      state at start, and equals `cur_era_stable_height` after making a
    ///      new checkpoint.
    ///
    /// The lower boundary height will be updated when:
    ///   1. New checkpoint
    ///   2. Full Node snapshot syncing
    ///   3. New Snapshot
    ///
    /// The upper boundary height will be updated when:
    ///   1. Pivot chain switch
    ///   2. Execution of new epoch
    ///
    /// The state of an epoch is valid if and only if the height of the epoch
    /// is inside the boundary.
    pub state_availability_boundary: RwLock<StateAvailabilityBoundary>,
}

impl BlockDataManager {
    pub fn new(
        cache_conf: CacheConfig, true_genesis: Arc<Block>, db: Arc<SystemDB>,
        storage_manager: Arc<StorageManager>,
        worker_pool: Arc<Mutex<ThreadPool>>, config: DataManagerConfiguration,
        pow: Arc<PowComputer>,
    ) -> Self
    {
        let mb = 1024 * 1024;
        let max_cache_size = cache_conf.ledger_mb() * mb;
        let pref_cache_size = max_cache_size * 3 / 4;
        let cache_man = Arc::new(Mutex::new(CacheManager::new(
            pref_cache_size,
            max_cache_size,
            3 * mb,
        )));
        let tx_data_manager = TransactionDataManager::new(
            config.tx_cache_index_maintain_timeout,
            worker_pool,
        );
        let db_manager = match config.db_type {
            DbType::Rocksdb => DBManager::new_from_rocksdb(db, pow.clone()),
            DbType::Sqlite => DBManager::new_from_sqlite(
                Path::new("./sqlite_db"),
                pow.clone(),
            ),
        };

        let data_man = Self {
            block_headers: RwLock::new(HashMap::new()),
            blocks: RwLock::new(HashMap::new()),
            compact_blocks: Default::default(),
            block_receipts: Default::default(),
            block_rewards: Default::default(),
            block_traces: Default::default(),
            transaction_indices: Default::default(),
            local_block_info: Default::default(),
            blamed_header_verified_roots: Default::default(),
            epoch_execution_commitments: Default::default(),
            epoch_execution_contexts: Default::default(),
            invalid_block_set: RwLock::new(InvalidBlockSet::new(
                cache_conf.invalid_block_hashes_cache_size_in_count,
            )),
            true_genesis: true_genesis.clone(),
            storage_manager,
            cache_man,
            instance_id: Mutex::new(0),
            config,
            target_difficulty_manager: TargetDifficultyManager::new(
                cache_conf.target_difficulties_cache_size_in_count,
            ),
            cur_consensus_era_genesis_hash: RwLock::new(true_genesis.hash()),
            cur_consensus_era_stable_hash: RwLock::new(true_genesis.hash()),
            tx_data_manager,
            db_manager,
            pow,
            state_availability_boundary: RwLock::new(
                StateAvailabilityBoundary::new(true_genesis.hash(), 0),
            ),
            gc_progress: Default::default(),
        };

        data_man.initialize_instance_id();

        let cur_era_genesis_hash =
            match data_man.db_manager.checkpoint_hashes_from_db() {
                None => true_genesis.hash(),
                Some((checkpoint_hash, stable_hash)) => {
                    *data_man.cur_consensus_era_genesis_hash.write() =
                        checkpoint_hash;
                    *data_man.cur_consensus_era_stable_hash.write() =
                        stable_hash;
                    checkpoint_hash
                }
            };

        debug!(
            "BlockDataManager::new() cur_era_genesis_hash: {:?}",
            &cur_era_genesis_hash
        );

        if cur_era_genesis_hash == data_man.true_genesis.hash() {
            // Only insert block body for true genesis
            data_man.insert_block(
                data_man.true_genesis.clone(),
                true, /* persistent */
            );
            for (index, tx) in
                data_man.true_genesis.transactions.iter().enumerate()
            {
                data_man.insert_transaction_index(
                    &tx.hash,
                    &TransactionIndex {
                        block_hash: cur_era_genesis_hash,
                        index,
                    },
                );
            }
            // Initialize ExecutionContext for true genesis
            data_man.insert_epoch_execution_context(
                cur_era_genesis_hash,
                EpochExecutionContext {
                    start_block_number: 0,
                },
                true,
            );
            // persist local_block_info for true genesis
            data_man.db_manager.insert_local_block_info_to_db(
                &data_man.true_genesis.hash(),
                &LocalBlockInfo::new(
                    BlockStatus::Valid,
                    0,
                    data_man.get_instance_id(),
                ),
            );
            data_man.insert_epoch_execution_commitment(
                data_man.true_genesis.hash(),
                data_man.true_genesis_state_root(),
                *data_man.true_genesis.block_header.deferred_receipts_root(),
                *data_man
                    .true_genesis
                    .block_header
                    .deferred_logs_bloom_hash(),
            );
        } else {
            // Recover ExecutionContext for cur_era_genesis from db
            data_man.insert_epoch_execution_context(
                cur_era_genesis_hash,
                data_man
                    .get_epoch_execution_context(&cur_era_genesis_hash)
                    .expect("ExecutionContext exists for cur_era_genesis"),
                false, /* Not persistent because it's already in db */
            );
            // for other era genesis, we need to change the instance_id
            if let Some(mut local_block_info) = data_man
                .db_manager
                .local_block_info_from_db(&cur_era_genesis_hash)
            {
                local_block_info.instance_id = data_man.get_instance_id();
                data_man.db_manager.insert_local_block_info_to_db(
                    &cur_era_genesis_hash,
                    &local_block_info,
                );
            }
            // The commitments of cur_era_genesis will be recovered in
            // `construct_pivot_state` with other epochs
        }

        data_man
    }

    pub fn get_instance_id(&self) -> u64 { *self.instance_id.lock() }

    pub fn initialize_instance_id(&self) {
        let mut my_instance_id = self.instance_id.lock();
        if *my_instance_id == 0 {
            // load last instance id
            let instance_id = self.db_manager.instance_id_from_db();

            // set new instance id
            if let Some(instance_id) = instance_id {
                *my_instance_id = instance_id + 1;
            }
        } else {
            // This case will only happen when full node begins to sync block
            // bodies. And we should change the instance_id of genesis block to
            // current one.
            *my_instance_id += 1;
            if let Some(mut local_block_info) =
                self.db_manager.local_block_info_from_db(
                    &self.get_cur_consensus_era_genesis_hash(),
                )
            {
                local_block_info.instance_id = *my_instance_id;
                self.db_manager.insert_local_block_info_to_db(
                    &self.get_cur_consensus_era_genesis_hash(),
                    &local_block_info,
                );
            }
        }

        // persist new instance id
        self.db_manager.insert_instance_id_to_db(*my_instance_id);
    }

    /// This will return the state root of true genesis block.
    pub fn true_genesis_state_root(&self) -> StateRootWithAuxInfo {
        let true_genesis_hash = self.true_genesis.hash();
        self.storage_manager
            .get_state_no_commit(
                StateIndex::new_for_readonly(
                    &true_genesis_hash,
                    &StateRootWithAuxInfo::genesis(&true_genesis_hash),
                ),
                /* try_open = */ false,
            )
            .unwrap()
            .unwrap()
            .get_state_root()
            .unwrap()
    }

    pub fn transaction_by_hash(
        &self, hash: &H256,
    ) -> Option<Arc<SignedTransaction>> {
        let tx_index = self
            .transaction_index_by_hash(hash, false /* update_cache */)?;
        let block = self.block_by_hash(
            &tx_index.block_hash,
            false, /* update_cache */
        )?;
        assert!(tx_index.index < block.transactions.len());
        Some(block.transactions[tx_index.index].clone())
    }

    /// insert block body in memory cache and db
    pub fn insert_block_body(
        &self, hash: H256, block: Arc<Block>, persistent: bool,
    ) {
        if persistent {
            self.db_manager.insert_block_body_to_db(block.as_ref());
        }
        self.cache_man.lock().note_used(CacheId::Block(hash));
        self.blocks.write().insert(hash, block);
    }

    /// remove block body in memory cache and db
    pub fn remove_block_body(&self, hash: &H256, remove_db: bool) {
        if remove_db {
            self.db_manager.remove_block_body_from_db(hash);
        }
        self.blocks.write().remove(hash);
    }

    /// TODO Also set block header
    pub fn block_by_hash(
        &self, hash: &H256, update_cache: bool,
    ) -> Option<Arc<Block>> {
        self.get(
            hash,
            &self.blocks,
            |key| self.db_manager.block_from_db(key).map(Arc::new),
            if update_cache {
                Some(CacheId::Block(*hash))
            } else {
                None
            },
        )
    }

    /// This function returns the block from db without wrapping it in `Arc`.
    pub fn block_from_db(&self, hash: &H256) -> Option<Block> {
        self.db_manager.block_from_db(hash)
    }

    pub fn blocks_by_hash_list(
        &self, hashes: &Vec<H256>, update_cache: bool,
    ) -> Option<Vec<Arc<Block>>> {
        let mut blocks = Vec::new();
        for h in hashes {
            blocks.push(self.block_by_hash(h, update_cache)?);
        }
        Some(blocks)
    }

    /// insert block/header into memory cache, block/header into db
    pub fn insert_block(&self, block: Arc<Block>, persistent: bool) {
        let hash = block.hash();
        self.insert_block_header(
            hash,
            Arc::new(block.block_header.clone()),
            persistent,
        );
        self.insert_block_body(hash, block, persistent);
    }

    /// Remove block body and block header in memory cache and db.
    /// This is used to delete invalid blocks or dangling blocks never connected
    /// to the pivot chain.
    pub fn remove_useless_block(&self, hash: &H256, remove_db: bool) {
        // If a block has entered consensus before, it is a part of the
        // blockchain and we should not remove it here.
        if self
            .local_block_info_by_hash(hash)
            .map(|info| info.get_status() == BlockStatus::Invalid)
            .unwrap_or(true)
        {
            self.remove_block_header(hash, remove_db);
            self.remove_block_body(hash, remove_db);
        }
    }

    pub fn block_traces_by_hash(&self, hash: &H256) -> Option<BlockExecTraces> {
        self.get(
            hash,
            &self.block_traces,
            |key| self.db_manager.block_traces_from_db(key),
            Some(CacheId::BlockTraces(*hash)),
        )
    }

    pub fn insert_block_traces(
        &self, hash: H256, block_traces: BlockExecTraces, persistent: bool,
    ) {
        self.insert(
            hash,
            block_traces,
            &self.block_traces,
            |_, value| self.db_manager.insert_block_traces_to_db(&hash, value),
            Some(CacheId::BlockTraces(hash)),
            persistent,
        )
    }

    /// remove block traces in memory cache and db
    pub fn remove_block_traces(&self, hash: &H256, remove_db: bool) {
        if remove_db {
            self.db_manager.remove_block_header_from_db(hash);
        }
        self.block_headers.write().remove(hash);
    }

    pub fn block_header_by_hash(
        &self, hash: &H256,
    ) -> Option<Arc<BlockHeader>> {
        self.get(
            hash,
            &self.block_headers,
            |key| self.db_manager.block_header_from_db(key).map(Arc::new),
            Some(CacheId::BlockHeader(*hash)),
        )
    }

    pub fn insert_block_header(
        &self, hash: H256, header: Arc<BlockHeader>, persistent: bool,
    ) {
        self.insert(
            hash,
            header,
            &self.block_headers,
            |_, value| {
                self.db_manager.insert_block_header_to_db(value.as_ref())
            },
            Some(CacheId::BlockHeader(hash)),
            persistent,
        )
    }

    /// remove block header in memory cache and db
    pub fn remove_block_header(&self, hash: &H256, remove_db: bool) {
        if remove_db {
            self.db_manager.remove_block_header_from_db(hash);
        }
        self.block_headers.write().remove(hash);
    }

    pub fn block_height_by_hash(&self, hash: &H256) -> Option<u64> {
        let result = self.block_header_by_hash(hash)?;
        Some(result.height())
    }

    pub fn compact_block_by_hash(&self, hash: &H256) -> Option<CompactBlock> {
        self.compact_blocks.read().get(hash).map(|b| {
            self.cache_man
                .lock()
                .note_used(CacheId::CompactBlock(b.hash()));
            b.clone()
        })
    }

    pub fn insert_compact_block(&self, cb: CompactBlock) {
        let hash = cb.hash();
        self.compact_blocks.write().insert(hash, cb);
        self.cache_man.lock().note_used(CacheId::CompactBlock(hash));
    }

    pub fn contains_compact_block(&self, hash: &H256) -> bool {
        self.compact_blocks.read().contains_key(hash)
    }

    /// Return None if receipts for corresponding epoch is not computed before
    /// or has been overwritten by another new pivot chain in db.
    /// If `update_pivot_assumption` is true and we have execution results of
    /// `assumed_epoch` in memory, we will also ensure `assumed_epoch`
    /// is persisted as the pivot hash in db.
    ///
    /// This function will require lock of block_receipts.
    pub fn block_execution_result_by_hash_with_epoch(
        &self, hash: &H256, assumed_epoch: &H256,
        update_pivot_assumption: bool, update_cache: bool,
    ) -> Option<BlockExecutionResult>
    {
        if let Some((receipts, is_on_pivot)) = self
            .block_receipts
            .write()
            .get_mut(hash)
            .and_then(|receipt_info| {
                let r = receipt_info.get_receipts_at_epoch(assumed_epoch);
                if update_pivot_assumption {
                    receipt_info.set_pivot_hash(*assumed_epoch);
                }
                r
            })
        {
            if update_cache {
                self.cache_man
                    .lock()
                    .note_used(CacheId::BlockReceipts(*hash));
            }
            if update_pivot_assumption && !is_on_pivot {
                self.db_manager.insert_block_execution_result_to_db(
                    hash,
                    &BlockExecutionResultWithEpoch(
                        *assumed_epoch,
                        receipts.clone(),
                    ),
                )
            }
            return Some(receipts);
        }
        let BlockExecutionResultWithEpoch(epoch, receipts) =
            self.db_manager.block_execution_result_from_db(hash)?;
        if epoch != *assumed_epoch {
            debug!(
                "epoch from db {} does not match assumed {}",
                epoch, assumed_epoch
            );
            return None;
        }
        if update_cache {
            self.block_receipts
                .write()
                .entry(*hash)
                .or_insert(BlockReceiptsInfo::default())
                .insert_receipts_at_epoch(assumed_epoch, receipts.clone());
            self.cache_man
                .lock()
                .note_used(CacheId::BlockReceipts(*hash));
        }
        Some(receipts)
    }

    pub fn block_execution_result_by_hash_from_db(
        &self, hash: &H256,
    ) -> Option<BlockExecutionResultWithEpoch> {
        self.db_manager.block_execution_result_from_db(hash)
    }

    pub fn block_epoch_number(&self, hash: &H256) -> Option<u64> {
        if hash == &self.true_genesis.hash() {
            // True genesis is not executed and does not have an execution
            // result, so we need to process it specially.
            return Some(0);
        }
        self.block_execution_result_by_hash_from_db(&hash)
            .map(|execution_result| execution_result.0)
            .and_then(|pivot| self.block_header_by_hash(&pivot))
            .map(|header| header.height())
    }

    pub fn insert_block_execution_result(
        &self, hash: H256, epoch: H256, block_receipts: Arc<BlockReceipts>,
        persistent: bool,
    )
    {
        let bloom =
            block_receipts
                .receipts
                .iter()
                .fold(Bloom::zero(), |mut b, r| {
                    b.accrue_bloom(&r.log_bloom);
                    b
                });
        let result = BlockExecutionResultWithEpoch(
            epoch,
            BlockExecutionResult {
                block_receipts,
                bloom,
            },
        );

        if persistent {
            self.db_manager
                .insert_block_execution_result_to_db(&hash, &result);
        }

        let mut block_receipts = self.block_receipts.write();
        let receipt_info = block_receipts
            .entry(hash)
            .or_insert(BlockReceiptsInfo::default());
        receipt_info.insert_receipts_at_epoch(&epoch, result.1);

        self.cache_man
            .lock()
            .note_used(CacheId::BlockReceipts(hash));
    }

    pub fn insert_block_reward_result(
        &self, hash: H256, block_reward: BlockRewardResult, persistent: bool,
    ) {
        self.insert(
            hash,
            block_reward,
            &self.block_rewards,
            |hash, value| {
                self.db_manager
                    .insert_block_reward_result_to_db(hash, value)
            },
            Some(CacheId::BlockRewards(hash)),
            persistent,
        )
    }

    pub fn block_reward_result_by_hash(
        &self, hash: &H256,
    ) -> Option<BlockRewardResult> {
        self.get(
            hash,
            &self.block_rewards,
            |key| self.db_manager.block_reward_result_from_db(key),
            Some(CacheId::BlockRewards(*hash)),
        )
    }

    pub fn remove_block_result(&self, hash: &H256, remove_db: bool) {
        self.block_receipts.write().remove(hash);
        self.block_rewards.write().remove(hash);
        if remove_db {
            self.db_manager.remove_block_execution_result_from_db(hash);
            self.db_manager.remove_block_reward_result_from_db(hash);
        }
    }

    pub fn transaction_index_by_hash(
        &self, hash: &H256, update_cache: bool,
    ) -> Option<TransactionIndex> {
        if self.config.persist_tx_index {
            self.get(
                hash,
                &self.transaction_indices,
                |key| self.db_manager.transaction_index_from_db(key),
                if update_cache {
                    Some(CacheId::TransactionAddress(*hash))
                } else {
                    None
                },
            )
        } else {
            self.transaction_indices.read().get(hash).map(|v| v.clone())
        }
    }

    pub fn insert_transaction_index(
        &self, hash: &H256, tx_index: &TransactionIndex,
    ) {
        if self.config.persist_tx_index {
            // transaction_indices will not be updated if it's not inserted
            // before
            self.transaction_indices
                .write()
                .entry(*hash)
                .and_modify(|v| {
                    *v = tx_index.clone();
                    self.cache_man
                        .lock()
                        .note_used(CacheId::TransactionAddress(*hash));
                });
            self.db_manager
                .insert_transaction_index_to_db(hash, tx_index);
        } else {
            // If not persisted, we will just hold it temporarily in memory
            self.transaction_indices
                .write()
                .insert(hash.clone(), tx_index.clone());
            self.cache_man
                .lock()
                .note_used(CacheId::TransactionAddress(*hash));
        }
    }

    pub fn insert_local_block_info(&self, hash: &H256, info: LocalBlockInfo) {
        self.insert(
            *hash,
            info,
            &self.local_block_info,
            |key, value| {
                self.db_manager.insert_local_block_info_to_db(key, value)
            },
            Some(CacheId::LocalBlockInfo(*hash)),
            true,
        )
    }

    pub fn local_block_info_by_hash(
        &self, hash: &H256,
    ) -> Option<LocalBlockInfo> {
        self.get(
            hash,
            &self.local_block_info,
            |key| self.db_manager.local_block_info_from_db(key),
            Some(CacheId::LocalBlockInfo(*hash)),
        )
    }

    pub fn insert_blamed_header_verified_roots(
        &self, height: u64, roots: BlamedHeaderVerifiedRoots,
    ) {
        self.insert(
            height,
            roots,
            &self.blamed_header_verified_roots,
            |key, value| {
                self.db_manager
                    .insert_blamed_header_verified_roots_to_db(*key, value)
            },
            Some(CacheId::BlamedHeaderVerifiedRoots(height)),
            true,
        )
    }

    /// Get correct roots of blamed headers from db.
    /// These are maintained on light nodes only.
    pub fn verified_blamed_roots_by_height(
        &self, height: u64,
    ) -> Option<BlamedHeaderVerifiedRoots> {
        self.get(
            &height,
            &self.blamed_header_verified_roots,
            |key| self.db_manager.blamed_header_verified_roots_from_db(*key),
            Some(CacheId::BlamedHeaderVerifiedRoots(height)),
        )
    }

    pub fn remove_blamed_header_verified_roots(&self, height: u64) {
        self.blamed_header_verified_roots.write().remove(&height);
        self.db_manager
            .remove_blamed_header_verified_roots_from_db(height);
    }

    fn insert<K, V, InsertF>(
        &self, key: K, value: V, in_mem: &RwLock<HashMap<K, V>>,
        insert_f: InsertF, maybe_cache_id: Option<CacheId>, persistent: bool,
    ) where
        K: Clone + Eq + Hash,
        InsertF: Fn(&K, &V),
    {
        if persistent {
            insert_f(&key, &value);
        }
        in_mem.write().insert(key.clone(), value);
        if let Some(cache_id) = maybe_cache_id {
            self.cache_man.lock().note_used(cache_id);
        }
    }

    fn get<K, V, LoadF>(
        &self, key: &K, in_mem: &RwLock<HashMap<K, V>>, load_f: LoadF,
        maybe_cache_id: Option<CacheId>,
    ) -> Option<V>
    where
        K: Clone + Eq + Hash,
        V: Clone,
        LoadF: Fn(&K) -> Option<V>,
    {
        if let Some(value) = in_mem.read().get(key) {
            return Some(value.clone());
        }
        load_f(key).map(|value| {
            if let Some(cache_id) = maybe_cache_id {
                let mut write = in_mem.write();
                write.insert(key.clone(), value.clone());
                self.cache_man.lock().note_used(cache_id);
            }
            value
        })
    }

    pub fn insert_terminals_to_db(&self, terminals: Vec<H256>) {
        self.db_manager.insert_terminals_to_db(&terminals)
    }

    pub fn terminals_from_db(&self) -> Option<Vec<H256>> {
        self.db_manager.terminals_from_db()
    }

    pub fn insert_executed_epoch_set_hashes_to_db(
        &self, epoch_number: u64, epoch_set: &Vec<H256>,
    ) {
        self.db_manager
            .insert_executed_epoch_set_hashes_to_db(epoch_number, epoch_set);
    }

    pub fn insert_skipped_epoch_set_hashes_to_db(
        &self, epoch_number: u64, skipped_set: &Vec<H256>,
    ) {
        self.db_manager
            .insert_skipped_epoch_set_hashes_to_db(epoch_number, skipped_set);
    }

    pub fn executed_epoch_set_hashes_from_db(
        &self, epoch_number: u64,
    ) -> Option<Vec<H256>> {
        if epoch_number != 0 {
            self.db_manager
                .executed_epoch_set_hashes_from_db(epoch_number)
        } else {
            Some(vec![self.true_genesis.hash()])
        }
    }

    pub fn skipped_epoch_set_hashes_from_db(
        &self, epoch_number: u64,
    ) -> Option<Vec<H256>> {
        if epoch_number != 0 {
            self.db_manager
                .skipped_epoch_set_hashes_from_db(epoch_number)
        } else {
            Some(vec![])
        }
    }

    pub fn all_epoch_set_hashes_from_db(
        &self, epoch_number: u64,
    ) -> Option<Vec<H256>> {
        if epoch_number != 0 {
            let mut res = self
                .db_manager
                .skipped_epoch_set_hashes_from_db(epoch_number)?;
            res.append(
                &mut self
                    .db_manager
                    .executed_epoch_set_hashes_from_db(epoch_number)?,
            );
            Some(res)
        } else {
            Some(vec![self.true_genesis.hash()])
        }
    }

    /// Return `false` if there is no executed results for given `block_hash`
    pub fn receipts_retain_epoch(
        &self, block_hash: &H256, epoch: &H256,
    ) -> bool {
        match self.block_receipts.write().get_mut(block_hash) {
            Some(r) => {
                r.retain_epoch(epoch);
                true
            }
            None => false,
        }
    }

    pub fn insert_epoch_execution_context(
        &self, hash: H256, ctx: EpochExecutionContext, persistent: bool,
    ) {
        self.insert(
            hash,
            ctx,
            &self.epoch_execution_contexts,
            |key, value| {
                self.db_manager.insert_execution_context_to_db(key, value)
            },
            None,
            persistent,
        );
    }

    /// The in-memory state will not be updated because it's only garbage
    /// collected explicitly when we make checkpoints.
    pub fn get_epoch_execution_context(
        &self, hash: &H256,
    ) -> Option<EpochExecutionContext> {
        self.get(
            hash,
            &self.epoch_execution_contexts,
            |key| self.db_manager.execution_context_from_db(key),
            None,
        )
    }

    /// TODO We can avoid persisting execution_commitments for blocks
    /// not on the pivot chain after a checkpoint
    pub fn insert_epoch_execution_commitment(
        &self, block_hash: H256,
        state_root_with_aux_info: StateRootWithAuxInfo, receipts_root: H256,
        logs_bloom_hash: H256,
    )
    {
        let commitment = EpochExecutionCommitment {
            state_root_with_aux_info,
            receipts_root,
            logs_bloom_hash,
        };
        self.insert(
            block_hash,
            commitment,
            &self.epoch_execution_commitments,
            |key, value| {
                self.db_manager
                    .insert_epoch_execution_commitment_to_db(key, value)
            },
            None,
            true,
        );
    }

    /// Get in-mem execution commitment.
    pub fn get_epoch_execution_commitment(
        &self, block_hash: &H256,
    ) -> GuardedValue<
        RwLockReadGuard<'_, HashMap<H256, EpochExecutionCommitment>>,
        NonCopy<Option<&'_ EpochExecutionCommitment>>,
    > {
        let read_lock = self.epoch_execution_commitments.read();
        let (read_lock, derefed) = GuardedValue::new_derefed(read_lock).into();
        GuardedValue::new(read_lock, NonCopy(derefed.0.get(block_hash)))
    }

    /// Load commitment from db.
    /// The caller should ensure that the loaded commitment is after
    /// cur_era_genesis and can be garbage-collected by checkpoint.
    pub fn load_epoch_execution_commitment_from_db(
        &self, block_hash: &H256,
    ) -> Option<EpochExecutionCommitment> {
        let commitment = self
            .db_manager
            .epoch_execution_commitment_from_db(block_hash)?;
        self.epoch_execution_commitments
            .write()
            .insert(*block_hash, commitment.clone());
        Some(commitment)
    }

    /// Get persisted execution commitment.
    /// It will check db if it's missing in db.
    pub fn get_epoch_execution_commitment_with_db(
        &self, block_hash: &H256,
    ) -> Option<EpochExecutionCommitment> {
        self.get_epoch_execution_commitment(block_hash).map_or_else(
            || {
                self.db_manager
                    .epoch_execution_commitment_from_db(block_hash)
            },
            |maybe_ref| Some(maybe_ref.clone()),
        )
    }

    pub fn remove_epoch_execution_commitment(&self, block_hash: &H256) {
        self.epoch_execution_commitments.write().remove(block_hash);
    }

    pub fn remove_epoch_execution_commitment_from_db(&self, block_hash: &H256) {
        self.db_manager
            .remove_epoch_execution_commitment_from_db(block_hash);
    }

    pub fn remove_epoch_execution_context(&self, block_hash: &H256) {
        self.epoch_execution_contexts.write().remove(block_hash);
    }

    pub fn remove_epoch_execution_context_from_db(&self, block_hash: &H256) {
        self.db_manager
            .remove_epoch_execution_context_from_db(block_hash);
    }

    pub fn epoch_executed(&self, epoch_hash: &H256) -> bool {
        // `block_receipts_root` is not computed when recovering from db
        self.get_epoch_execution_commitment(epoch_hash).is_some()
    }

    /// Check if all executed results of an epoch exist
    pub fn epoch_executed_and_recovered(
        &self, epoch_hash: &H256, epoch_block_hashes: &Vec<H256>,
        on_local_pivot: bool,
    ) -> bool
    {
        if !self.epoch_executed(epoch_hash) {
            return false;
        }

        if on_local_pivot {
            // Check if all blocks receipts are from this epoch
            let mut epoch_receipts = Vec::new();
            for h in epoch_block_hashes {
                if let Some(r) = self.block_execution_result_by_hash_with_epoch(
                    h, epoch_hash, true, /* update_pivot_assumption */
                    true, /* update_cache */
                ) {
                    epoch_receipts.push(r.block_receipts);
                } else {
                    return false;
                }
            }
            // Recover tx address if we will skip pivot chain execution
            for (block_idx, block_hash) in epoch_block_hashes.iter().enumerate()
            {
                let block = self
                    .block_by_hash(block_hash, true /* update_cache */)
                    .expect("block exists");
                for (tx_idx, tx) in block.transactions.iter().enumerate() {
                    match epoch_receipts[block_idx]
                        .receipts
                        .get(tx_idx)
                        .unwrap()
                        .outcome_status
                    {
                        TRANSACTION_OUTCOME_SUCCESS
                        | TRANSACTION_OUTCOME_EXCEPTION_WITH_NONCE_BUMPING => {
                            self.insert_transaction_index(
                                &tx.hash,
                                &TransactionIndex {
                                    block_hash: *block_hash,
                                    index: tx_idx,
                                },
                            )
                        }
                        _ => {}
                    }
                }
            }
        }
        true
    }

    pub fn invalidate_block(&self, block_hash: H256) {
        // This block will never enter consensus graph, so
        // assign it a NULL sequence number.
        let block_info =
            LocalBlockInfo::new(BlockStatus::Invalid, NULLU64, NULLU64);
        self.db_manager
            .insert_local_block_info_to_db(&block_hash, &block_info);
        self.invalid_block_set.write().insert(block_hash);
    }

    /// Check if a block is already marked as invalid.
    pub fn verified_invalid(
        &self, block_hash: &H256,
    ) -> (bool, Option<LocalBlockInfo>) {
        let invalid_block_set = self.invalid_block_set.upgradable_read();
        if invalid_block_set.contains(block_hash) {
            return (true, None);
        } else {
            if let Some(block_info) =
                self.db_manager.local_block_info_from_db(block_hash)
            {
                match block_info.get_status() {
                    BlockStatus::Invalid => {
                        RwLockUpgradableReadGuard::upgrade(invalid_block_set)
                            .insert(*block_hash);
                        return (true, Some(block_info));
                    }
                    _ => return (false, Some(block_info)),
                }
            } else {
                // No status on disk, so the block is not marked invalid before
                return (false, None);
            }
        }
    }

    pub fn cached_block_count(&self) -> usize { self.blocks.read().len() }

    /// Get current cache size.
    pub fn cache_size(&self) -> CacheSize {
        let malloc_ops = &mut new_malloc_size_ops();
        let block_headers = self.block_headers.read().size_of(malloc_ops);
        let blocks = self.blocks.read().size_of(malloc_ops);
        let compact_blocks = self.compact_blocks.read().size_of(malloc_ops);
        let block_receipts = self.block_receipts.read().size_of(malloc_ops);
        let block_rewards = self.block_rewards.read().size_of(malloc_ops);
        let block_traces = self.block_traces.read().size_of(malloc_ops);
        let transaction_indices =
            self.transaction_indices.read().size_of(malloc_ops);
        let local_block_infos =
            self.local_block_info.read().size_of(malloc_ops);

        CacheSize {
            block_headers,
            blocks,
            block_receipts,
            block_rewards,
            block_traces,
            transaction_indices,
            compact_blocks,
            local_block_infos,
        }
    }

    fn block_cache_gc(&self) {
        let current_size = self.cache_size().total();
        let mut block_headers = self.block_headers.write();
        let mut blocks = self.blocks.write();
        let mut compact_blocks = self.compact_blocks.write();
        let mut executed_results = self.block_receipts.write();
        let mut reward_results = self.block_rewards.write();
        let mut block_traces = self.block_traces.write();
        let mut tx_indices = self.transaction_indices.write();
        let mut local_block_info = self.local_block_info.write();
        let mut blamed_header_verified_roots =
            self.blamed_header_verified_roots.write();
        let mut cache_man = self.cache_man.lock();

        debug!(
            "Before gc cache_size={} {} {} {} {} {} {} {} {} {}",
            current_size,
            block_headers.len(),
            blocks.len(),
            compact_blocks.len(),
            executed_results.len(),
            reward_results.len(),
            block_traces.len(),
            tx_indices.len(),
            local_block_info.len(),
            blamed_header_verified_roots.len(),
        );

        cache_man.collect_garbage(current_size, |ids| {
            for id in &ids {
                match id {
                    CacheId::Block(h) => {
                        blocks.remove(h);
                    }
                    CacheId::BlockHeader(h) => {
                        block_headers.remove(h);
                    }
                    CacheId::CompactBlock(h) => {
                        compact_blocks.remove(h);
                    }
                    CacheId::BlockReceipts(h) => {
                        executed_results.remove(h);
                    }
                    CacheId::BlockRewards(h) => {
                        reward_results.remove(h);
                    }
                    CacheId::BlockTraces(h) => {
                        block_traces.remove(h);
                    }
                    CacheId::TransactionAddress(h) => {
                        tx_indices.remove(h);
                    }
                    CacheId::LocalBlockInfo(h) => {
                        local_block_info.remove(h);
                    }
                    CacheId::BlamedHeaderVerifiedRoots(h) => {
                        blamed_header_verified_roots.remove(h);
                    }
                }
            }

            let malloc_ops = &mut new_malloc_size_ops();
            block_headers.size_of(malloc_ops)
                + blocks.size_of(malloc_ops)
                + executed_results.size_of(malloc_ops)
                + reward_results.size_of(malloc_ops)
                + block_traces.size_of(malloc_ops)
                + tx_indices.size_of(malloc_ops)
                + compact_blocks.size_of(malloc_ops)
                + local_block_info.size_of(malloc_ops)
        });

        block_headers.shrink_to_fit();
        blocks.shrink_to_fit();
        executed_results.shrink_to_fit();
        reward_results.shrink_to_fit();
        block_traces.shrink_to_fit();
        tx_indices.shrink_to_fit();
        compact_blocks.shrink_to_fit();
        local_block_info.shrink_to_fit();
    }

    pub fn cache_gc(&self) { self.block_cache_gc(); }

    pub fn set_cur_consensus_era_genesis_hash(
        &self, cur_era_hash: &H256, next_era_hash: &H256,
    ) {
        self.db_manager
            .insert_checkpoint_hashes_to_db(cur_era_hash, next_era_hash);

        let mut era_hash = self.cur_consensus_era_genesis_hash.write();
        let mut stable_hash = self.cur_consensus_era_stable_hash.write();
        *era_hash = cur_era_hash.clone();
        *stable_hash = next_era_hash.clone();
    }

    pub fn get_cur_consensus_era_genesis_hash(&self) -> H256 {
        self.cur_consensus_era_genesis_hash.read().clone()
    }

    pub fn get_cur_consensus_era_stable_hash(&self) -> H256 {
        self.cur_consensus_era_stable_hash.read().clone()
    }

    pub fn recover_unsigned_tx(
        &self, transactions: &Vec<TransactionWithSignature>,
    ) -> Result<Vec<Arc<SignedTransaction>>, DecoderError> {
        let _timer = MeterTimer::time_func(TX_POOL_RECOVER_TIMER.as_ref());
        self.tx_data_manager.recover_unsigned_tx(transactions)
    }

    pub fn recover_block(&self, block: &mut Block) -> Result<(), DecoderError> {
        self.tx_data_manager.recover_block(block)
    }

    pub fn recover_unsigned_tx_with_order(
        &self, transactions: &Vec<TransactionWithSignature>,
    ) -> Result<Vec<Arc<SignedTransaction>>, DecoderError> {
        self.tx_data_manager
            .recover_unsigned_tx_with_order(transactions)
    }

    pub fn find_missing_tx_indices_encoded(
        &self, compact_block: &mut CompactBlock,
    ) -> Vec<usize> {
        self.tx_data_manager
            .find_missing_tx_indices_encoded(compact_block)
    }

    /// Caller should make sure the state exists.
    pub fn get_state_readonly_index(
        &self, block_hash: &EpochId,
    ) -> Option<StateIndex> {
        let maybe_commitment =
            self.get_epoch_execution_commitment_with_db(block_hash);
        let maybe_state_index = match maybe_commitment {
            None => None,
            Some(execution_commitment) => Some(StateIndex::new_for_readonly(
                block_hash,
                &execution_commitment.state_root_with_aux_info,
            )),
        };
        maybe_state_index
    }

    // TODO: There could be io error when getting block by hash.
    pub fn get_parent_epochs_for(
        &self, mut block: EpochId, mut count: u64,
    ) -> (EpochId, Vec<EpochId>) {
        let mut epochs_reverse_order = vec![];
        while count > 0 {
            debug!("getting parent for block {:?}", block);
            epochs_reverse_order.push(block);
            block = *self.block_header_by_hash(&block).unwrap().parent_hash();
            if block == NULL_EPOCH
                || block == *self.cur_consensus_era_genesis_hash.read()
            {
                break;
            }
            count -= 1;
        }

        debug!("get_parent_epochs stopped at block {:?}", block);
        epochs_reverse_order.reverse();
        (block, epochs_reverse_order)
    }

    pub fn get_snapshot_epoch_count(&self) -> u32 {
        self.storage_manager
            .get_storage_manager()
            .get_snapshot_epoch_count()
    }

    pub fn get_snapshot_blame_plus_depth(&self) -> usize {
        // We need the extra + 1 to get a state root that points to the
        // snapshot we want.
        self.storage_manager
            .get_storage_manager()
            .get_snapshot_epoch_count() as usize
            + 1
    }

    pub fn get_executed_state_root(&self, block_hash: &H256) -> Option<H256> {
        let maybe_commitment =
            self.get_epoch_execution_commitment(block_hash).take();
        if let Some(commitment) = maybe_commitment {
            Some(commitment.state_root_with_aux_info.aux_info.state_root_hash)
        } else {
            None
        }
    }

    pub fn earliest_epoch_with_block_body(&self) -> u64 {
        match self.config.additional_maintained_block_body_epoch_count {
            Some(defer) => self.gc_progress.lock().gc_end - defer as u64,
            None => 0,
        }
    }

    pub fn earliest_epoch_with_execution_result(&self) -> u64 {
        match self
            .config
            .additional_maintained_execution_result_epoch_count
        {
            Some(defer) => self.gc_progress.lock().gc_end - defer as u64,
            None => 0,
        }
    }

    pub fn new_checkpoint(
        &self, new_checkpoint_height: u64, best_epoch_number: u64,
    ) {
        let mut gc_progress = self.gc_progress.lock();
        gc_progress.gc_end = new_checkpoint_height;
        gc_progress.last_consensus_best_epoch = best_epoch_number;
        gc_progress.expected_end_consensus_best_epoch = best_epoch_number
            + self.config.checkpoint_gc_time_in_epoch_count as u64;
    }

    pub fn database_gc(&self, best_epoch: u64) {
        let maybe_range = self.gc_progress.lock().get_gc_range(best_epoch);
        if let Some((start, end)) = maybe_range {
            for epoch_number in start..end {
                self.gc_epoch(epoch_number);
            }
            let mut gc_progress = self.gc_progress.lock();
            gc_progress.last_consensus_best_epoch = best_epoch;
            gc_progress.next_to_process = end;
        }
    }

    fn gc_epoch(&self, epoch_number: u64) {
        // We must GC tx index before block body, otherwise we may be unable to
        // get the transactions in this epoch.
        if let Some(defer_epochs) = self
            .config
            .additional_maintained_transaction_index_epoch_count
        {
            if epoch_number > defer_epochs as u64 {
                let epoch_to_remove = epoch_number - defer_epochs as u64;
                match self.all_epoch_set_hashes_from_db(epoch_to_remove) {
                    None => warn!(
                        "GC epoch set is missing! epoch_to_remove: {}",
                        epoch_to_remove
                    ),
                    Some(epoch_set) => {
                        // Store all packed transactions in a set first to
                        // deduplicate transactions for database operations.
                        let mut transaction_set = HashSet::new();
                        for b in epoch_set {
                            if let Some(transactions) =
                                self.db_manager.block_body_from_db(&b)
                            {
                                for tx in transactions {
                                    transaction_set.insert(tx.hash());
                                }
                            }
                        }
                        for tx in transaction_set {
                            self.db_manager
                                .remove_transaction_index_from_db(&tx);
                        }
                    }
                }
            }
        };
        self.gc_epoch_with_defer(
            epoch_number,
            self.config.additional_maintained_block_body_epoch_count,
            |h| self.db_manager.remove_block_body_from_db(h),
        );
        self.gc_epoch_with_defer(
            epoch_number,
            self.config
                .additional_maintained_execution_result_epoch_count,
            |h| self.db_manager.remove_block_execution_result_from_db(h),
        );
        self.gc_epoch_with_defer(
            epoch_number,
            self.config.additional_maintained_reward_epoch_count,
            |h| self.db_manager.remove_block_reward_result_from_db(h),
        );
        self.gc_epoch_with_defer(
            epoch_number,
            self.config.additional_maintained_trace_epoch_count,
            |h| self.db_manager.remove_block_trace_from_db(h),
        );
    }

    fn gc_epoch_with_defer<F>(
        &self, epoch_number: u64, maybe_defer_epochs: Option<usize>, gc_func: F,
    ) where F: Fn(&H256) -> () {
        if let Some(defer_epochs) = maybe_defer_epochs {
            if epoch_number > defer_epochs as u64 {
                let epoch_to_remove = epoch_number - defer_epochs as u64;
                match self.all_epoch_set_hashes_from_db(epoch_to_remove) {
                    None => warn!(
                        "GC epoch set is missing! epoch_to_remove: {}",
                        epoch_to_remove
                    ),
                    Some(epoch_set) => {
                        for b in epoch_set {
                            gc_func(&b);
                        }
                    }
                }
            }
        }
    }
}

#[derive(Copy, Clone)]
pub enum DbType {
    Rocksdb,
    Sqlite,
}

pub struct DataManagerConfiguration {
    pub persist_tx_index: bool,
    pub tx_cache_index_maintain_timeout: Duration,
    pub db_type: DbType,
    pub additional_maintained_block_body_epoch_count: Option<usize>,
    pub additional_maintained_execution_result_epoch_count: Option<usize>,
    pub additional_maintained_reward_epoch_count: Option<usize>,
    pub additional_maintained_trace_epoch_count: Option<usize>,
    pub additional_maintained_transaction_index_epoch_count: Option<usize>,
    pub checkpoint_gc_time_in_epoch_count: usize,
}

impl MallocSizeOf for DataManagerConfiguration {
    fn size_of(&self, _ops: &mut MallocSizeOfOps) -> usize { 0 }
}

impl DataManagerConfiguration {
    pub fn new(
        persist_tx_index: bool, tx_cache_index_maintain_timeout: Duration,
        db_type: DbType,
    ) -> Self
    {
        Self {
            persist_tx_index,
            tx_cache_index_maintain_timeout,
            db_type,
            additional_maintained_block_body_epoch_count: None,
            additional_maintained_execution_result_epoch_count: None,
            additional_maintained_reward_epoch_count: None,
            additional_maintained_trace_epoch_count: None,
            additional_maintained_transaction_index_epoch_count: None,
            checkpoint_gc_time_in_epoch_count: 1,
        }
    }
}
