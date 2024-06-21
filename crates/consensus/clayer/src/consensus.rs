mod config;
// use alloy_primitives::{keccak256, B64};
pub use config::*;
mod logs;
pub use logs::*;
mod message;
use lru_cache::LruCache;
pub use message::*;
mod pbft_error;
pub use pbft_error::*;
mod state;
use reth_db::models::consensus::ConsensusBytes;
use reth_provider::{BlockReaderIdExt, ConsensusNumberReader, ConsensusNumberWriter};
use reth_rpc_types::{engine::PayloadId, ExecutionPayloadV1, ExecutionPayloadV2, PeerId};
pub use state::*;
mod validators;

use alloy_rlp::{Decodable, Encodable};
use itertools::Itertools;
use reth_ecies::util::id2pk;
use reth_eth_wire::{
    ClayerBlock, ClayerConsensusMessage, ClayerConsensusMessageHeader, ClayerExecutionPayload,
    ClayerSignature, PbftMessage, PbftMessageInfo, PbftMessageType, PbftNewValidator, PbftNewView,
    PbftSeal, PbftSignedVote,
};
use reth_interfaces::clayer::{ClayerConsensusEvent, ClayerConsensusMessageAgentTrait};
use reth_primitives::{
    keccak256, public_key_to_address, sign_message, BlockId, SealedHeader, B256, B64,
};
use std::{
    collections::{HashSet, VecDeque},
    sync::Arc,
};
use tokio::sync::mpsc::{self, Receiver, Sender};
use tracing::*;

use crate::{
    engine_api::{ApiService, ExecutionPayloadWrapperV2},
    timing::{retry_until_ok, Timeout},
};

pub const ELECT_VOTING_ADDRESS: &str = "0x0000000000000000000000000000000000001000";

pub struct ClayerConsensusMessagingAgent {
    pub inner: Arc<parking_lot::RwLock<ClayerConsensusMessagingAgentInner>>,
}

impl Clone for ClayerConsensusMessagingAgent {
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone() }
    }
}

impl ClayerConsensusMessagingAgent {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(parking_lot::RwLock::new(ClayerConsensusMessagingAgentInner::new())),
        }
    }
}

impl ClayerConsensusMessageAgentTrait for ClayerConsensusMessagingAgent {
    /// Returns pending consensus listener
    fn pending_consensus_listener(&self) -> Receiver<(Vec<PeerId>, reth_primitives::Bytes)> {
        self.inner.write().pending_consensus_listener()
    }

    /// push data received from network into cache
    fn push_received_cache(&self, peer_id: PeerId, data: reth_primitives::Bytes) {
        self.inner.write().push_received_cache(peer_id, data);
    }

    /// push network event(PeerConnected, PeerDisconnected)
    fn push_network_event(&self, peer_id: PeerId, connect: bool) {
        self.inner.write().push_network_event(peer_id, connect);
    }

    fn pop_event(&self) -> Option<ClayerConsensusEvent> {
        self.inner.write().pop_event()
    }

    fn push_block_event(&self, event: ClayerConsensusEvent) {
        self.inner.write().push_block_event(event);
    }

    /// broadcast consensus
    fn broadcast_consensus(&self, peers: Vec<PeerId>, data: reth_primitives::Bytes) {
        self.inner.read().broadcast_consensus(peers, data);
    }
    /// get all peers
    fn get_peers(&self) -> Vec<PeerId> {
        self.inner.read().get_peers()
    }
}

pub struct ClayerConsensusMessagingAgentInner {
    queued: VecDeque<ClayerConsensusEvent>,
    // cache_tx: crossbeam_channel::Sender<ClayerConsensusEvent>,
    // cache_re: crossbeam_channel::Receiver<ClayerConsensusEvent>,
    sender: Option<Sender<(Vec<PeerId>, reth_primitives::Bytes)>>,
    active_peers: HashSet<PeerId>,
}

impl ClayerConsensusMessagingAgentInner {
    pub fn new() -> Self {
        // let (tx, re) = crossbeam_channel::unbounded::<ClayerConsensusEvent>();
        Self { queued: VecDeque::new(), sender: None, active_peers: HashSet::new() }
    }
}

impl ClayerConsensusMessagingAgentInner {
    fn pending_consensus_listener(&mut self) -> Receiver<(Vec<PeerId>, reth_primitives::Bytes)> {
        let (sender, rx) = mpsc::channel(1024);
        self.sender = Some(sender);
        rx
    }

    fn push_received_cache(&mut self, peer_id: PeerId, data: reth_primitives::Bytes) {
        self.queued.push_back(ClayerConsensusEvent::PeerMessage(peer_id, data));
        // let _ = self.cache_tx.send(ClayerConsensusEvent::PeerMessage(peer_id, data));
    }

    /// push network event(PeerConnected, PeerDisconnected)
    fn push_network_event(&mut self, peer_id: PeerId, connect: bool) {
        self.queued.push_back(ClayerConsensusEvent::PeerNetWork(peer_id, connect));
        // let _ = self.cache_tx.send(ClayerConsensusEvent::PeerNetWork(peer_id, connect));
        if connect {
            self.active_peers.insert(peer_id);
        } else {
            self.active_peers.remove(&peer_id);
        }
    }

    /// pop network event(PeerConnected, PeerDisconnected)
    fn pop_event(&mut self) -> Option<ClayerConsensusEvent> {
        self.queued.pop_front()
    }

    fn push_block_event(&mut self, event: ClayerConsensusEvent) {
        self.queued.push_front(event);
    }

    fn broadcast_consensus(&self, peers: Vec<PeerId>, data: reth_primitives::Bytes) {
        if let Some(sender) = &self.sender {
            match sender.try_send((peers, data)) {
                Ok(()) => {}
                Err(err) => {
                    error!(target:"consensus::cl","broadcast_consensus error {:?}",err);
                }
            }
        }
    }

    fn get_peers(&self) -> Vec<PeerId> {
        self.active_peers.iter().cloned().collect()
    }
}

pub struct ClayerConsensusEngine<Client, CDB> {
    /// Log of messages this node has received and accepted
    pub msg_log: PbftLog,
    service: ApiService,
    agent: ClayerConsensusMessagingAgent,
    db: Arc<CDB>,
    client: Client,
    announce_block: LruCache<B256, u64>,
}

impl<Client, CDB> ClayerConsensusEngine<Client, CDB>
where
    CDB: ConsensusNumberReader + ConsensusNumberWriter + 'static,
    Client: BlockReaderIdExt + 'static,
{
    pub fn new(
        agent: ClayerConsensusMessagingAgent,
        service: ApiService,
        db: Arc<CDB>,
        client: Client,
    ) -> Self {
        Self {
            msg_log: PbftLog::default(),
            service,
            agent,
            db,
            client,
            announce_block: LruCache::new(10),
        }
    }

    pub fn initialize(&mut self, block: ClayerBlock, config: &PbftConfig, state: &mut PbftState) {
        // Add chain head to log and update state
        self.msg_log.resize_log(&config);
        self.msg_log.add_validated_block(block.clone());
        state.chain_head = block.block_id();

        info!(target: "consensus::cl","Initialized with block number {}, state {}", block.block_num(),state);

        // If starting up from a non-genesis block, the node may need to perform some special
        // actions
        if block.block_num() > 0 {
            // If starting up with a block that has a consensus seal, update the view to match
            if let Ok(seal) = PbftSeal::decode(&mut block.seal_bytes.to_vec().as_slice()) {
                state.view = seal.info.view;
                info!(target: "consensus::cl","Updated view to {} on startup", state.view);
            }

            // If connected to any peers already, send bootstrap commit messages to them
            if state.is_validator() {
                let connected_peers: Vec<PeerId> = self.agent.get_peers();
                for peer_id in connected_peers {
                    self.broadcast_bootstrap_commit(peer_id, state).unwrap_or_else(|err| {
                        error!("Failed to broadcast bootstrap commit due to error: {}", err)
                    });
                }
            } else {
                //nothing
            }
        }

        // Primary initializes a block
        if state.is_primary() {
            self.service.initialize_block(None).unwrap_or_else(|err| {
                error!("Couldn't initialize block on startup due to error: {}", err)
            });
        }
    }

    fn service_mut(&mut self) -> &mut ApiService {
        &mut self.service
    }

    // ---------- Methods for handling Updates from the Validator ----------

    /// Handle a peer message from another PbftNode
    ///
    /// Handle all messages from other nodes. Such messages include `PrePrepare`, `Prepare`,
    /// `Commit`, `ViewChange`, and `NewView`. Make sure the message is from a PBFT member. If the
    /// node is view changing, ignore all messages that aren't `ViewChange`s or `NewView`s.
    pub fn on_peer_message(
        &mut self,
        peer_id: PeerId,
        msg: ParsedMessage,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        if msg.info().signer_id == state.id {
            info!(target: "consensus::cl","{}: Got peer(Self) message: {}", state, msg.info());
        } else {
            info!(target: "consensus::cl","{}: Got peer message: {}", state, msg.info());
        }

        // Make sure this message is from a known member of the PBFT network
        if !state.validators.contains(&msg.info().signer_id) {
            return Err(PbftError::InvalidMessage(format!(
                "Received message from node ({:?}) that is not a member of the PBFT network",
                hex::encode(msg.info().signer_id),
            )));
        }

        let msg_type = PbftMessageType::from(msg.info().ptype);

        //  if the node is a member of the PBFT network or not；
        if !state.is_validator() {
            match msg_type {
                PbftMessageType::AnnounceBlock => {
                    self.handle_announceblock_response(&msg, state)?
                }
                PbftMessageType::NewValidator => self.handle_new_validator(&msg, state)?,
                PbftMessageType::Seal => {
                    if state.becoming_validator {
                        self.handle_seal_response(&msg, state)?
                    }
                }
                _ => {
                    warn!(target: "consensus::cl","Received message with validiator type: {:?}", msg_type)
                }
            }
            return Ok(());
        }

        // If this node is in the process of a view change, ignore all messages except ViewChanges
        // and NewViews
        if matches!(state.mode, PbftMode::ViewChanging(_))
            && msg_type != PbftMessageType::ViewChange
            && msg_type != PbftMessageType::NewView
        {
            debug!(target: "consensus::cl","{}: Node is view changing; ignoring {} message", state, msg_type);
            return Ok(());
        }

        match msg_type {
            PbftMessageType::PrePrepare => self.handle_pre_prepare(msg, state)?,
            PbftMessageType::Prepare => self.handle_prepare(msg, state)?,
            PbftMessageType::Commit => self.handle_commit(msg, state)?,
            PbftMessageType::ViewChange => self.handle_view_change(&msg, state)?,
            PbftMessageType::NewView => self.handle_new_view(&msg, state)?,
            PbftMessageType::SealRequest => self.handle_seal_request(peer_id, msg, state)?,
            PbftMessageType::Seal => self.handle_seal_response(&msg, state)?,
            PbftMessageType::BlockNew => self.handle_block_new(msg, state)?,
            PbftMessageType::AnnounceBlock => self.handle_announceblock_response(&msg, state)?,
            _ => {
                warn!(target: "consensus::cl","Received message with unknown type: {:?}", msg_type)
            }
        }

        Ok(())
    }

    /// Handle a `PrePrepare` message
    ///
    /// A `PrePrepare` message is accepted and added to the log if the following are true:
    /// - The message signature is valid (already verified by validator)
    /// - The message is from the primary
    /// - The message's view matches the node's current view
    /// - A `PrePrepare` message does not already exist at this view and sequence number with a
    ///   different block
    ///
    /// Once a `PrePrepare` for the current sequence number is accepted and added to the log, the
    /// node will try to switch to the `Preparing` phase.
    fn handle_pre_prepare(
        &mut self,
        msg: ParsedMessage,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        info!(target: "consensus::cl","handle_pre_prepare: state {}", state);
        // Check that the message is from the current primary
        if msg.info().signer_id != state.get_primary_id() {
            warn!(target: "consensus::cl",
                "Got PrePrepare from a secondary node {:?}; ignoring message",
                msg.info().signer_id
            );
            return Ok(());
        }

        // Check that the message is for the current view
        if msg.info().view != state.view {
            return Err(PbftError::InvalidMessage(format!(
                "Node is on view {}, but a PrePrepare for view {} was received",
                state.view,
                msg.info().view,
            )));
        }

        // Check that no `PrePrepare`s already exist with this view and sequence number but a
        // different block; if this is violated, the primary is faulty so initiate a view change
        let mismatched_blocks = self
            .msg_log
            .get_messages_of_type_seq_view(
                PbftMessageType::PrePrepare,
                msg.info().seq_num,
                msg.info().view,
            )
            .iter()
            .filter_map(|existing_msg| {
                let block_id = existing_msg.get_block_id();
                if block_id != msg.get_block_id() {
                    Some(block_id)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        if !mismatched_blocks.is_empty() {
            self.start_view_change(state, state.view + 1)?;
            return Err(PbftError::FaultyPrimary(format!(
                "When checking PrePrepare with block {:?}, found PrePrepare(s) with same view and \
                 seq num but mismatched block(s): {:?}",
                hex::encode(msg.get_block_id()),
                mismatched_blocks,
            )));
        }

        // Add message to the log
        self.msg_log.add_message(msg.clone());

        // If the node is in the PrePreparing phase, this message is for the current sequence
        // number, and the node already has this block: switch to Preparing
        self.try_preparing(msg.get_block_id(), state)
    }

    /// Handle a `Prepare` message
    ///
    /// Once a `Prepare` for the current sequence number is accepted and added to the log, the node
    /// will check if it has the required 2f + 1 `Prepared` messages to move on to the Committing
    /// phase
    fn handle_prepare(
        &mut self,
        msg: ParsedMessage,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        info!(target: "consensus::cl","handle_prepare: state {}", state);
        let info = msg.info().clone();
        let block_id = msg.get_block_id();

        // Check that the message is for the current view
        if msg.info().view != state.view {
            return Err(PbftError::InvalidMessage(format!(
                "Node is on view {}, but a Prepare for view {} was received",
                state.view,
                msg.info().view,
            )));
        }

        // The primary is not allowed to send a Prepare; its PrePrepare counts as its "vote"
        if info.signer_id == state.get_primary_id() {
            self.start_view_change(state, state.view + 1)?;
            return Err(PbftError::FaultyPrimary(format!(
                "Received Prepare from primary at view {}, seq_num {}",
                state.view, state.seq_num
            )));
        }

        self.msg_log.add_message(msg);

        // If this message is for the current sequence number and the node is in the Preparing
        // phase, check if the node is ready to move on to the Committing phase
        if info.seq_num == state.seq_num && state.phase == PbftPhase::Preparing {
            // The node is ready to move on to the Committing phase (i.e. the predicate `prepared`
            // is true) when its log has 2f + 1 Prepare messages from different nodes that match
            // the PrePrepare message received earlier (same view, sequence number, and block
            let has_matching_pre_prepare =
                self.msg_log.has_pre_prepare(info.seq_num, info.view, block_id);
            let has_required_prepares = self
                .msg_log
                // Only get Prepares with matching seq_num, view, and block_id
                .get_messages_of_type_seq_view_block(
                    PbftMessageType::Prepare,
                    info.seq_num,
                    info.view,
                    block_id,
                )
                // Check if there are at least 2f + 1 Prepares
                .len() as u64
                > 2 * state.f;
            if has_matching_pre_prepare && has_required_prepares {
                state.switch_phase(PbftPhase::Committing)?;
                info!(target: "consensus::cl","Broadcasting Commit");
                self.broadcast_pbft_message(
                    state.view,
                    state.seq_num,
                    PbftMessageType::Commit,
                    block_id,
                    state,
                )?;
            }
        }

        Ok(())
    }

    /// Handle a `Commit` message
    ///
    /// Once a `Commit` for the current sequence number is accepted and added to the log, the node
    /// will check if it has the required 2f + 1 `Commit` messages to actually commit the block
    fn handle_commit(
        &mut self,
        msg: ParsedMessage,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        info!(target: "consensus::cl","handle_commit: state {}", state);
        let info = msg.info().clone();
        let block_id = msg.get_block_id();

        // Check that the message is for the current view
        if msg.info().view != state.view {
            return Err(PbftError::InvalidMessage(format!(
                "Node is on view {}, but a Commit for view {} was received",
                state.view,
                msg.info().view,
            )));
        }

        self.msg_log.add_message(msg);

        // If this message is for the current sequence number and the node is in the Committing
        // phase, check if the node is ready to commit the block
        if info.seq_num == state.seq_num && state.phase == PbftPhase::Committing {
            // The node is ready to commit the block (i.e. the predicate `committable` is true)
            // when its log has 2f + 1 Commit messages from different nodes that match the
            // PrePrepare message received earlier (same view, sequence number, and block)
            let has_matching_pre_prepare =
                self.msg_log.has_pre_prepare(info.seq_num, info.view, block_id);
            let has_required_commits = self
                .msg_log
                // Only get Commits with matching seq_num, view, and block_id
                .get_messages_of_type_seq_view_block(
                    PbftMessageType::Commit,
                    info.seq_num,
                    info.view,
                    block_id,
                )
                // Check if there are at least 2f + 1 Commits
                .len() as u64
                > 2 * state.f;

            if has_matching_pre_prepare && has_required_commits {
                let payload = self.service.commit_block(block_id.clone()).map_err(|err| {
                    PbftError::ServiceError(
                        format!("Failed to commit block {:?}", hex::encode(&block_id)),
                        err.to_string(),
                    )
                })?;
                self.agent.push_block_event(ClayerConsensusEvent::BlockCommit((
                    block_id,
                    payload.execution_payload.payload_inner.timestamp,
                    true,
                )));

                state.switch_phase(PbftPhase::Finishing(false))?;
                // Stop the commit timeout, since the network has agreed to commit the block
                state.commit_timeout.stop();

                // state.last_block_timestamp = payload.execution_payload.payload_inner.timestamp;
                // self.on_block_commit(block_id, state)?;

                //broadcast new block hash
                self.broadcast_pbft_message(
                    state.view,
                    state.seq_num,
                    PbftMessageType::AnnounceBlock,
                    block_id,
                    state,
                )?;
            }
        }

        Ok(())
    }

    /// Handle a `ViewChange` message
    ///
    /// When a `ViewChange` is received, check that it isn't outdated and add it to the log. If the
    /// node isn't already view changing but it now has f + 1 ViewChange messages, start view
    /// changing early. If the node is the primary and has 2f view change messages now, broadcast
    /// the NewView message to the rest of the nodes to move to the new view.
    fn handle_view_change(
        &mut self,
        msg: &ParsedMessage,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        // Ignore old view change messages (already on a view >= the one this message is
        // for or already trying to change to a later view)
        let msg_view = msg.info().view;
        if msg_view <= state.view
            || match state.mode {
                PbftMode::ViewChanging(v) => msg_view < v,
                _ => false,
            }
        {
            debug!(target: "consensus::cl","Ignoring stale view change message for view {}", msg_view);
            return Ok(());
        }

        self.msg_log.add_message(msg.clone());

        // Even if the node hasn't detected a faulty primary yet, start view changing if there are
        // f + 1 ViewChange messages in the log for this proposed view (but if already view
        // changing, only do this for a later view); this will prevent starting the view change too
        // late
        let is_later_view = match state.mode {
            PbftMode::ViewChanging(v) => msg_view > v,
            PbftMode::Normal => true,
        };
        let start_view_change = self
            .msg_log
            // Only get ViewChanges with matching view
            .get_messages_of_type_view(PbftMessageType::ViewChange, msg_view)
            // Check if there are at least f + 1 ViewChanges
            .len() as u64
            > state.f;
        if is_later_view && start_view_change {
            info!(target: "consensus::cl","{}: Received f + 1 ViewChange messages; starting early view change", state);
            // Can exit early since the node will self-send another ViewChange message here
            return self.start_view_change(state, msg_view);
        }

        let messages =
            self.msg_log.get_messages_of_type_view(PbftMessageType::ViewChange, msg_view);

        // If there are 2f + 1 ViewChange messages and the view change timeout is not already
        // started, update the timeout and start it
        if !state.view_change_timeout.is_active() && messages.len() as u64 > state.f * 2 {
            state.view_change_timeout = Timeout::new(
                state
                    .view_change_duration
                    .checked_mul((msg_view - state.view) as u32)
                    .expect("View change timeout has overflowed"),
            );
            state.view_change_timeout.start();
        }

        // If this node is the new primary and the required 2f ViewChange messages (not including
        // the primary's own) are present in the log, broadcast the NewView message
        let messages_from_other_nodes =
            messages.iter().filter(|msg| !msg.from_self).cloned().collect::<Vec<_>>();

        if state.is_primary_at_view(msg_view)
            && messages_from_other_nodes.len() as u64 >= 2 * state.f
        {
            let new_view = PbftNewView {
                info: PbftMessageInfo {
                    ptype: PbftMessageType::NewView as u8,
                    view: msg_view,
                    seq_num: state.seq_num - 1,
                    signer_id: state.id.clone(),
                },
                view_changes: Self::signed_votes_from_messages(
                    messages_from_other_nodes.as_slice(),
                ),
            };

            trace!(target: "consensus::cl","Created NewView message: {:?}", new_view);

            self.broadcast_message(
                ParsedMessage::from_new_view_message(new_view),
                state,
                false,
                true,
            )?;
        }

        Ok(())
    }

    /// Handle a `NewView` message
    ///
    /// When a `NewView` is received, verify that it is valid; if it is, update the view and the
    /// node's state.
    fn handle_new_view(
        &mut self,
        msg: &ParsedMessage,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        let new_view = msg.get_new_view();

        match self.verify_new_view(new_view, state) {
            Ok(_) => trace!(target: "consensus::cl","NewView passed verification"),
            Err(err) => {
                return Err(PbftError::InvalidMessage(format!(
                    "NewView failed verification - Error was: {}",
                    err
                )));
            }
        }

        // If this node was the primary before, cancel any block that may have been initialized
        if state.is_primary() {
            self.service.cancel_block().unwrap_or_else(|err| {
                info!(target: "consensus::cl","Failed to cancel block when becoming secondary: {:?}", err);
            });
        }

        // Update view
        state.view = new_view.info.view;
        state.view_change_timeout.stop();

        info!(target: "consensus::cl","{}: Updated to view {}", state, state.view);

        // Reset state to Normal mode, reset the phase (unless waiting for a BlockCommit) and
        // restart the idle timeout
        state.mode = PbftMode::Normal;
        if !matches!(state.phase, PbftPhase::Finishing(_)) {
            state.phase = PbftPhase::PrePreparing;
        }
        state.idle_timeout.start();

        // Initialize a new block if this node is the new primary
        if state.is_primary() {
            self.service.initialize_block(None).map_err(|err| {
                PbftError::ServiceError(
                    "Couldn't initialize block after view change".into(),
                    err.to_string(),
                )
            })?;
        }

        Ok(())
    }

    /// Handle a `SealRequest` message
    ///
    /// A node is requesting a consensus seal for the last block. If the block was the last one
    /// committed by this node, build the seal and send it to the requesting node; if the block has
    /// not been committed yet but it's the next one to be committed, add the request to the log
    /// and the node will build/send the seal when it's done committing. If this is an older block
    /// (state.seq_num > msg.seq_num + 1) or this node is behind (state.seq_num < msg.seq_num), the
    /// node will not be able to build the requseted seal, so just ignore the message.
    fn handle_seal_request(
        &mut self,
        peer_id: PeerId,
        msg: ParsedMessage,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        if state.seq_num == msg.info().seq_num + 1 {
            return self.send_seal_response(state, &msg.info().signer_id);
        } else if state.seq_num == msg.info().seq_num {
            self.msg_log.add_message(msg);
        } else if state.seq_num > msg.info().seq_num + 1 {
            //for sync seal
            let result = self.load_seal(msg.get_pbft().block_id)?;
            if let Some(seal) = result {
                return self.send_seal_response_v2(state, &peer_id, &seal);
            } else {
                warn!(target: "consensus::cl","can not load seal from block {}", msg.get_pbft().block_id);
            }
        }
        Ok(())
    }

    /// Handle a `Seal` message
    ///
    /// A node has responded to the seal request by sending a seal for the last block; validate the
    /// seal and commit the block.
    fn handle_seal_response(
        &mut self,
        msg: &ParsedMessage,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        let seal = msg.get_seal();
        trace!(target: "consensus::cl","{}:trace sync: Received seal response for block {:?}", state, hex::encode(&seal.block_id));

        // If the node has already committed the block, ignore
        if let PbftPhase::Finishing(_) = state.phase {
            trace!(target: "consensus::cl","{}:trace sync: Received seal response for block {:?}, but return ok", state, hex::encode(&seal.block_id));
            return Ok(());
        }

        // Get the previous ID of the block this seal is for so it can be used to verify the seal
        let previous_id = self
            .msg_log
            .get_block_with_id(seal.block_id)
            // Make sure the node actually has the block
            .ok_or_else(|| {
                PbftError::InvalidMessage(format!(
                    "Received a seal for a block ({:?}) that the node does not have",
                    hex::encode(&seal.block_id),
                ))
            })
            .and_then(|block| {
                // Make sure the block is at the node's current sequence number
                if block.block_num() != state.seq_num {
                    Err(PbftError::InvalidMessage(format!(
                        "Received a seal for block {:?}, but block_num does not match node's \
                         seq_num: {} != {}",
                        hex::encode(&seal.block_id),
                        block.block_num(),
                        state.seq_num,
                    )))
                } else {
                    Ok(block.previous_id().clone())
                }
            })?;

        // Verify the seal
        match self.verify_consensus_seal(seal, previous_id, state) {
            Ok(_) => {
                trace!(target: "consensus::cl","Consensus seal passed verification");
            }
            Err(err) => {
                return Err(PbftError::InvalidMessage(format!(
                    "Consensus seal failed verification - Error was: {}",
                    err
                )));
            }
        }

        // Catch up
        self.catchup(state, seal, false)
    }

    /// Handle a `announceblock` message
    fn handle_announceblock_response(
        &mut self,
        msg: &ParsedMessage,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        let blockhash = msg.get_block_id();
        if !self.announce_block.contains_key(&blockhash) {
            self.announce_block.insert(blockhash, msg.info().seq_num);
            let _ = self.service_mut().announce_block(blockhash);

            self.broadcast_pbft_message(
                state.view,
                state.seq_num,
                PbftMessageType::AnnounceBlock,
                blockhash,
                state,
            )?;

            let state_seq_num = state.seq_num;
            let latest_header = self.client.latest_header().ok().flatten();
            if let Some(latest_header) = latest_header {
                let latest_number = latest_header.header.number;

                trace!(target: "consensus::cl", "trace sync:announceblock {} latest_number {} state_seq_num {}",msg.info().seq_num,latest_number,state_seq_num);
            }
        };

        Ok(())
    }

    fn handle_new_validator(
        &mut self,
        msg: &ParsedMessage,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        let peerid = msg.get_new_validator();

        info!(target: "consensus::cl","trace sync handle_new_validator: state.becoming_validator {}",state.becoming_validator);

        if state.id == peerid && state.becoming_validator == false {
            state.becoming_validator = true;
            // self.sync_seal(state)?
        }
        Ok(())
    }

    pub fn sync_seal(&mut self, state: &mut PbftState) -> Result<(), PbftError> {
        let now: u64 = chrono::prelude::Local::now().timestamp() as u64;
        let interval = now - state.last_send_seal_timestamp;
        if state.block_publishing_min_interval.as_secs() > interval {
            return Ok(());
        }
        state.last_send_seal_timestamp = now;

        if state.is_validator() || state.becoming_validator == true {
            let mut request_count: u64 = 0;
            let state_seq_num = state.seq_num;
            let mut has_send_seal = state.has_send_seal;
            let latest_db_header = self.client.latest_header().ok().flatten();
            if let Some(latest_db_header) = latest_db_header {
                if latest_db_header.number >= state_seq_num {
                    //info!(target: "consensus::cl","trace sync state_seq_num {}, latest_db_header.number {} has_send_seal {}",state_seq_num,latest_db_header.number,has_send_seal);
                    for seq in state_seq_num..=latest_db_header.number {
                        // info!(target: "consensus::cl","trace sync:seal reqeust seq {} has_send_seal {} ====",seq,has_send_seal);
                        if seq > has_send_seal {
                            let header =
                                self.client.sealed_header_by_id(BlockId::from(seq)).ok().flatten();
                            if let Some(header) = header {
                                info!(target: "consensus::cl", "trace sync:seal reqeust {} {} has_send_seal {} latest_number {} state_seq_num {}",header.number,header.hash,has_send_seal,latest_db_header.number,state_seq_num);
                                self.send_seal_request(state, header.hash)?;
                                self.msg_log.add_validated_block(clayer_block_from_header(&header));
                                // info!(target: "consensus::cl", "trace sync:seal reqeust sended");
                                has_send_seal = header.number;
                                request_count = request_count + 1;
                                if request_count >= 100 {
                                    break;
                                }
                            } else {
                                warn!(target: "consensus::cl", "trace sync:seal reqeust {} header not found",seq);
                                break;
                            }
                        }
                    }
                    state.has_send_seal = has_send_seal;
                }
            }
        }
        Ok(())
    }

    /// Handle a `BlockNew` update from the Validator
    ///
    /// The validator has received a new block; check if it is a block that should be considered,
    /// add it to the log as an unvalidated block, and instruct the validator to validate it.
    pub fn on_block_new(
        &mut self,
        block: ClayerBlock,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        info!(target: "consensus::cl",
            "{}: Got BlockNew: {} / {}",
            state,
            block.block_num(),
            hex::encode(&block.block_id())
        );
        trace!(target: "consensus::cl","Block details: {:?}", block);

        // Only future blocks should be considered since committed blocks are final
        if block.block_num() < state.seq_num {
            self.service.fail_block(block.block_id()).unwrap_or_else(
                |err| error!(target: "consensus::cl","Couldn't fail block due to error: {:?}", err),
            );
            return Err(PbftError::InternalError(format!(
                "Received block {:?} / {:?} that is older than the current sequence number: {:?}",
                block.block_num(),
                hex::encode(&block.block_id()),
                state.seq_num,
            )));
        }

        // Make sure the node already has the previous block, since the consensus seal can't be
        // verified without it
        let previous_block = self
            .msg_log
            .get_block_with_id(block.previous_id())
            .or_else(|| self.msg_log.get_unvalidated_block_with_id(&block.previous_id()));
        if previous_block.is_none() {
            self.service.fail_block(block.block_id().clone()).unwrap_or_else(
                |err| error!(target: "consensus::cl","Couldn't fail block due to error: {:?}", err),
            );
            return Err(PbftError::InternalError(format!(
                "Received block {:?} / {:?} but node does not have previous block {:?}",
                block.block_num(),
                hex::encode(&block.block_id()),
                hex::encode(&block.previous_id()),
            )));
        }

        // Make sure that the previous block has the previous block number (enforces that blocks
        // are strictly monotically increasing by 1)
        let previous_block = previous_block.expect("Previous block's existence already checked");
        if previous_block.block_num() != block.block_num() - 1 {
            self.service.fail_block(block.block_id()).unwrap_or_else(
                |err| error!(target: "consensus::cl","Couldn't fail block due to error: {:?}", err),
            );
            return Err(PbftError::InternalError(format!(
                "Received block {:?} / {:?} but its previous block ({:?} / {:?}) does not have \
                 the previous block_num",
                block.block_num(),
                hex::encode(&block.block_id()),
                block.block_num() - 1,
                hex::encode(&block.previous_id()),
            )));
        }

        // Add the currently unvalidated block to the log
        self.msg_log.add_unvalidated_block(block.clone());

        if block.payload_id == B64::ZERO {
            return Err(PbftError::InternalError(format!("BlockNew payload_id is zero")));
        }
        // Have the validator check the block
        self.service
            .check_blocks(
                PayloadId::from(block.payload_id),
                execution_payload_to_payload(&block.block),
                state.is_primary(),
            )
            .map_err(|err| {
                PbftError::ServiceError(
                    format!(
                        "Failed to check block {:?} / {:?}",
                        block.block_num(),
                        hex::encode(&block.block_id()),
                    ),
                    err.to_string(),
                )
            })?;

        self.on_block_valid(block.block_id(), state)?;

        Ok(())
    }

    /// Handle a `BlockValid` update from the Validator
    ///
    /// The block has been verified by the validator, so mark it as validated in the log and
    /// attempt to handle the block.
    pub fn on_block_valid(
        &mut self,
        block_id: B256,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        info!(target: "consensus::cl","Got BlockValid: {}", hex::encode(&block_id));

        // Mark block as validated in the log and get the block
        let block = self.msg_log.block_validated(block_id.clone()).ok_or_else(|| {
            PbftError::InvalidMessage(format!(
                "Received BlockValid message for an unknown block: {}",
                hex::encode(&block_id)
            ))
        })?;

        self.try_handling_block(block, state)
    }

    /// Validate the block's seal and handle the block. If this is the block the node is waiting
    /// for and this node is the primary, broadcast a PrePrepare; if the node isn't the primary but
    /// it already has the PrePrepare for this block, switch to `Preparing`. If this is a future
    /// block, use it to catch up.
    fn try_handling_block(
        &mut self,
        block: ClayerBlock,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        // If the block's number is higher than the current sequence number + 1 (i.e., it is newer
        // than the grandchild of the last committed block), the seal cannot be verified; this is
        // because the settings in a block's grandparent are needed to verify the block's seal, and
        // these settings are only guaranteed to be in the validator's state when the block is
        // committed. If this is a newer block, wait until after the grandparent is committed
        // before validating the seal and handling the block.

        if block.block_num() > state.seq_num + 1 {
            return Ok(());
        }

        let seal = match self.verify_consensus_seal_from_block(&block, state) {
            Ok(seal) => seal,
            Err(err) => {
                self.service.fail_block(block.block_id()).unwrap_or_else(
                    |err| error!(target: "consensus::cl","Couldn't fail block due to error: {:?}", err),
                );
                return Err(PbftError::InvalidMessage(format!(
                    "Consensus seal failed verification - Error was: {}",
                    err
                )));
            }
        };

        // This block's seal can be used to commit the block previous to it (i.e. catch-up) if it's
        // a future block and the node isn't waiting for a commit message for a previous block (if
        // it is waiting for a commit message, catch-up will have to be done after the message is
        // received)
        let is_waiting = matches!(state.phase, PbftPhase::Finishing(_));
        // block.block_num == state.seq_num + 1  &&  state.phase ！= PbftPhase::Finishing
        if block.block_num() > state.seq_num && !is_waiting {
            self.catchup(state, &seal, true)?;
        } else if block.block_num() == state.seq_num {
            if block.info.signer_id == state.id && state.is_primary() {
                // This is the next block and this node is the primary; broadcast PrePrepare
                // messages
                info!(target: "consensus::cl","Broadcasting PrePrepare");
                self.broadcast_pbft_message(
                    state.view,
                    state.seq_num,
                    PbftMessageType::PrePrepare,
                    block.block_id(),
                    state,
                )?;
            } else {
                // If the node is in the PrePreparing phase and it already has a PrePrepare for
                // this block: switch to Preparing
                self.try_preparing(block.block_id(), state)?;
            }
        }

        Ok(())
    }

    /// Handle a `BlockInvalid` update from the Validator
    ///
    /// The block is invalid, so drop it from the log and fail it.
    pub fn on_block_invalid(&mut self, block_id: B256) -> Result<(), PbftError> {
        info!(target: "consensus::cl","Got BlockInvalid: {}", hex::encode(&block_id));

        // Drop block from the log
        if !self.msg_log.block_invalidated(block_id.clone()) {
            return Err(PbftError::InvalidMessage(format!(
                "Received BlockInvalid message for an unknown block: {}",
                hex::encode(&block_id)
            )));
        }

        // Fail the block
        self.service.fail_block(block_id).unwrap_or_else(
            |err| error!(target: "consensus::cl","Couldn't fail block due to error: {:?}", err),
        );

        Ok(())
    }

    /// Use the given consensus seal to verify and commit the block this node is working on
    fn catchup(
        &mut self,
        state: &mut PbftState,
        seal: &PbftSeal,
        catchup_again: bool,
    ) -> Result<(), PbftError> {
        info!(target: "consensus::cl","{}: Attempting to commit block {} using catch-up. trace sync:seal {}", state, state.seq_num,seal);

        let messages = seal.commit_votes.iter().try_fold(Vec::new(), |mut msgs, vote| {
            msgs.push(ParsedMessage::from_signed_vote(vote)?);
            Ok(msgs)
        })?;

        // Update view if necessary
        let view = messages[0].info().view;
        if view != state.view {
            info!(target: "consensus::cl","Updating view from {} to {}", state.view, view);
            state.view = view;
        }

        // Add messages to the log
        for message in &messages {
            self.msg_log.add_message(message.clone());
        }

        let latest_header = self.client.latest_header().ok().flatten();
        let latest_number = if let Some(latest_header) = latest_header {
            latest_header.header.number
        } else {
            // Commit the block, stop the idle timeout, and skip straight to Finishing
            let payload = self.service.commit_block(seal.block_id.clone()).map_err(|err| {
                PbftError::ServiceError(
                    format!(
                        "Failed to commit block with catch-up {:?} / {:?}",
                        state.seq_num,
                        hex::encode(&seal.block_id)
                    ),
                    err.to_string(),
                )
            })?;
            payload.execution_payload.payload_inner.timestamp
        };

        let header = self.client.header_by_id(BlockId::from(seal.block_id.clone())).ok().flatten();
        let timestamp = if let Some(header) = header {
            header.timestamp
        } else {
            return Err(PbftError::InternalError(format!(
                "Failed to get header with catch-up {}",
                seal.info.seq_num,
            )));
        };
        trace!(target: "consensus::cl","trace sync: catch-up latest_number {} state.seq_num {} seal seq_num {}", latest_number, state.seq_num,seal.info.seq_num);

        // // Commit the block, stop the idle timeout, and skip straight to Finishing
        // let _ = self.service.commit_block(seal.block_id.clone()).map_err(|err| {
        //     PbftError::ServiceError(
        //         format!(
        //             "Failed to commit block with catch-up {:?} / {:?}",
        //             state.seq_num,
        //             hex::encode(&seal.block_id)
        //         ),
        //         err.to_string(),
        //     )
        // })?;

        self.agent.push_block_event(ClayerConsensusEvent::BlockCommit((
            seal.block_id.clone(),
            timestamp,
            false,
        )));

        state.idle_timeout.stop();
        state.phase = PbftPhase::Finishing(catchup_again);

        // state.last_block_timestamp = payload.execution_payload.payload_inner.timestamp;
        // self.on_block_commit(seal.block_id.clone(), state)?;
        self.save_seal(seal)?;

        Ok(())
    }

    /// Handle a `BlockCommit` update from the Validator
    ///
    /// A block was sucessfully committed; clean up any uncommitted blocks, update state to be
    /// ready for the next block, make any necessary view and membership changes, garbage collect
    /// the logs, and start a new block if this node is the primary.
    pub fn on_block_commit(
        &mut self,
        block_id: B256,
        timestamp: u64,
        committing: bool,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        let is_catching_up = matches!(state.phase, PbftPhase::Finishing(true));
        info!(target: "consensus::cl","{}: Got BlockCommit for {} from committing({}) is_catching_up {}", state, hex::encode(&block_id),committing,is_catching_up);

        // If there are any blocks in the log at this sequence number other than the one that was
        // just committed, reject them
        let invalid_block_ids =
            self.msg_log
                .get_blocks_with_num(state.seq_num)
                .iter()
                .filter_map(|block| {
                    if block.block_id() != block_id {
                        Some(block.block_id())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();

        for id in invalid_block_ids {
            self.service.fail_block(id.clone()).unwrap_or_else(|err| {
                error!(target: "consensus::cl","Couldn't fail block {:?} due to error: {:?}", &hex::encode(id), err)
            });
        }

        // Increment sequence number and update state
        state.seq_num += 1;
        state.mode = PbftMode::Normal;
        state.phase = PbftPhase::PrePreparing;
        state.chain_head = block_id.clone();
        state.last_block_timestamp = timestamp;

        // create the seal
        if committing {
            let seal = self.build_seal(state).map_err(|err| {
                PbftError::InternalError(format!("Failed to build requested seal due to: {}", err))
            })?;
            self.save_seal(&seal)?;
        }

        // If node(s) are waiting for a seal to commit the last block, send it now
        let requesters = self
            .msg_log
            .get_messages_of_type_seq(PbftMessageType::SealRequest, state.seq_num - 1)
            .iter()
            .map(|req| req.info().signer_id)
            .collect::<Vec<_>>();

        for req in requesters {
            self.send_seal_response(state, &req).unwrap_or_else(|err| {
                error!(target: "consensus::cl","Failed to send seal response due to: {:?}", err);
            });
        }

        // Update membership if necessary
        self.update_membership(block_id.clone(), state.seq_num - 1, state);

        // Increment the view if a view change must be forced for fairness view
        if state.at_forced_view_change() {
            state.view += 1;
        }

        // Tell the log to garbage collect if it needs to
        self.msg_log.garbage_collect(state.seq_num);

        // If the node already has grandchild(ren) of the block that was just committed, one of
        // them may be used to perform catch-up to commit the next block.
        // commit block (state.seq_num - 1)
        // current public block (state.seq_num)
        // grandchildren (state.seq_num + 1)
        let grandchildren = self
            .msg_log
            .get_blocks_with_num(state.seq_num + 1)
            .iter()
            .cloned()
            .cloned()
            .collect::<Vec<_>>();
        for block in grandchildren {
            if self.try_handling_block(block, state).is_ok() {
                return Ok(());
            }
        }

        // If the node is catching up but doesn't have a block with a seal to commit the next one,
        // it will need to request the seal to commit the last block. The node doesn't know which
        // block that the network decided to commit, so it can't request the seal for a specific
        // block (puts an empty BlockId in the message)
        if is_catching_up {
            info!(target: "consensus::cl","{}: Requesting seal to finish catch-up to block {}", state, state.seq_num);
            return self.broadcast_pbft_message(
                state.view,
                state.seq_num,
                PbftMessageType::SealRequest,
                B256::default(),
                state,
            );
        }

        // Start the idle timeout for the next block
        state.idle_timeout.start();

        // If we already have a block at this sequence number with a valid PrePrepare for it, start
        // Preparing (there may be multiple blocks, but only one will have a valid PrePrepare)
        let block_ids = self
            .msg_log
            .get_blocks_with_num(state.seq_num)
            .iter()
            .map(|block| block.block_id())
            .collect::<Vec<_>>();
        for id in block_ids {
            self.try_preparing(id, state)?;
        }

        // Initialize a new block if this node is the primary and it is not in the process of
        // catching up
        if state.is_primary() {
            info!(target: "consensus::cl","{}: Initializing block on top of {}", state, hex::encode(&block_id));
            self.service.initialize_block(Some(block_id)).map_err(|err| {
                PbftError::ServiceError(
                    "Couldn't initialize block after commit".into(),
                    err.to_string(),
                )
            })?;
        }
        info!(target: "consensus::cl","==================================on_block_commit over {}======================================",state.seq_num-1);
        Ok(())
    }

    /// Check the on-chain list of members; if it has changed, update members list and return true.
    ///
    /// # Panics
    /// + If the `sawtooth.consensus.pbft.members` setting is unset or invalid
    /// + If the network this node is on does not have enough nodes to be Byzantine fault tolernant
    fn update_membership(&mut self, block_id: B256, block_number: u64, state: &mut PbftState) {
        trace!(target: "consensus::cl","Updating membership for block {}",block_id);

        let last_is_validator = state.is_validator();

        // let on_chain_members = state.validators.member_ids().clone();

        let on_chain_members =
            retry_until_ok(state.exponential_retry_base, state.exponential_retry_max, || {
                self.service.query_validators(ELECT_VOTING_ADDRESS.to_string(), block_number)
            });

        let on_chain_members: Vec<PeerId> =
            assemble_peer_id(on_chain_members).unwrap_or_else(|err| {
                error!(target: "consensus::cl","Failed to parse members on-chain: {}", err);
                state.validators.member_ids().clone()
            });

        let (add_or_sub, peerid) = state.validators.compare(&on_chain_members);
        if !state.validators.is_same(&on_chain_members) {
            info!(target: "consensus::cl","Updating membership: {:?}", on_chain_members);
            state.validators.update(&on_chain_members);
            let f = (state.validators.len() - 1) / 3;
            if f == 0 {
                panic!("This network no longer contains enough nodes to be fault tolerant");
            }
            state.f = f as u64;
        }

        // broadcast to new validator
        if last_is_validator && state.is_validator() && add_or_sub > 0 {
            let info = PbftMessageInfo {
                ptype: PbftMessageType::NewValidator as u8,
                view: state.view,
                seq_num: block_number,
                signer_id: state.id.clone(),
            };
            let msg = PbftNewValidator { info, peerid };
            let _ = self.broadcast_message(
                ParsedMessage::from_new_validator_message(msg),
                state,
                false,
                true,
            );
        }

        if state.is_validator() && state.becoming_validator {
            state.becoming_validator = false;
        }
    }

    /// When the node has a block and a corresponding PrePrepare for its current sequence number,
    /// and it is in the PrePreparing phase, it can enter the Preparing phase and broadcast its
    /// Prepare
    fn try_preparing(&mut self, block_id: B256, state: &mut PbftState) -> Result<(), PbftError> {
        info!(target: "consensus::cl","try_preparing: state {}", state);
        if let Some(block) = self.msg_log.get_block_with_id(block_id) {
            if state.phase == PbftPhase::PrePreparing
                && self.msg_log.has_pre_prepare(state.seq_num, state.view, block_id)
                // PrePrepare.seq_num == state.seq_num == block.block_num enforces the one-to-one
                // correlation between seq_num and block_num (PrePrepare n should be for block n)
                && block.block_num() == state.seq_num
            {
                state.switch_phase(PbftPhase::Preparing)?;

                // Stop idle timeout, since a new block and valid PrePrepare were received in time
                state.idle_timeout.stop();

                // Now start the commit timeout in case the network fails to commit the block
                // within a reasonable amount of time
                state.commit_timeout.start();

                // The primary doesn't broadcast a Prepare; its PrePrepare counts as its "vote"
                info!(target: "consensus::cl","Broadcasting Prepare");
                if !state.is_primary() {
                    self.broadcast_pbft_message(
                        state.view,
                        state.seq_num,
                        PbftMessageType::Prepare,
                        block_id,
                        state,
                    )?;
                }
            }
        }

        Ok(())
    }

    /// Handle a `PeerConnected` update from the Validator
    ///
    /// A peer has just connected to this node. Send a bootstrap commit message if the peer is part
    /// of the network and the node isn't at the genesis block.
    pub fn on_peer_connected(
        &mut self,
        peer_id: PeerId,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        // Ignore if the peer is not a member of the PBFT network or the chain head is block 0
        if !state.validators.contains(&peer_id)
            || state.seq_num == 0
            || !state.validators.contains(&state.id)
        {
            return Ok(());
        }

        self.broadcast_bootstrap_commit(peer_id, state)
    }

    /// When the whole network is starting "fresh" from a non-genesis block, none of the nodes will
    /// have the `Commit` messages necessary to build the consensus seal for the last committed
    /// block (the chain head). To bootstrap the network in this scenario, all nodes will send a
    /// `Commit` message for their chain head whenever one of the PBFT members connects; when
    /// > 2f + 1 nodes have connected and received these `Commit` messages, the nodes will be able
    /// to build a seal using the messages.
    fn broadcast_bootstrap_commit(
        &mut self,
        peer_id: PeerId,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        // The network must agree on a single view number for the Commit messages, so the view
        // of the chain head's predecessor is used. For block 1 this is view 0; otherwise, it's the
        // view of the block's consensus seal
        info!(target: "consensus::cl","broadcast_bootstrap_commit: state {}", state);
        let view = if state.seq_num == 1 {
            0
        } else {
            self.msg_log
                .get_block_with_id(state.chain_head)
                .ok_or_else(|| {
                    PbftError::InternalError(format!(
                        "Node does not have chain head ({:?}) in its log",
                        state.chain_head
                    ))
                })
                .and_then(|block| {
                    PbftSeal::decode(&mut block.seal_bytes.to_vec().as_slice()).map_err(|err| {
                        PbftError::SerializationError(
                            "Error parsing seal from chain head".into(),
                            err.to_string(),
                        )
                    })
                })?
                .info
                .view
        };

        // Construct the commit message for the chain head and send it to the connected peer

        let info = PbftMessageInfo {
            ptype: PbftMessageType::Commit as u8,
            view,
            seq_num: state.seq_num - 1,
            signer_id: state.id.clone(),
        };
        let commit = PbftMessage { info, block_id: state.chain_head.clone() };

        let mut msg_out = vec![];
        commit.encode(&mut msg_out);
        let message_bytes = reth_primitives::Bytes::copy_from_slice(msg_out.as_slice());

        //create header
        let header = ClayerConsensusMessageHeader {
            message_type: commit.info.ptype,
            content_hash: keccak256(&message_bytes),
            signer_id: state.id.clone(),
        };
        let mut header_out = vec![];
        header.encode(&mut header_out);
        let header_bytes = reth_primitives::Bytes::copy_from_slice(header_out.as_slice());

        //sign header
        let signature_hash = keccak256(&header_bytes);
        let signature =
            sign_message(B256::from_slice(&state.kp.secret_bytes()[..]), signature_hash).map_err(
                |err| PbftError::SigningError(format!("signing header error: {}", err.to_string())),
            )?;

        let clayer_msg = ClayerConsensusMessage {
            header_bytes,
            header_signature: ClayerSignature(signature),
            message_bytes,
        };
        let mut msg_out = vec![];
        clayer_msg.encode(&mut msg_out);
        let msg_bytes = reth_primitives::Bytes::copy_from_slice(msg_out.as_slice());

        // Send the seal to the requester
        self.agent.broadcast_consensus(vec![peer_id.clone()], msg_bytes);

        Ok(())
    }

    // ---------- Methods for building & verifying proofs and signed messages from other nodes ----------

    /// Generate a `protobuf::RepeatedField` of signed votes from a list of parsed messages
    fn signed_votes_from_messages(msgs: &[&ParsedMessage]) -> Vec<PbftSignedVote> {
        msgs.iter()
            .map(|m| {
                let vote = PbftSignedVote {
                    header_bytes: m.header_bytes.clone(),
                    header_signature: ClayerSignature(m.header_signature.clone()),
                    message_bytes: m.get_message_bytes(),
                };
                vote
            })
            .collect::<Vec<_>>()
    }

    /// Build a consensus seal that proves the last block committed by this node
    fn build_seal(&self, state: &PbftState) -> Result<PbftSeal, PbftError> {
        trace!(target: "consensus::cl","{}: Building seal for block {}", state, state.seq_num - 1);

        // The previous block may have been committed in a different view, so the node will need to
        // find the view that contains the required 2f Commit messages for building the seal
        let (block_id, view, messages) = self
            .msg_log
            .get_messages_of_type_seq(PbftMessageType::Commit, state.seq_num - 1)
            .iter()
            // Filter out this node's own messages because self-sent messages aren't signed and
            // therefore can't be included in the seal
            .filter(|msg| !msg.from_self)
            .cloned()
            // Map to ((block_id, view), msg)
            .map(|msg| ((msg.get_block_id(), msg.info().view), msg))
            // Group messages together by block and view
            .into_group_map()
            .into_iter()
            // One and only one block/view should have the required number of messages, since only
            // one block at this sequence number should have been committed and in only one view
            .find_map(|((block_id, view), msgs)| {
                if msgs.len() as u64 >= 2 * state.f {
                    Some((block_id, view, msgs))
                } else {
                    None
                }
            })
            .ok_or_else(|| {
                PbftError::InternalError(String::from(
                    "Couldn't find 2f commit messages in the message log for building a seal",
                ))
            })?;

        let seal = PbftSeal {
            info: PbftMessageInfo {
                ptype: PbftMessageType::Seal as u8,
                view,
                seq_num: state.seq_num - 1,
                signer_id: state.id.clone(),
            },
            block_id,
            commit_votes: Self::signed_votes_from_messages(messages.as_slice()),
        };

        let vote_ids = messages
            .iter()
            .map(|&p| format!("{}", p.get_pbft().info.signer_id))
            .collect::<Vec<_>>();
        info!(target: "consensus::cl","Seal created: {} vote_ids [{}]", seal,vote_ids.join(" ,"));

        Ok(seal)
    }

    /// Verify that a vote matches the expected type, is properly signed, and passes the specified
    /// criteria; if it passes verification, return the signer ID to be used for further
    /// verification
    fn verify_vote<F>(
        vote: &PbftSignedVote,
        expected_type: PbftMessageType,
        validation_criteria: F,
    ) -> Result<PeerId, PbftError>
    where
        F: Fn(&PbftMessage) -> Result<(), PbftError>,
    {
        // Parse the message
        let pbft_message: PbftMessage =
            PbftMessage::decode(&mut vote.message_bytes.to_vec().as_slice()).map_err(|err| {
                PbftError::SerializationError(
                    "Error parsing PbftMessage from vote".into(),
                    err.to_string(),
                )
            })?;

        let header: ClayerConsensusMessageHeader =
            ClayerConsensusMessageHeader::decode(&mut vote.header_bytes.to_vec().as_slice())
                .map_err(|err| {
                    PbftError::SerializationError(
                        "Error parsing header from vote".into(),
                        err.to_string(),
                    )
                })?;

        trace!("Verifying vote with PbftMessage: {:?} and header: {:?}", pbft_message, header);

        // Verify the header's signer matches the PbftMessage's signer
        if header.signer_id != pbft_message.info.signer_id {
            return Err(PbftError::InvalidMessage(format!(
                "Received a vote where PbftMessage's signer ID ({:?}) and PeerMessage's signer ID \
                 ({:?}) don't match",
                pbft_message.info.signer_id, header.signer_id
            )));
        }

        // Verify the message type
        let msg_type = PbftMessageType::from(pbft_message.info.ptype);
        if msg_type != expected_type {
            return Err(PbftError::InvalidMessage(format!(
                "Received a {:?} vote, but expected a {:?}",
                msg_type, expected_type
            )));
        }

        // Verify the signature
        let pk = id2pk(header.signer_id).map_err(|err| {
            PbftError::SigningError(format!(
                "Couldn't parse public key from signer ID ({:?}) due to error: {:?}",
                header.signer_id, err
            ))
        })?;

        let signature = vote.header_signature.0.clone();
        let signature_hash = keccak256(&vote.header_bytes);
        let recovered = signature
            .recover_signer(signature_hash)
            .ok_or(PbftError::SigningError("Couldn't recover signer from signature".into()))?;
        let expected = public_key_to_address(pk);

        if recovered != expected {
            return Err(PbftError::SigningError(format!(
                "Signature verification failed - Recovered: `{:?}`, Expected: `{:?}`",
                recovered, expected
            )));
        }

        // Verify message_bytes
        if header.content_hash != keccak256(vote.message_bytes.clone()) {
            return Err(PbftError::SigningError(format!(
                "Hash verification failed - Content: `{:?}`, Hash: `{:?}`",
                vote.message_bytes, header.content_hash
            )));
        }

        // Validate against the specified criteria
        validation_criteria(&pbft_message)?;

        Ok(pbft_message.info.signer_id)
    }

    /// Verify that a NewView messsage is valid
    fn verify_new_view(
        &mut self,
        new_view: &PbftNewView,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        // Make sure this is for a future view (prevents re-using old NewView messages)
        if new_view.info.view <= state.view {
            return Err(PbftError::InvalidMessage(format!(
                "Node is on view {}, but received NewView message for view {}",
                state.view, new_view.info.view,
            )));
        }

        // Make sure this is from the new primary
        if new_view.info.signer_id != state.get_primary_id_at_view(new_view.info.view) {
            return Err(PbftError::InvalidMessage(format!(
                "Received NewView message for view {} that is not from the primary for that view",
                new_view.info.view
            )));
        }

        // Verify each individual vote and extract the signer ID from each ViewChange so the IDs
        // can be verified
        let voter_ids =
            new_view.view_changes.iter().try_fold(HashSet::new(), |mut ids, vote| {
                Self::verify_vote(vote, PbftMessageType::ViewChange, |msg| {
                    if msg.info.view != new_view.info.view {
                        return Err(PbftError::InvalidMessage(format!(
                            "ViewChange's view number ({}) doesn't match NewView's view \
                                 number ({})",
                            msg.info.view, new_view.info.view,
                        )));
                    }
                    Ok(())
                })
                .map(|id| ids.insert(id))?;
                Ok(ids)
            })?;

        // All of the votes must come from PBFT members, and the primary can't explicitly vote
        // itself, since broacasting the NewView is an implicit vote. Check that the votes received
        // are from a subset of "members - primary".
        let peer_ids: HashSet<_> = state
            .validators
            .member_ids()
            .iter()
            .cloned()
            .filter(|pid| pid != &new_view.info.signer_id)
            .collect();

        trace!(target: "consensus::cl","Comparing voter IDs ({:?}) with member IDs - primary ({:?})", voter_ids, peer_ids);

        if !voter_ids.is_subset(&peer_ids) {
            return Err(PbftError::InvalidMessage(format!(
                "NewView contains vote(s) from invalid IDs: {:?}",
                voter_ids.difference(&peer_ids).collect::<Vec<_>>()
            )));
        }

        // Check that the NewView contains 2f votes (primary vote is implicit, so total of 2f + 1)
        if (voter_ids.len() as u64) < 2 * state.f {
            return Err(PbftError::InvalidMessage(format!(
                "NewView needs {} votes, but only {} found",
                2 * state.f,
                voter_ids.len()
            )));
        }

        Ok(())
    }

    /// Verify the consensus seal from the current block that proves the previous block and return
    /// the parsed seal
    fn verify_consensus_seal_from_block(
        &mut self,
        block: &ClayerBlock,
        state: &mut PbftState,
    ) -> Result<PbftSeal, PbftError> {
        // Since block 0 is genesis, block 1 is the first that can be verified with a seal; this
        // means that the node won't see a seal until block 2
        if block.block_num() < 2 {
            return Ok(PbftSeal::default());
        }

        // Parse the seal
        if block.seal_bytes.is_empty() {
            return Err(PbftError::InvalidMessage("Block published without a seal".into()));
        }
        let seal: PbftSeal =
            PbftSeal::decode(&mut block.seal_bytes.to_vec().as_slice()).map_err(|err| {
                PbftError::SerializationError(
                    "Error parsing seal for verification".into(),
                    err.to_string(),
                )
            })?;

        // trace!("Parsed seal: {}", seal);

        // Make sure this is the correct seal for the previous block
        if seal.block_id != block.previous_id() {
            return Err(PbftError::InvalidMessage(format!(
                "Seal's ID ({}) doesn't match block's previous ID ({})",
                hex::encode(&seal.block_id),
                hex::encode(&block.previous_id())
            )));
        }

        // Get the previous ID of the block this seal is supposed to prove so it can be used to
        // verify the seal
        let proven_block_previous_id = self
            .msg_log
            .get_block_with_id(seal.block_id)
            .map(|proven_block| proven_block.previous_id())
            .ok_or_else(|| {
                PbftError::InternalError(format!(
                    "Got seal for block {:?}, but block was not found in the log",
                    seal.block_id,
                ))
            })?;

        // Verify the seal itself
        self.verify_consensus_seal(&seal, proven_block_previous_id, state)?;

        Ok(seal)
    }

    /// Verify the given consenus seal
    ///
    /// # Panics
    /// + If the `sawtooth.consensus.pbft.members` setting is unset or invalid
    fn verify_consensus_seal(
        &mut self,
        seal: &PbftSeal,
        previous_id: B256,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        // Verify each individual vote and extract the signer ID from each PbftMessage so the IDs
        // can be verified
        let voter_ids = seal.commit_votes.iter().try_fold(HashSet::new(), |mut ids, vote| {
            Self::verify_vote(vote, PbftMessageType::Commit, |msg| {
                // Make sure all votes are for the right block
                if msg.block_id != seal.block_id {
                    return Err(PbftError::InvalidMessage(format!(
                        "Commit vote's block ID ({:?}) doesn't match seal's ID ({:?})",
                        msg.block_id, seal.block_id
                    )));
                }
                // Make sure all votes are for the right view
                if msg.info.view != seal.info.view {
                    return Err(PbftError::InvalidMessage(format!(
                        "Commit vote's view ({:?}) doesn't match seal's view ({:?})",
                        msg.info.view, seal.info.view
                    )));
                }
                // Make sure all votes are for the right sequence number
                if msg.info.seq_num != seal.info.seq_num {
                    return Err(PbftError::InvalidMessage(format!(
                        "Commit vote's seq_num ({:?}) doesn't match seal's seq_num ({:?})",
                        msg.info.seq_num, seal.info.seq_num
                    )));
                }
                Ok(())
            })
            .map(|id| ids.insert(id))?;
            Ok(ids)
        })?;

        // All of the votes in a seal must come from PBFT members, and the primary can't explicitly
        // vote itself, since building a consensus seal is an implicit vote. Check that the votes
        // received are from a subset of "members - seal creator". Use the list of members from the
        // block previous to the one this seal verifies, since that represents the state of the
        // network at the time this block was voted on.
        trace!(target: "consensus::cl","Getting members for block {} to verify seal",previous_id);
        // get previous_id setting
        // let members = state.validators.member_ids();

        let on_chain_validators =
            retry_until_ok(state.exponential_retry_base, state.exponential_retry_max, || {
                self.service
                    .query_validators(ELECT_VOTING_ADDRESS.to_string(), seal.info.seq_num - 1)
            });
        let members: Vec<PeerId> = assemble_peer_id(on_chain_validators)?;

        // Verify that the seal's signer is a PBFT member
        if !members.contains(&seal.info.signer_id) {
            return Err(PbftError::InvalidMessage(format!(
                "Consensus seal is signed by an unknown peer: {:?}",
                seal.info.signer_id
            )));
        }

        let peer_ids: HashSet<_> =
            members.iter().cloned().filter(|pid| pid.as_slice() != seal.info.signer_id).collect();

        trace!(target: "consensus::cl",
            "Comparing voter IDs ({:?}) with on-chain member IDs - primary ({:?})",
            voter_ids,
            peer_ids
        );

        if !voter_ids.is_subset(&peer_ids) {
            return Err(PbftError::InvalidMessage(format!(
                "Consensus seal contains vote(s) from invalid ID(s): {:?}",
                voter_ids.difference(&peer_ids).collect::<Vec<_>>()
            )));
        }

        // Check that the seal contains 2f votes (primary vote is implicit, so total of 2f + 1)
        if (voter_ids.len() as u64) < 2 * state.f {
            return Err(PbftError::InvalidMessage(format!(
                "Consensus seal needs {} votes, but only {} found",
                2 * state.f,
                voter_ids.len()
            )));
        }

        Ok(())
    }

    // ---------- Methods called in the main engine loop to periodically check and update state ----------

    /// At a regular interval, try to finalize a block when the primary is ready
    pub fn try_publish(&mut self, state: &mut PbftState) -> Result<(), PbftError> {
        // Only the primary takes care of this, and we try publishing a block
        // on every engine loop, even if it's not yet ready. This isn't an error,
        // so just return Ok(()).
        if !state.is_primary() || state.phase != PbftPhase::PrePreparing {
            return Ok(());
        }

        let now = chrono::prelude::Local::now().timestamp() as u64;
        let interval = now - state.last_block_timestamp;
        if state.seq_num > 1 && state.block_publishing_min_interval.as_secs() > interval {
            return Ok(());
        }

        info!(target: "consensus::cl","===================================try_publish============================================");
        info!(target: "consensus::cl","{}: Try publish proposal", state);

        match self.service.summarize_block(state.seq_num, state.validators.accounts()) {
            Ok(_) => {}
            Err(err) => {
                info!("Couldn't summarize, so not finalizing: {}", err);
                return Ok(());
            }
        }

        // We don't publish a consensus seal at block 1, since we never receive any
        // votes on the genesis block. Leave payload blank for the first block.
        let data = if state.seq_num <= 1 {
            vec![]
        } else {
            let mut msg_out = vec![];
            self.build_seal(state)?.encode(&mut msg_out);
            msg_out
        };
        let seal_bytes = reth_primitives::Bytes::copy_from_slice(&data);

        match self.service.finalize_block() {
            Ok((payload_id, execution_payload)) => {
                let block_id = execution_payload.execution_payload.payload_inner.block_hash;
                let payload = execution_payload_from_payload(&execution_payload);
                info!(target: "consensus::cl","{}: Publishing block {}", state, hex::encode(block_id));
                self.broadcast_block_new(
                    state.view,
                    state.seq_num,
                    payload,
                    payload_id,
                    seal_bytes,
                    state,
                )?;

                Ok(())
            }
            Err(err) => {
                Err(PbftError::ServiceError("Couldn't finalize block".into(), err.to_string()))
            }
        }
    }

    /// Check to see if the idle timeout has expired
    pub fn check_idle_timeout_expired(&mut self, state: &mut PbftState) -> bool {
        state.idle_timeout.check_expired()
    }

    /// Start the idle timeout
    pub fn start_idle_timeout(&self, state: &mut PbftState) {
        state.idle_timeout.start();
    }

    /// Check to see if the commit timeout has expired
    pub fn check_commit_timeout_expired(&mut self, state: &mut PbftState) -> bool {
        state.commit_timeout.check_expired()
    }

    /// Start the commit timeout
    pub fn start_commit_timeout(&self, state: &mut PbftState) {
        state.commit_timeout.start();
    }

    /// Check to see if the view change timeout has expired
    pub fn check_view_change_timeout_expired(&mut self, state: &mut PbftState) -> bool {
        state.view_change_timeout.check_expired()
    }

    // ---------- Methods for communication between nodes ----------

    /// Construct a PbftMessage message and broadcast it to all peers (including self)
    fn broadcast_pbft_message(
        &mut self,
        view: u64,
        seq_num: u64,
        msg_type: PbftMessageType,
        block_id: B256,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        let info =
            PbftMessageInfo { ptype: msg_type as u8, view, seq_num, signer_id: state.id.clone() };

        let msg = PbftMessage { info, block_id };

        info!(target: "consensus::cl","{}: Broadcast PBFT message: {}", state, msg);

        let mut to_all = false;
        if msg_type == PbftMessageType::AnnounceBlock {
            to_all = true;
        }

        let mut to_self = true;
        if msg_type == PbftMessageType::SealRequest {
            to_self = false;
        }

        self.broadcast_message(ParsedMessage::from_pbft_message(msg)?, state, to_all, to_self)
    }

    /// Broadcast the specified message to all of the node's peers, including itself
    fn broadcast_message(
        &mut self,
        msg: ParsedMessage,
        state: &mut PbftState,
        to_all: bool,
        to_self: bool,
    ) -> Result<(), PbftError> {
        // Broadcast to peers
        let message_bytes = msg.get_message_bytes();

        //create header
        let header = ClayerConsensusMessageHeader {
            message_type: msg.info().ptype,
            content_hash: keccak256(&message_bytes),
            signer_id: state.id.clone(),
        };
        let mut header_out = vec![];
        header.encode(&mut header_out);
        let header_bytes = reth_primitives::Bytes::copy_from_slice(header_out.as_slice());

        //sign header
        let signature_hash = keccak256(&header_bytes);
        let signature =
            sign_message(B256::from_slice(&state.kp.secret_bytes()[..]), signature_hash).map_err(
                |err| PbftError::SigningError(format!("signing header error: {}", err.to_string())),
            )?;

        let clayer_msg = ClayerConsensusMessage {
            header_bytes,
            header_signature: ClayerSignature(signature),
            message_bytes,
        };
        let mut msg_out = vec![];
        clayer_msg.encode(&mut msg_out);
        let msg_bytes = reth_primitives::Bytes::copy_from_slice(msg_out.as_slice());

        if to_all {
            self.agent.broadcast_consensus(vec![], msg_bytes);
            return Ok(());
        } else {
            self.agent.broadcast_consensus(state.validators.member_ids().clone(), msg_bytes);

            // Send to self
            if to_self {
                self.on_peer_message(state.id.clone(), msg, state)
            } else {
                Ok(())
            }
        }
    }

    fn broadcast_block_new(
        &mut self,
        view: u64,
        seq_num: u64,
        payload: ClayerExecutionPayload,
        payload_id: PayloadId,
        seal_bytes: reth_primitives::Bytes,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        let info = PbftMessageInfo {
            ptype: PbftMessageType::BlockNew as u8,
            view,
            seq_num,
            signer_id: state.id.clone(),
        };

        let msg: ClayerBlock =
            ClayerBlock { info, block: payload, seal_bytes, payload_id: payload_id.id() };

        trace!(target: "consensus::cl","{}: Created BlockNew message: {:?}", state, msg);

        self.broadcast_message(ParsedMessage::from_block_new_message(msg), state, false, true)
    }

    /// Build a consensus seal for the last block this node committed and send it to the node that
    /// requested the seal (the `recipient`)
    #[allow(clippy::ptr_arg)]
    fn send_seal_response(
        &mut self,
        state: &PbftState,
        recipient: &PeerId,
    ) -> Result<(), PbftError> {
        let seal = self.build_seal(state).map_err(|err| {
            PbftError::InternalError(format!("Failed to build requested seal due to: {}", err))
        })?;

        let mut msg_out = vec![];
        seal.encode(&mut msg_out);
        let message_bytes = reth_primitives::Bytes::copy_from_slice(msg_out.as_slice());

        //create header
        let header = ClayerConsensusMessageHeader {
            message_type: seal.info.ptype,
            content_hash: keccak256(&message_bytes),
            signer_id: state.id.clone(),
        };
        let mut header_out = vec![];
        header.encode(&mut header_out);
        let header_bytes = reth_primitives::Bytes::copy_from_slice(header_out.as_slice());

        //sign header
        let signature_hash = keccak256(&header_bytes);
        let signature =
            sign_message(B256::from_slice(&state.kp.secret_bytes()[..]), signature_hash).map_err(
                |err| PbftError::SigningError(format!("signing header error: {}", err.to_string())),
            )?;

        let clayer_msg = ClayerConsensusMessage {
            header_bytes,
            header_signature: ClayerSignature(signature),
            message_bytes,
        };
        let mut msg_out = vec![];
        clayer_msg.encode(&mut msg_out);
        let msg_bytes = reth_primitives::Bytes::copy_from_slice(msg_out.as_slice());

        // Send the seal to the requester
        self.agent.broadcast_consensus(vec![recipient.clone()], msg_bytes);

        Ok(())
    }

    /// Build a consensus seal for the last block this node committed and send it to the node that
    /// requested the seal (the `recipient`)
    #[allow(clippy::ptr_arg)]
    fn send_seal_response_v2(
        &mut self,
        state: &PbftState,
        recipient: &PeerId,
        seal: &PbftSeal,
    ) -> Result<(), PbftError> {
        let mut msg_out = vec![];
        seal.encode(&mut msg_out);
        let message_bytes = reth_primitives::Bytes::copy_from_slice(msg_out.as_slice());
        info!(target: "consensus::cl", "Sending block seal to {:?}: {}", recipient, seal);

        //create header
        let header = ClayerConsensusMessageHeader {
            message_type: seal.info.ptype,
            content_hash: keccak256(&message_bytes),
            signer_id: state.id.clone(),
        };
        let mut header_out = vec![];
        header.encode(&mut header_out);
        let header_bytes = reth_primitives::Bytes::copy_from_slice(header_out.as_slice());

        //sign header
        let signature_hash = keccak256(&header_bytes);
        let signature =
            sign_message(B256::from_slice(&state.kp.secret_bytes()[..]), signature_hash).map_err(
                |err| PbftError::SigningError(format!("signing header error: {}", err.to_string())),
            )?;

        let clayer_msg = ClayerConsensusMessage {
            header_bytes,
            header_signature: ClayerSignature(signature),
            message_bytes,
        };
        let mut msg_out = vec![];
        clayer_msg.encode(&mut msg_out);
        let msg_bytes = reth_primitives::Bytes::copy_from_slice(msg_out.as_slice());

        // Send the seal to the requester
        self.agent.broadcast_consensus(vec![recipient.clone()], msg_bytes);

        Ok(())
    }

    fn save_seal(&mut self, seal: &PbftSeal) -> Result<(), PbftError> {
        let mut msg_out = vec![];
        seal.encode(&mut msg_out);
        self.db
            .save_consensus_content(seal.block_id, ConsensusBytes { content: msg_out })
            .map_err(|err| {
                PbftError::InternalError(format!("Failed to save seal due to: {}", err))
            })?;
        Ok(())
    }

    pub fn load_seal(&self, block_id: B256) -> Result<Option<PbftSeal>, PbftError> {
        let result = self.db.consensus_content(block_id).map_err(|err| {
            PbftError::InternalError(format!("Failed to load seal due to: {}", err))
        })?;
        match result {
            Some(content) => {
                let seal = PbftSeal::decode(&mut content.content.as_slice()).map_err(|err| {
                    PbftError::SerializationError(
                        "Error parsing seal for verification".into(),
                        err.to_string(),
                    )
                })?;
                Ok(Some(seal))
            }
            None => Ok(None),
        }
    }

    // ---------- Miscellaneous methods ----------

    /// Start a view change when this node suspects that the primary is faulty
    ///
    /// Update state to reflect that the node is now in the process of this view change, start the
    /// view change timeout, and broadcast a view change message
    ///
    /// # Panics
    /// + If the view change timeout overflows
    pub fn start_view_change(&mut self, state: &mut PbftState, view: u64) -> Result<(), PbftError> {
        // Do not send messages again if we are already in the midst of this or a later view change
        if match state.mode {
            PbftMode::ViewChanging(v) => view <= v,
            _ => false,
        } {
            return Ok(());
        }

        info!(target: "consensus::cl","{}: Starting change to view {}", state, view);

        state.mode = PbftMode::ViewChanging(view);

        // Stop the idle and commit timeouts because they are not needed until after the view
        // change
        state.idle_timeout.stop();
        state.commit_timeout.stop();

        // Stop the view change timeout if it is already active (will be restarted when 2f + 1
        // ViewChange messages for the new view are received)
        state.view_change_timeout.stop();

        // Broadcast the view change message
        self.broadcast_pbft_message(
            view,
            state.seq_num - 1,
            PbftMessageType::ViewChange,
            B256::ZERO,
            state,
        )
    }

    fn handle_block_new(
        &mut self,
        msg: ParsedMessage,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        let block = msg.get_block_new().clone();
        self.on_block_new(block, state)?;
        Ok(())
    }

    fn send_seal_request(
        &mut self,
        state: &mut PbftState,
        block_id: B256,
    ) -> Result<(), PbftError> {
        self.broadcast_pbft_message(
            state.view,
            state.seq_num,
            PbftMessageType::SealRequest,
            block_id,
            state,
        )
    }
}

fn execution_payload_from_payload(payload: &ExecutionPayloadWrapperV2) -> ClayerExecutionPayload {
    let p = &payload.execution_payload.payload_inner;
    let withdrawals = payload
        .execution_payload
        .withdrawals
        .iter()
        .map(|w| reth_primitives::Withdrawal {
            index: w.index,
            validator_index: w.validator_index,
            address: w.address,
            amount: w.amount,
        })
        .collect::<Vec<_>>();
    ClayerExecutionPayload {
        parent_hash: p.parent_hash,
        fee_recipient: p.fee_recipient,
        state_root: p.state_root,
        receipts_root: p.receipts_root,
        logs_bloom: p.logs_bloom,
        prev_randao: p.prev_randao,
        block_number: p.block_number,
        gas_limit: p.gas_limit,
        gas_used: p.gas_used,
        timestamp: p.timestamp,
        extra_data: p.extra_data.clone(),
        base_fee_per_gas: p.base_fee_per_gas,
        block_hash: p.block_hash,
        transactions: p.transactions.clone(),
        withdrawals,
        block_value: payload.block_value,
    }
}

fn execution_payload_to_payload(payload: &ClayerExecutionPayload) -> ExecutionPayloadWrapperV2 {
    let withdrawals = payload
        .withdrawals
        .iter()
        .map(|w| reth_rpc_types::Withdrawal {
            index: w.index,
            validator_index: w.validator_index,
            address: w.address,
            amount: w.amount,
        })
        .collect::<Vec<_>>();
    ExecutionPayloadWrapperV2 {
        execution_payload: ExecutionPayloadV2 {
            payload_inner: ExecutionPayloadV1 {
                parent_hash: payload.parent_hash,
                fee_recipient: payload.fee_recipient,
                state_root: payload.state_root,
                receipts_root: payload.receipts_root,
                logs_bloom: payload.logs_bloom.clone(),
                prev_randao: payload.prev_randao,
                block_number: payload.block_number,
                gas_limit: payload.gas_limit,
                gas_used: payload.gas_used,
                timestamp: payload.timestamp,
                extra_data: payload.extra_data.clone(),
                base_fee_per_gas: payload.base_fee_per_gas,
                block_hash: payload.block_hash,
                transactions: payload.transactions.clone(),
            },
            withdrawals,
        },
        block_value: payload.block_value,
    }
}

/// for initialize, broadcast_bootstrap_commit
pub fn clayer_block_from_header(header: &SealedHeader) -> ClayerBlock {
    let block = ClayerExecutionPayload {
        parent_hash: header.parent_hash,
        fee_recipient: reth_primitives::Address::ZERO,
        state_root: header.state_root,
        receipts_root: header.receipts_root,
        logs_bloom: header.logs_bloom,
        prev_randao: B256::ZERO,
        block_number: header.number,
        gas_limit: header.gas_limit,
        gas_used: header.gas_used,
        timestamp: header.timestamp,
        extra_data: header.extra_data.clone(),
        base_fee_per_gas: if let Some(base_fee_per_gas) = header.base_fee_per_gas {
            reth_primitives::U256::from(base_fee_per_gas)
        } else {
            reth_primitives::U256::from(0)
        },
        block_hash: header.hash,
        transactions: Vec::new(),
        withdrawals: Vec::new(),
        block_value: reth_primitives::U256::from(0),
    };
    let info = PbftMessageInfo {
        ptype: PbftMessageType::BlockNew as u8,
        view: 0,
        seq_num: 0,
        signer_id: PeerId::default(),
    };

    ClayerBlock {
        info,
        block,
        seal_bytes: reth_primitives::Bytes::default(),
        payload_id: B64::ZERO,
    }
}

/// for initialize, broadcast_bootstrap_commit
pub fn clayer_block_from_seal(header: &SealedHeader, seal: PbftSeal) -> ClayerBlock {
    let block = ClayerExecutionPayload {
        parent_hash: header.parent_hash,
        fee_recipient: reth_primitives::Address::ZERO,
        state_root: header.state_root,
        receipts_root: header.receipts_root,
        logs_bloom: header.logs_bloom,
        prev_randao: B256::ZERO,
        block_number: header.number,
        gas_limit: header.gas_limit,
        gas_used: header.gas_used,
        timestamp: header.timestamp,
        extra_data: header.extra_data.clone(),
        base_fee_per_gas: if let Some(base_fee_per_gas) = header.base_fee_per_gas {
            reth_primitives::U256::from(base_fee_per_gas)
        } else {
            reth_primitives::U256::from(0)
        },
        block_hash: header.hash,
        transactions: Vec::new(),
        withdrawals: Vec::new(),
        block_value: reth_primitives::U256::from(0),
    };
    let info = PbftMessageInfo {
        ptype: PbftMessageType::BlockNew as u8,
        view: seal.info.view,
        seq_num: seal.info.seq_num,
        signer_id: seal.info.signer_id,
    };

    let mut msg_out = vec![];
    seal.encode(&mut msg_out);
    let message_bytes = reth_primitives::Bytes::copy_from_slice(msg_out.as_slice());

    ClayerBlock { info, block, seal_bytes: message_bytes, payload_id: B64::ZERO }
}

pub fn assemble_peer_id(datas: Vec<Vec<u8>>) -> Result<Vec<PeerId>, PbftError> {
    if datas.is_empty() {
        error!(target: "consensus::cl","The number of elect contract returns must be not 0");
        return Err(PbftError::FaultyPrimary(format!(
            "The number of elect contract returns must be not 0"
        )));
    }
    if datas.len() % 2 != 0 {
        error!(target: "consensus::cl","The number of elect contract returns must be even");
        return Err(PbftError::FaultyPrimary(format!(
            "The number of elect contract returns must be even"
        )));
        // panic!("The number of elect contract returns must be even");
    }
    let mut peers = Vec::new();
    let mut bytes = Vec::new();
    for (i, item) in datas.iter().enumerate() {
        if i % 2 == 0 {
            bytes.extend_from_slice(item);
        } else {
            bytes.extend_from_slice(item);
            let id = PeerId::from_slice(&bytes);
            info!(target: "consensus::cl","peer id: {}",  id);
            peers.push(id);
            bytes.clear();
        }
    }
    Ok(peers)
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;

    #[test]
    fn test_bytes_default() {
        let b = reth_primitives::Bytes::default();

        println!("{}  {}", reth_primitives::Bytes::default(), b.is_empty());
    }

    #[test]
    fn test_peer_id_default() {
        let p = reth_primitives::PeerId::default();
        let p1 = reth_rpc_types::PeerId::default();

        println!("{} {}", reth_primitives::PeerId::default(), p.is_zero());

        println!("{} {}", reth_rpc_types::PeerId::default(), p1.is_zero());
    }

    #[test]
    fn test_address_default() {
        let a = reth_primitives::Address::default();
        println!("{} {}", reth_primitives::Address::default(), a.is_zero());
    }

    #[test]
    fn test_b256_default() {
        let b = B256::default();
        println!("{} {}", B256::default(), b.is_zero());
    }
}
