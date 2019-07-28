// Copyright 2019 Conflux Foundation. All rights reserved.
// Conflux is free software and distributed under GNU General Public License.
// See http://www.gnu.org/licenses/

use crate::{
    consensus::ConsensusGraphInner,
    sync::{
        synchronization_protocol_handler::{
            SynchronizationProtocolHandler, CATCH_UP_EPOCH_LAG_THRESHOLD,
        },
        synchronization_state::SynchronizationState,
        SharedSynchronizationGraph, SynchronizationGraphInner,
    },
};
use network::NetworkContext;
use parking_lot::RwLock;
use std::{collections::HashMap, sync::Arc};

///
/// Archive node goes through the following phases:
///     CatchUpRecoverBlockFromDB --> CatchUpSyncBlock --> Normal
///
/// Full node goes through the following phases:
///     CatchUpRecoverBlockHeaderFromDB --> CatchUpSyncBlockHeader -->
///     CatchUpCheckpoint --> CatchUpRecoverBlockFromDB -->
///     CatchUpSyncBlock --> Normal

#[derive(Debug, Clone, Copy, Ord, PartialOrd, Eq, PartialEq, Hash)]
pub enum SyncPhaseType {
    CatchUpRecoverBlockHeaderFromDB = 0,
    CatchUpSyncBlockHeader = 1,
    CatchUpCheckpoint = 2,
    CatchUpRecoverBlockFromDB = 3,
    CatchUpSyncBlock = 4,
    Normal = 5,
}

pub trait SynchronizationPhaseTrait: Send + Sync {
    fn name(&self) -> String;
    fn phase_type(&self) -> SyncPhaseType;
    fn next(&self) -> SyncPhaseType;
    fn start(
        &self, _io: &NetworkContext,
        _sync_handler: &SynchronizationProtocolHandler,
    );
}

pub struct SynchronizationPhaseManagerInner {
    initialized: bool,
    current_phase: SyncPhaseType,
    phases: HashMap<SyncPhaseType, Arc<SynchronizationPhaseTrait>>,
}

impl SynchronizationPhaseManagerInner {
    pub fn new(initial_phase_type: SyncPhaseType) -> Self {
        SynchronizationPhaseManagerInner {
            initialized: false,
            current_phase: initial_phase_type,
            phases: HashMap::new(),
        }
    }

    pub fn register_phase(&mut self, phase: Arc<SynchronizationPhaseTrait>) {
        self.phases.insert(phase.phase_type(), phase);
        //self.phases[phase.phase_type()] = phase;
    }

    pub fn get_phase(
        &self, phase_type: SyncPhaseType,
    ) -> Arc<SynchronizationPhaseTrait> {
        self.phases.get(&phase_type).unwrap().clone()
    }

    pub fn get_current_phase(&self) -> Arc<SynchronizationPhaseTrait> {
        self.get_phase(self.current_phase)
    }

    pub fn change_phase_to(&mut self, phase_type: SyncPhaseType) {
        self.current_phase = phase_type;
    }

    pub fn try_initialize(&mut self) -> bool {
        let initialized = self.initialized;
        if !self.initialized {
            self.initialized = true;
        }

        initialized
    }
}

pub struct SynchronizationPhaseManager {
    inner: RwLock<SynchronizationPhaseManagerInner>,
}

impl SynchronizationPhaseManager {
    pub fn new(
        initial_phase_type: SyncPhaseType,
        sync_state: Arc<SynchronizationState>,
        sync_graph: SharedSynchronizationGraph,
    ) -> Self
    {
        let sync_manager = SynchronizationPhaseManager {
            inner: RwLock::new(SynchronizationPhaseManagerInner::new(
                initial_phase_type,
            )),
        };

        sync_manager.register_phase(Arc::new(
            CatchUpRecoverBlockHeaderFromDbPhase::new(sync_graph.clone()),
        ));
        sync_manager.register_phase(Arc::new(
            CatchUpSyncBlockHeaderPhase::new(
                sync_state.clone(),
                sync_graph.clone(),
            ),
        ));
        sync_manager.register_phase(Arc::new(CatchUpCheckpointPhase::new(
            sync_graph.clone(),
        )));
        sync_manager.register_phase(Arc::new(
            CatchUpRecoverBlockFromDbPhase::new(sync_graph.clone()),
        ));
        sync_manager.register_phase(Arc::new(CatchUpSyncBlockPhase::new(
            sync_state.clone(),
            sync_graph.clone(),
        )));
        sync_manager.register_phase(Arc::new(NormalSyncPhase::new()));

        sync_manager
    }

    pub fn register_phase(&self, phase: Arc<SynchronizationPhaseTrait>) {
        self.inner.write().register_phase(phase);
    }

    pub fn get_phase(
        &self, phase_type: SyncPhaseType,
    ) -> Arc<SynchronizationPhaseTrait> {
        self.inner.read().get_phase(phase_type)
    }

    pub fn get_current_phase(&self) -> Arc<SynchronizationPhaseTrait> {
        self.inner.read().get_current_phase()
    }

    pub fn change_phase_to(
        &self, phase_type: SyncPhaseType, io: &NetworkContext,
        sync_handler: &SynchronizationProtocolHandler,
    )
    {
        self.inner.write().change_phase_to(phase_type);
        let current_phase = self.get_current_phase();
        current_phase.start(io, sync_handler);
    }

    pub fn try_initialize(
        &self, io: &NetworkContext,
        sync_handler: &SynchronizationProtocolHandler,
    )
    {
        if !self.inner.write().try_initialize() {
            // if not initialized
            let current_phase = self.get_current_phase();
            current_phase.start(io, sync_handler);
        }
    }
}

pub struct CatchUpRecoverBlockHeaderFromDbPhase {
    pub graph: SharedSynchronizationGraph,
}

impl CatchUpRecoverBlockHeaderFromDbPhase {
    pub fn new(graph: SharedSynchronizationGraph) -> Self {
        CatchUpRecoverBlockHeaderFromDbPhase { graph }
    }
}

impl SynchronizationPhaseTrait for CatchUpRecoverBlockHeaderFromDbPhase {
    fn name(&self) -> String {
        String::from("CatchUpRecoverBlockHeaderFromDbPhase")
    }

    fn phase_type(&self) -> SyncPhaseType {
        SyncPhaseType::CatchUpRecoverBlockHeaderFromDB
    }

    fn next(&self) -> SyncPhaseType { SyncPhaseType::CatchUpSyncBlockHeader }

    fn start(
        &self, _io: &NetworkContext,
        _sync_handler: &SynchronizationProtocolHandler,
    )
    {
        info!("start phase {:?}", self.name());
        // FIXME: should dispatch to another worker thread to do this.
        self.graph.recover_graph_from_db(true /* header_only */);
    }
}

pub struct CatchUpSyncBlockHeaderPhase {
    pub syn: Arc<SynchronizationState>,
    pub graph: SharedSynchronizationGraph,
}

impl CatchUpSyncBlockHeaderPhase {
    pub fn new(
        syn: Arc<SynchronizationState>, graph: SharedSynchronizationGraph,
    ) -> Self {
        CatchUpSyncBlockHeaderPhase { syn, graph }
    }
}

impl SynchronizationPhaseTrait for CatchUpSyncBlockHeaderPhase {
    fn name(&self) -> String { String::from("CatchUpSyncBlockHeaderPhase") }

    fn phase_type(&self) -> SyncPhaseType {
        SyncPhaseType::CatchUpSyncBlockHeader
    }

    fn next(&self) -> SyncPhaseType {
        let middle_epoch = self.syn.get_middle_epoch();
        if middle_epoch.is_none() {
            return self.phase_type();
        }
        let middle_epoch = middle_epoch.unwrap();
        if self.graph.consensus.best_epoch_number()
            + CATCH_UP_EPOCH_LAG_THRESHOLD
            >= middle_epoch
        {
            return SyncPhaseType::CatchUpCheckpoint;
        }

        self.phase_type()
    }

    fn start(
        &self, io: &NetworkContext,
        sync_handler: &SynchronizationProtocolHandler,
    )
    {
        info!("start phase {:?}", self.name());
        let (_, cur_era_genesis_height) =
            self.graph.get_genesis_hash_and_height_in_current_era();
        *sync_handler.latest_epoch_requested.lock() = cur_era_genesis_height;

        sync_handler.request_initial_missed_block(io);
        sync_handler.request_epochs(io);
    }
}

pub struct CatchUpCheckpointPhase {}

impl CatchUpCheckpointPhase {
    pub fn new(_graph: SharedSynchronizationGraph) -> Self {
        CatchUpCheckpointPhase {}
    }
}

impl SynchronizationPhaseTrait for CatchUpCheckpointPhase {
    fn name(&self) -> String { String::from("CatchUpCheckpointPhase") }

    fn phase_type(&self) -> SyncPhaseType { SyncPhaseType::CatchUpCheckpoint }

    fn next(&self) -> SyncPhaseType { SyncPhaseType::CatchUpRecoverBlockFromDB }

    fn start(
        &self, _io: &NetworkContext,
        _sync_handler: &SynchronizationProtocolHandler,
    )
    {
        info!("start phase {:?}", self.name());
    }
}

pub struct CatchUpRecoverBlockFromDbPhase {
    pub graph: SharedSynchronizationGraph,
}

impl CatchUpRecoverBlockFromDbPhase {
    pub fn new(graph: SharedSynchronizationGraph) -> Self {
        CatchUpRecoverBlockFromDbPhase { graph }
    }
}

impl SynchronizationPhaseTrait for CatchUpRecoverBlockFromDbPhase {
    fn name(&self) -> String { String::from("CatchUpRecoverBlockFromDbPhase") }

    fn phase_type(&self) -> SyncPhaseType {
        SyncPhaseType::CatchUpRecoverBlockFromDB
    }

    fn next(&self) -> SyncPhaseType { SyncPhaseType::CatchUpSyncBlock }

    fn start(
        &self, _io: &NetworkContext,
        _sync_handler: &SynchronizationProtocolHandler,
    )
    {
        info!("start phase {:?}", self.name());
        {
            let (cur_era_genesis_hash, _) =
                self.graph.get_genesis_hash_and_height_in_current_era();
            // Acquire both lock first to ensure consistency
            let old_consensus_inner = &mut *self.graph.consensus.inner.write();
            let old_sync_inner = &mut *self.graph.inner.write();
            let new_consensus_inner =
                ConsensusGraphInner::with_era_genesis_block(
                    old_consensus_inner.pow_config.clone(),
                    self.graph.data_man.clone(),
                    old_consensus_inner.inner_conf.clone(),
                    &cur_era_genesis_hash,
                );
            self.graph.consensus.update_best_info(&new_consensus_inner);
            *old_consensus_inner = new_consensus_inner;
            let new_sync_inner = SynchronizationGraphInner::with_genesis_block(
                self.graph
                    .data_man
                    .block_header_by_hash(&cur_era_genesis_hash)
                    .expect("era genesis exists"),
                old_sync_inner.pow_config.clone(),
                old_sync_inner.data_man.clone(),
            );
            *old_sync_inner = new_sync_inner;
        }

        // FIXME: should dispatch to another worker thread to do this.
        self.graph.recover_graph_from_db(false /* header_only */);
    }
}

pub struct CatchUpSyncBlockPhase {
    pub syn: Arc<SynchronizationState>,
    pub graph: SharedSynchronizationGraph,
}

impl CatchUpSyncBlockPhase {
    pub fn new(
        syn: Arc<SynchronizationState>, graph: SharedSynchronizationGraph,
    ) -> Self {
        CatchUpSyncBlockPhase { syn, graph }
    }
}

impl SynchronizationPhaseTrait for CatchUpSyncBlockPhase {
    fn name(&self) -> String { String::from("CatchUpSyncBlockPhase") }

    fn phase_type(&self) -> SyncPhaseType { SyncPhaseType::CatchUpSyncBlock }

    fn next(&self) -> SyncPhaseType {
        let middle_epoch = self.syn.get_middle_epoch();
        if middle_epoch.is_none() {
            return self.phase_type();
        }
        let middle_epoch = middle_epoch.unwrap();
        if self.graph.consensus.best_epoch_number()
            + CATCH_UP_EPOCH_LAG_THRESHOLD
            >= middle_epoch
        {
            return SyncPhaseType::Normal;
        }

        self.phase_type()
    }

    fn start(
        &self, io: &NetworkContext,
        sync_handler: &SynchronizationProtocolHandler,
    )
    {
        info!("start phase {:?}", self.name());

        let (_, cur_era_genesis_height) =
            self.graph.get_genesis_hash_and_height_in_current_era();
        *sync_handler.latest_epoch_requested.lock() = cur_era_genesis_height;

        sync_handler.request_initial_missed_block(io);
        sync_handler.request_epochs(io);
    }
}

pub struct NormalSyncPhase {}

impl NormalSyncPhase {
    pub fn new() -> Self { NormalSyncPhase {} }
}

impl SynchronizationPhaseTrait for NormalSyncPhase {
    fn name(&self) -> String { String::from("NormalSyncPhase") }

    fn phase_type(&self) -> SyncPhaseType { SyncPhaseType::Normal }

    fn next(&self) -> SyncPhaseType {
        // FIXME: handle the case where we need to switch back phase
        self.phase_type()
    }

    fn start(
        &self, io: &NetworkContext,
        sync_handler: &SynchronizationProtocolHandler,
    )
    {
        info!("start phase {:?}", self.name());
        sync_handler.request_missing_terminals(io);
    }
}
