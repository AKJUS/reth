//! State root task related functionality.

use derive_more::derive::Deref;
use metrics::Histogram;
use rayon::iter::{ParallelBridge, ParallelIterator};
use reth_errors::{ProviderError, ProviderResult};
use reth_evm::system_calls::{OnStateHook, StateChangeSource};
use reth_metrics::Metrics;
use reth_provider::{
    providers::ConsistentDbView, BlockReader, DBProvider, DatabaseProviderFactory,
    StateCommitmentProvider,
};
use reth_revm::state::EvmState;
use reth_trie::{
    hashed_cursor::HashedPostStateCursorFactory,
    prefix_set::TriePrefixSetsMut,
    proof::ProofBlindedProviderFactory,
    trie_cursor::InMemoryTrieCursorFactory,
    updates::{TrieUpdates, TrieUpdatesSorted},
    HashedPostState, HashedPostStateSorted, HashedStorage, MultiProof, MultiProofTargets, Nibbles,
    TrieInput,
};
use reth_trie_db::{DatabaseHashedCursorFactory, DatabaseTrieCursorFactory};
use reth_trie_parallel::{proof::ParallelProof, root::ParallelStateRootError};
use reth_trie_sparse::{
    blinded::{BlindedProvider, BlindedProviderFactory},
    errors::{SparseStateTrieResult, SparseTrieErrorKind},
    SparseStateTrie,
};
use revm_primitives::{keccak256, B256};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        mpsc::{self, channel, Receiver, Sender},
        Arc,
    },
    time::{Duration, Instant},
};
use tracing::{debug, error, trace, trace_span};

/// The level below which the sparse trie hashes are calculated in [`update_sparse_trie`].
const SPARSE_TRIE_INCREMENTAL_LEVEL: usize = 2;

/// Determines the size of the rayon thread pool to be used in [`StateRootTask`].
///
/// The value is determined as `max(NUM_THREADS - 2, 3)`:
/// - It should leave at least 2 threads to the rest of the system to be used in:
///     - Engine
///     - State Root Task spawned in [`StateRootTask::spawn`]
/// - It should heave at least 3 threads to be used in:
///     - Sparse Trie spawned in [`run_sparse_trie`]
///     - Multiproof computation spawned in [`MultiProofManager::spawn_multiproof`]
///     - Storage root computation spawned in [`ParallelProof::multiproof`]
///
/// NOTE: this value can be greater than the available cores in the host, it
/// represents the maximum number of threads that can be handled by the pool.
pub(crate) fn rayon_thread_pool_size() -> usize {
    std::thread::available_parallelism().map_or(3, |num| (num.get().saturating_sub(2).max(3)))
}

/// Determines if the host has enough parallelism to run the state root task.
///
/// It requires at least 5 parallel threads:
/// - Engine in main thread that spawns the state root task.
/// - State Root Task spawned in [`StateRootTask::spawn`]
/// - Sparse Trie spawned in [`run_sparse_trie`]
/// - Multiproof computation spawned in [`MultiProofManager::spawn_multiproof`]
/// - Storage root computation spawned in [`ParallelProof::multiproof`]
pub(crate) fn has_enough_parallelism() -> bool {
    std::thread::available_parallelism().is_ok_and(|num| num.get() >= 5)
}

/// Outcome of the state root computation, including the state root itself with
/// the trie updates and the total time spent.
#[derive(Debug)]
pub struct StateRootComputeOutcome {
    /// The computed state root and trie updates
    pub state_root: (B256, TrieUpdates),
    /// The total time spent calculating the state root
    pub total_time: Duration,
    /// The time spent calculating the state root since the last state update
    pub time_from_last_update: Duration,
}

/// A trie update that can be applied to sparse trie alongside the proofs for touched parts of the
/// state.
#[derive(Default, Debug)]
pub struct SparseTrieUpdate {
    /// The state update that was used to calculate the proof
    state: HashedPostState,
    /// The calculated multiproof
    multiproof: MultiProof,
}

impl SparseTrieUpdate {
    /// Returns true if the update is empty.
    pub fn is_empty(&self) -> bool {
        self.state.is_empty() && self.multiproof.is_empty()
    }

    /// Construct update from multiproof.
    pub fn from_multiproof(multiproof: MultiProof) -> Self {
        Self { multiproof, ..Default::default() }
    }

    /// Extend update with contents of the other.
    pub fn extend(&mut self, other: Self) {
        self.state.extend(other.state);
        self.multiproof.extend(other.multiproof);
    }
}

/// Result of the state root calculation
pub(crate) type StateRootResult = Result<StateRootComputeOutcome, ParallelStateRootError>;

/// Handle to a spawned state root task.
#[derive(Debug)]
pub struct StateRootHandle {
    /// Channel for receiving the final result.
    rx: mpsc::Receiver<StateRootResult>,
}

impl StateRootHandle {
    /// Creates a new handle from a receiver.
    pub(crate) const fn new(rx: mpsc::Receiver<StateRootResult>) -> Self {
        Self { rx }
    }

    /// Waits for the state root calculation to complete.
    pub fn wait_for_result(self) -> StateRootResult {
        self.rx.recv().expect("state root task was dropped without sending result")
    }
}

/// Common configuration for state root tasks
#[derive(Debug, Clone)]
pub struct StateRootConfig<Factory> {
    /// View over the state in the database.
    pub consistent_view: ConsistentDbView<Factory>,
    /// The sorted collection of cached in-memory intermediate trie nodes that
    /// can be reused for computation.
    pub nodes_sorted: Arc<TrieUpdatesSorted>,
    /// The sorted in-memory overlay hashed state.
    pub state_sorted: Arc<HashedPostStateSorted>,
    /// The collection of prefix sets for the computation. Since the prefix sets _always_
    /// invalidate the in-memory nodes, not all keys from `state_sorted` might be present here,
    /// if we have cached nodes for them.
    pub prefix_sets: Arc<TriePrefixSetsMut>,
}

impl<Factory> StateRootConfig<Factory> {
    /// Creates a new state root config from the consistent view and the trie input.
    pub fn new_from_input(consistent_view: ConsistentDbView<Factory>, input: TrieInput) -> Self {
        Self {
            consistent_view,
            nodes_sorted: Arc::new(input.nodes.into_sorted()),
            state_sorted: Arc::new(input.state.into_sorted()),
            prefix_sets: Arc::new(input.prefix_sets),
        }
    }
}

/// Messages used internally by the state root task
#[derive(Debug)]
pub enum StateRootMessage {
    /// Prefetch proof targets
    PrefetchProofs(MultiProofTargets),
    /// New state update from transaction execution with its source
    StateUpdate(StateChangeSource, EvmState),
    /// Empty proof for a specific state update
    EmptyProof {
        /// The index of this proof in the sequence of state updates
        sequence_number: u64,
        /// The state update that was used to calculate the proof
        state: HashedPostState,
    },
    /// Proof calculation completed for a specific state update
    ProofCalculated(Box<ProofCalculated>),
    /// Error during proof calculation
    ProofCalculationError(ProviderError),
    /// State root calculation completed
    RootCalculated {
        /// Final state root.
        state_root: B256,
        /// Trie updates.
        trie_updates: TrieUpdates,
        /// The number of time sparse trie was updated.
        iterations: u64,
    },
    /// Error during state root calculation
    RootCalculationError(ParallelStateRootError),
    /// Signals state update stream end.
    FinishedStateUpdates,
}

/// Message about completion of proof calculation for a specific state update
#[derive(Debug)]
pub struct ProofCalculated {
    /// The index of this proof in the sequence of state updates
    sequence_number: u64,
    /// Sparse trie update
    update: SparseTrieUpdate,
    /// Total number of account targets
    account_targets: usize,
    /// Total number of storage slot targets
    storage_targets: usize,
    /// The time taken to calculate the proof.
    elapsed: Duration,
}

/// Whether or not a proof was fetched due to a state update, or due to a prefetch command.
#[derive(Debug)]
pub enum ProofFetchSource {
    /// The proof was fetched due to a prefetch command.
    Prefetch,
    /// The proof was fetched due to a state update.
    StateUpdate,
}

/// Handle to track proof calculation ordering
#[derive(Debug, Default)]
pub(crate) struct ProofSequencer {
    /// The next proof sequence number to be produced.
    next_sequence: u64,
    /// The next sequence number expected to be delivered.
    next_to_deliver: u64,
    /// Buffer for out-of-order proofs and corresponding state updates
    pending_proofs: BTreeMap<u64, SparseTrieUpdate>,
}

impl ProofSequencer {
    /// Creates a new proof sequencer
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Gets the next sequence number and increments the counter
    pub(crate) fn next_sequence(&mut self) -> u64 {
        let seq = self.next_sequence;
        self.next_sequence += 1;
        seq
    }

    /// Adds a proof with the corresponding state update and returns all sequential proofs and state
    /// updates if we have a continuous sequence
    pub(crate) fn add_proof(
        &mut self,
        sequence: u64,
        update: SparseTrieUpdate,
    ) -> Vec<SparseTrieUpdate> {
        if sequence >= self.next_to_deliver {
            self.pending_proofs.insert(sequence, update);
        }

        // return early if we don't have the next expected proof
        if !self.pending_proofs.contains_key(&self.next_to_deliver) {
            return Vec::new()
        }

        let mut consecutive_proofs = Vec::with_capacity(self.pending_proofs.len());
        let mut current_sequence = self.next_to_deliver;

        // keep collecting proofs and state updates as long as we have consecutive sequence numbers
        while let Some(pending) = self.pending_proofs.remove(&current_sequence) {
            consecutive_proofs.push(pending);
            current_sequence += 1;

            // if we don't have the next number, stop collecting
            if !self.pending_proofs.contains_key(&current_sequence) {
                break;
            }
        }

        self.next_to_deliver += consecutive_proofs.len() as u64;

        consecutive_proofs
    }

    /// Returns true if we still have pending proofs
    pub(crate) fn has_pending(&self) -> bool {
        !self.pending_proofs.is_empty()
    }
}

/// A wrapper for the sender that signals completion when dropped
#[derive(Deref, Debug)]
pub struct StateHookSender(Sender<StateRootMessage>);

impl StateHookSender {
    pub(crate) const fn new(inner: Sender<StateRootMessage>) -> Self {
        Self(inner)
    }
}

impl Drop for StateHookSender {
    fn drop(&mut self) {
        // Send completion signal when the sender is dropped
        let _ = self.0.send(StateRootMessage::FinishedStateUpdates);
    }
}

fn evm_state_to_hashed_post_state(update: EvmState) -> HashedPostState {
    let mut hashed_state = HashedPostState::with_capacity(update.len());

    for (address, account) in update {
        if account.is_touched() {
            let hashed_address = keccak256(address);
            trace!(target: "engine::root", ?address, ?hashed_address, "Adding account to state update");

            let destroyed = account.is_selfdestructed();
            let info = if destroyed { None } else { Some(account.info.into()) };
            hashed_state.accounts.insert(hashed_address, info);

            let mut changed_storage_iter = account
                .storage
                .into_iter()
                .filter(|(_slot, value)| value.is_changed())
                .map(|(slot, value)| (keccak256(B256::from(slot)), value.present_value))
                .peekable();

            if destroyed {
                hashed_state.storages.insert(hashed_address, HashedStorage::new(true));
            } else if changed_storage_iter.peek().is_some() {
                hashed_state
                    .storages
                    .insert(hashed_address, HashedStorage::from_iter(false, changed_storage_iter));
            }
        }
    }

    hashed_state
}

/// Input parameters for spawning a multiproof calculation.
#[derive(Debug)]
struct MultiProofInput<Factory> {
    config: StateRootConfig<Factory>,
    source: Option<StateChangeSource>,
    hashed_state_update: HashedPostState,
    proof_targets: MultiProofTargets,
    proof_sequence_number: u64,
    state_root_message_sender: Sender<StateRootMessage>,
}

#[derive(Metrics, Clone)]
#[metrics(scope = "tree.root")]
struct MultiProofMetrics {
    /// Histogram of the number of inflight multiproofs.
    pub inflight_multiproofs_histogram: Histogram,
    /// Histogram of the number of pending multiproofs.
    pub pending_multiproofs_histogram: Histogram,
}

/// Manages concurrent multiproof calculations.
/// Takes care of not having more calculations in flight than a given thread
/// pool size, further calculation requests are queued and spawn later, after
/// availability has been signaled.
#[derive(Debug)]
struct MultiProofManager<Factory> {
    /// Maximum number of concurrent calculations.
    max_concurrent: usize,
    /// Currently running calculations.
    inflight: usize,
    /// Queued calculations.
    pending: VecDeque<MultiProofInput<Factory>>,
    /// Thread pool to spawn multiproof calculations.
    thread_pool: Arc<rayon::ThreadPool>,
    metrics: MultiProofMetrics,
}

impl<Factory> MultiProofManager<Factory>
where
    Factory:
        DatabaseProviderFactory<Provider: BlockReader> + StateCommitmentProvider + Clone + 'static,
{
    /// Creates a new [`MultiProofManager`].
    fn new(
        thread_pool: Arc<rayon::ThreadPool>,
        thread_pool_size: usize,
        metrics: MultiProofMetrics,
    ) -> Self {
        // we keep 2 threads to be used internally by [`StateRootTask`]
        let max_concurrent = thread_pool_size.saturating_sub(2);
        debug_assert!(max_concurrent != 0);
        Self {
            thread_pool,
            max_concurrent,
            inflight: 0,
            pending: VecDeque::with_capacity(max_concurrent),
            metrics,
        }
    }

    /// Spawns a new multiproof calculation or enqueues it for later if
    /// `max_concurrent` are already inflight.
    fn spawn_or_queue(&mut self, input: MultiProofInput<Factory>) {
        // If there are no proof targets, we can just send an empty multiproof back immediately
        if input.proof_targets.is_empty() {
            debug!(
                sequence_number = input.proof_sequence_number,
                "No proof targets, sending empty multiproof back immediately"
            );
            let _ = input.state_root_message_sender.send(StateRootMessage::EmptyProof {
                sequence_number: input.proof_sequence_number,
                state: input.hashed_state_update,
            });
            return
        }

        if self.inflight >= self.max_concurrent {
            self.pending.push_back(input);
            self.metrics.pending_multiproofs_histogram.record(self.pending.len() as f64);
            return;
        }

        self.spawn_multiproof(input);
    }

    /// Signals that a multiproof calculation has finished and there's room to
    /// spawn a new calculation if needed.
    fn on_calculation_complete(&mut self) {
        self.inflight = self.inflight.saturating_sub(1);
        self.metrics.inflight_multiproofs_histogram.record(self.inflight as f64);

        if let Some(input) = self.pending.pop_front() {
            self.metrics.pending_multiproofs_histogram.record(self.pending.len() as f64);
            self.spawn_multiproof(input);
        }
    }

    /// Spawns a multiproof calculation.
    fn spawn_multiproof(
        &mut self,
        MultiProofInput {
            config,
            source,
            hashed_state_update,
            proof_targets,
            proof_sequence_number,
            state_root_message_sender,
        }: MultiProofInput<Factory>,
    ) {
        let thread_pool = self.thread_pool.clone();

        self.thread_pool.spawn(move || {
            let account_targets = proof_targets.len();
            let storage_targets = proof_targets.values().map(|slots| slots.len()).sum();

            trace!(
                target: "engine::root",
                proof_sequence_number,
                ?proof_targets,
                account_targets,
                storage_targets,
                "Starting multiproof calculation",
            );
            let start = Instant::now();
            let result = calculate_multiproof(thread_pool, config, proof_targets);
            let elapsed = start.elapsed();
            trace!(
                target: "engine::root",
                proof_sequence_number,
                ?elapsed,
                ?source,
                account_targets,
                storage_targets,
                "MultiProof calculated",
            );

            match result {
                Ok(proof) => {
                    let _ = state_root_message_sender.send(StateRootMessage::ProofCalculated(
                        Box::new(ProofCalculated {
                            sequence_number: proof_sequence_number,
                            update: SparseTrieUpdate {
                                state: hashed_state_update,
                                multiproof: proof,
                            },
                            account_targets,
                            storage_targets,
                            elapsed,
                        }),
                    ));
                }
                Err(error) => {
                    let _ = state_root_message_sender
                        .send(StateRootMessage::ProofCalculationError(error));
                }
            }
        });

        self.inflight += 1;
        self.metrics.inflight_multiproofs_histogram.record(self.inflight as f64);
    }
}

#[derive(Metrics, Clone)]
#[metrics(scope = "tree.root")]
pub(crate) struct StateRootTaskMetrics {
    /// Histogram of proof calculation durations.
    pub proof_calculation_duration_histogram: Histogram,
    /// Histogram of proof calculation account targets.
    pub proof_calculation_account_targets_histogram: Histogram,
    /// Histogram of proof calculation storage targets.
    pub proof_calculation_storage_targets_histogram: Histogram,

    /// Histogram of sparse trie update durations.
    pub sparse_trie_update_duration_histogram: Histogram,
    /// Histogram of sparse trie final update durations.
    pub sparse_trie_final_update_duration_histogram: Histogram,

    /// Histogram of state updates received.
    pub state_updates_received_histogram: Histogram,
    /// Histogram of proofs processed.
    pub proofs_processed_histogram: Histogram,
    /// Histogram of state root update iterations.
    pub state_root_iterations_histogram: Histogram,

    /// Histogram of the number of updated state nodes.
    pub nodes_sorted_account_nodes_histogram: Histogram,
    /// Histogram of the number of emoved state nodes.
    pub nodes_sorted_removed_nodes_histogram: Histogram,
    /// Histogram of the number of storage tries.
    pub nodes_sorted_storage_tries_histogram: Histogram,

    /// Histogram of the number of updated state of accounts.
    pub state_sorted_accounts_histogram: Histogram,
    /// Histogram of the number of hashed storages.
    pub state_sorted_storages_histogram: Histogram,

    /// Histogram of the number of account prefixes that have changed.
    pub prefix_sets_accounts_histogram: Histogram,
    /// Histogram of the number of storage prefixes that have changed.
    pub prefix_sets_storages_histogram: Histogram,
    /// Histogram of the number of destroyed accounts.
    pub prefix_sets_destroyed_accounts_histogram: Histogram,
}

/// Standalone task that receives a transaction state stream and updates relevant
/// data structures to calculate state root.
///
/// It is responsible of  initializing a blinded sparse trie and subscribe to
/// transaction state stream. As it receives transaction execution results, it
/// fetches the proofs for relevant accounts from the database and reveal them
/// to the tree.
/// Then it updates relevant leaves according to the result of the transaction.
#[derive(Debug)]
pub struct StateRootTask<Factory> {
    /// Task configuration.
    config: StateRootConfig<Factory>,
    /// Receiver for state root related messages.
    rx: Receiver<StateRootMessage>,
    /// Sender for state root related messages.
    tx: Sender<StateRootMessage>,
    /// Proof targets that have been already fetched.
    fetched_proof_targets: MultiProofTargets,
    /// Proof sequencing handler.
    proof_sequencer: ProofSequencer,
    /// Reference to the shared thread pool for parallel proof generation.
    thread_pool: Arc<rayon::ThreadPool>,
    /// Manages calculation of multiproofs.
    multiproof_manager: MultiProofManager<Factory>,
    /// State root task metrics
    metrics: StateRootTaskMetrics,
}

impl<Factory> StateRootTask<Factory>
where
    Factory:
        DatabaseProviderFactory<Provider: BlockReader> + StateCommitmentProvider + Clone + 'static,
{
    /// Creates a new state root task with the unified message channel
    pub fn new(config: StateRootConfig<Factory>, thread_pool: Arc<rayon::ThreadPool>) -> Self {
        let (tx, rx) = channel();
        Self {
            config,
            rx,
            tx,
            fetched_proof_targets: Default::default(),
            proof_sequencer: ProofSequencer::new(),
            thread_pool: thread_pool.clone(),
            multiproof_manager: MultiProofManager::new(
                thread_pool,
                rayon_thread_pool_size(),
                MultiProofMetrics::default(),
            ),
            metrics: StateRootTaskMetrics::default(),
        }
    }

    /// Returns a [`Sender`] that can be used to send arbitrary [`StateRootMessage`]s to this task.
    pub fn state_root_message_sender(&self) -> Sender<StateRootMessage> {
        self.tx.clone()
    }

    /// Returns a [`StateHookSender`] that can be used to send state updates to this task.
    pub fn state_hook_sender(&self) -> StateHookSender {
        StateHookSender::new(self.tx.clone())
    }

    /// Returns a state hook to be used to send state updates to this task.
    pub fn state_hook(&self) -> impl OnStateHook {
        let state_hook = self.state_hook_sender();

        move |source: StateChangeSource, state: &EvmState| {
            if let Err(error) =
                state_hook.send(StateRootMessage::StateUpdate(source, state.clone()))
            {
                error!(target: "engine::root", ?error, "Failed to send state update");
            }
        }
    }

    /// Spawns the state root task and returns a handle to await its result.
    pub fn spawn(self) -> StateRootHandle {
        let sparse_trie_tx = Self::spawn_sparse_trie(
            self.thread_pool.clone(),
            self.config.clone(),
            self.metrics.clone(),
            self.tx.clone(),
        );
        let (tx, rx) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("State Root Task".to_string())
            .spawn(move || {
                debug!(target: "engine::tree", "State root task starting");

                self.observe_config();

                let result = self.run(sparse_trie_tx);
                let _ = tx.send(result);
            })
            .expect("failed to spawn state root thread");

        StateRootHandle::new(rx)
    }

    /// Logs and records in metrics the state root config parameters.
    fn observe_config(&self) {
        let nodes_sorted_account_nodes = self.config.nodes_sorted.account_nodes.len();
        let nodes_sorted_removed_nodes = self.config.nodes_sorted.removed_nodes.len();
        let nodes_sorted_storage_tries = self.config.nodes_sorted.storage_tries.len();
        let state_sorted_accounts = self.config.state_sorted.accounts.accounts.len();
        let state_sorted_destroyed_accounts =
            self.config.state_sorted.accounts.destroyed_accounts.len();
        let state_sorted_storages = self.config.state_sorted.storages.len();
        let prefix_sets_accounts = self.config.prefix_sets.account_prefix_set.len();
        let prefix_sets_storages = self
            .config
            .prefix_sets
            .storage_prefix_sets
            .values()
            .map(|set| set.len())
            .sum::<usize>();
        let prefix_sets_destroyed_accounts = self.config.prefix_sets.destroyed_accounts.len();

        debug!(
            target: "engine::tree",
            ?nodes_sorted_account_nodes,
            ?nodes_sorted_removed_nodes,
            ?nodes_sorted_storage_tries,
            ?state_sorted_accounts,
            ?state_sorted_destroyed_accounts,
            ?state_sorted_storages,
            ?prefix_sets_accounts,
            ?prefix_sets_storages,
            ?prefix_sets_destroyed_accounts,
            "State root config"
        );

        self.metrics.nodes_sorted_account_nodes_histogram.record(nodes_sorted_account_nodes as f64);
        self.metrics.nodes_sorted_removed_nodes_histogram.record(nodes_sorted_removed_nodes as f64);
        self.metrics.nodes_sorted_storage_tries_histogram.record(nodes_sorted_storage_tries as f64);
        self.metrics.state_sorted_accounts_histogram.record(state_sorted_accounts as f64);
        self.metrics.state_sorted_storages_histogram.record(state_sorted_storages as f64);
        self.metrics.prefix_sets_accounts_histogram.record(prefix_sets_accounts as f64);
        self.metrics.prefix_sets_storages_histogram.record(prefix_sets_storages as f64);
        self.metrics
            .prefix_sets_destroyed_accounts_histogram
            .record(prefix_sets_destroyed_accounts as f64);
    }

    /// Spawn long running sparse trie task that forwards the final result upon completion.
    fn spawn_sparse_trie(
        thread_pool: Arc<rayon::ThreadPool>,
        config: StateRootConfig<Factory>,
        metrics: StateRootTaskMetrics,
        task_tx: Sender<StateRootMessage>,
    ) -> Sender<SparseTrieUpdate> {
        let (tx, rx) = mpsc::channel();
        thread_pool.spawn(move || {
            debug!(target: "engine::tree", "Sparse trie task starting");
            // We clone the task sender here so that it can be used in case the sparse trie task
            // succeeds, without blocking due to any `Drop` implementation.
            //
            // It's more important to make sure we capture any errors, than to make sure we send an
            // error result without blocking, which is why we wait for `run_sparse_trie` to return
            // before sending errors.
            if let Err(err) = run_sparse_trie(config, metrics, rx, task_tx.clone()) {
                let _ = task_tx.send(StateRootMessage::RootCalculationError(err));
            }
        });
        tx
    }

    /// Handles request for proof prefetch.
    fn on_prefetch_proof(&mut self, mut proof_targets: MultiProofTargets) {
        proof_targets.retain_difference(&self.fetched_proof_targets);
        self.fetched_proof_targets.extend_ref(&proof_targets);

        self.multiproof_manager.spawn_or_queue(MultiProofInput {
            config: self.config.clone(),
            source: None,
            hashed_state_update: Default::default(),
            proof_targets,
            proof_sequence_number: self.proof_sequencer.next_sequence(),
            state_root_message_sender: self.tx.clone(),
        });
    }

    /// Handles state updates.
    ///
    /// Returns proof targets derived from the state update.
    fn on_state_update(
        &mut self,
        source: StateChangeSource,
        update: EvmState,
        proof_sequence_number: u64,
    ) {
        let hashed_state_update = evm_state_to_hashed_post_state(update);
        let proof_targets =
            hashed_state_update.multi_proof_targets_difference(&self.fetched_proof_targets);
        self.fetched_proof_targets.extend_ref(&proof_targets);

        self.multiproof_manager.spawn_or_queue(MultiProofInput {
            config: self.config.clone(),
            source: Some(source),
            hashed_state_update,
            proof_targets,
            proof_sequence_number,
            state_root_message_sender: self.tx.clone(),
        });
    }

    /// Handler for new proof calculated, aggregates all the existing sequential proofs.
    fn on_proof(
        &mut self,
        sequence_number: u64,
        update: SparseTrieUpdate,
    ) -> Option<SparseTrieUpdate> {
        let ready_proofs = self.proof_sequencer.add_proof(sequence_number, update);

        ready_proofs
            .into_iter()
            // Merge all ready proofs and state updates
            .reduce(|mut acc_update, update| {
                acc_update.extend(update);
                acc_update
            })
            // Return None if the resulting proof is empty
            .filter(|proof| !proof.is_empty())
    }

    /// Starts the main loop that handles all incoming messages, fetches proofs, applies them to the
    /// sparse trie, updates the sparse trie, and eventually returns the state root.
    ///
    /// The lifecycle is the following:
    /// 1. Either [`StateRootMessage::PrefetchProofs`] or [`StateRootMessage::StateUpdate`] is
    ///    received from the engine.
    ///    * For [`StateRootMessage::StateUpdate`], the state update is hashed with
    ///      [`evm_state_to_hashed_post_state`], and then (proof targets) [`MultiProofTargets`] are
    ///      extracted via [`HashedPostState::multi_proof_targets_difference`].
    ///    * For both messages, proof targets are deduplicated according to `fetched_proof_targets`,
    ///      so that the proofs for accounts and storage slots that were already fetched are not
    ///      requested again.
    /// 2. Using the proof targets, a new multiproof is calculated using
    ///    [`MultiProofManager::spawn_or_queue`].
    ///    * If the list of proof targets is empty, the [`StateRootMessage::EmptyProof`] message is
    ///      sent back to this task along with the original state update.
    ///    * Otherwise, the multiproof is calculated and the [`StateRootMessage::ProofCalculated`]
    ///      message is sent back to this task along with the resulting multiproof, proof targets
    ///      and original state update.
    /// 3. Either [`StateRootMessage::EmptyProof`] or [`StateRootMessage::ProofCalculated`] is
    ///    received.
    ///    * The multiproof is added to the (proof sequencer)[`ProofSequencer`].
    ///    * If the proof sequencer has a contiguous sequence of multiproofs in the same order as
    ///      state updates arrived (i.e. transaction order), such sequence is returned.
    /// 4. Once there's a sequence of contiguous multiproofs along with the proof targets and state
    ///    updates associated with them, a [`SparseTrieUpdate`] is generated and sent to the sparse
    ///    trie task that's running in [`run_sparse_trie`].
    ///    * Sparse trie task reveals the multiproof, updates the sparse trie, computes storage trie
    ///      roots, and calculates RLP nodes of the state trie below
    ///      [`SPARSE_TRIE_INCREMENTAL_LEVEL`].
    /// 5. Steps above are repeated until this task receives a
    ///    [`StateRootMessage::FinishedStateUpdates`].
    ///    * Once this message is received, on every [`StateRootMessage::EmptyProof`] and
    ///      [`StateRootMessage::ProofCalculated`] message, we check if there are any proofs are
    ///      currently being calculated, or if there are any pending proofs in the proof sequencer
    ///      left to be revealed using [`check_end_condition`].
    ///    * If there are none left, we drop the sparse trie task sender channel, and it signals
    ///      [`run_sparse_trie`] to calculate the state root of the full state trie, and send it
    ///      back to this task via [`StateRootMessage::RootCalculated`] message.
    /// 6. On [`StateRootMessage::RootCalculated`] message, the loop exits and the the state root is
    ///    returned.
    fn run(mut self, sparse_trie_tx: Sender<SparseTrieUpdate>) -> StateRootResult {
        let mut sparse_trie_tx = Some(sparse_trie_tx);

        let mut prefetch_proofs_received = 0;
        let mut updates_received = 0;
        let mut proofs_processed = 0;

        let mut updates_finished = false;

        // Timestamp when the first state update was received
        let mut first_update_time = None;
        // Timestamp when the last state update was received
        let mut last_update_time = None;

        loop {
            trace!(target: "engine::root", "entering main channel receiving loop");
            match self.rx.recv() {
                Ok(message) => match message {
                    StateRootMessage::PrefetchProofs(targets) => {
                        trace!(target: "engine::root", "processing StateRootMessage::PrefetchProofs");
                        prefetch_proofs_received += 1;
                        debug!(
                            target: "engine::root",
                            targets = targets.len(),
                            storage_targets = targets.values().map(|slots| slots.len()).sum::<usize>(),
                            total_prefetches = prefetch_proofs_received,
                            "Prefetching proofs"
                        );
                        self.on_prefetch_proof(targets);
                    }
                    StateRootMessage::StateUpdate(source, update) => {
                        trace!(target: "engine::root", "processing StateRootMessage::StateUpdate");
                        if updates_received == 0 {
                            first_update_time = Some(Instant::now());
                            debug!(target: "engine::root", "Started state root calculation");
                        }
                        last_update_time = Some(Instant::now());

                        updates_received += 1;
                        debug!(
                            target: "engine::root",
                            ?source,
                            len = update.len(),
                            total_updates = updates_received,
                            "Received new state update"
                        );
                        let next_sequence = self.proof_sequencer.next_sequence();
                        self.on_state_update(source, update, next_sequence);
                    }
                    StateRootMessage::FinishedStateUpdates => {
                        trace!(target: "engine::root", "processing StateRootMessage::FinishedStateUpdates");
                        updates_finished = true;

                        if check_end_condition(CheckEndConditionParams {
                            proofs_processed,
                            updates_received,
                            prefetch_proofs_received,
                            updates_finished,
                            proof_sequencer: &self.proof_sequencer,
                        }) {
                            sparse_trie_tx.take();
                            debug!(
                                target: "engine::root",
                                "State updates finished and all proofs processed, ending calculation"
                            );
                        };
                    }
                    StateRootMessage::EmptyProof { sequence_number, state } => {
                        trace!(target: "engine::root", "processing StateRootMessage::EmptyProof");

                        proofs_processed += 1;

                        if let Some(combined_update) = self.on_proof(
                            sequence_number,
                            SparseTrieUpdate { state, multiproof: MultiProof::default() },
                        ) {
                            let _ = sparse_trie_tx
                                .as_ref()
                                .expect("tx not dropped")
                                .send(combined_update);
                        }

                        if check_end_condition(CheckEndConditionParams {
                            proofs_processed,
                            updates_received,
                            prefetch_proofs_received,
                            updates_finished,
                            proof_sequencer: &self.proof_sequencer,
                        }) {
                            sparse_trie_tx.take();
                            debug!(
                                target: "engine::root",
                                "State updates finished and all proofs processed, ending calculation"
                            );
                        };
                    }
                    StateRootMessage::ProofCalculated(proof_calculated) => {
                        trace!(target: "engine::root", "processing StateRootMessage::ProofCalculated");

                        // we increment proofs_processed for both state updates and prefetches,
                        // because both are used for the root termination condition.
                        proofs_processed += 1;

                        self.metrics
                            .proof_calculation_duration_histogram
                            .record(proof_calculated.elapsed);
                        self.metrics
                            .proof_calculation_account_targets_histogram
                            .record(proof_calculated.account_targets as f64);
                        self.metrics
                            .proof_calculation_storage_targets_histogram
                            .record(proof_calculated.storage_targets as f64);

                        debug!(
                            target: "engine::root",
                            sequence = proof_calculated.sequence_number,
                            total_proofs = proofs_processed,
                            "Processing calculated proof"
                        );

                        self.multiproof_manager.on_calculation_complete();

                        if let Some(combined_update) =
                            self.on_proof(proof_calculated.sequence_number, proof_calculated.update)
                        {
                            let _ = sparse_trie_tx
                                .as_ref()
                                .expect("tx not dropped")
                                .send(combined_update);
                        }

                        if check_end_condition(CheckEndConditionParams {
                            proofs_processed,
                            updates_received,
                            prefetch_proofs_received,
                            updates_finished,
                            proof_sequencer: &self.proof_sequencer,
                        }) {
                            sparse_trie_tx.take();
                            debug!(
                                target: "engine::root",
                                "State updates finished and all proofs processed, ending calculation"
                            );
                        };
                    }
                    StateRootMessage::RootCalculated { state_root, trie_updates, iterations } => {
                        trace!(target: "engine::root", "processing StateRootMessage::RootCalculated");
                        let total_time =
                            first_update_time.expect("first update time should be set").elapsed();
                        let time_from_last_update =
                            last_update_time.expect("last update time should be set").elapsed();
                        debug!(
                            target: "engine::root",
                            total_updates = updates_received,
                            total_proofs = proofs_processed,
                            roots_calculated = iterations,
                            ?total_time,
                            ?time_from_last_update,
                            "All proofs processed, ending calculation"
                        );

                        self.metrics
                            .state_updates_received_histogram
                            .record(updates_received as f64);
                        self.metrics.proofs_processed_histogram.record(proofs_processed as f64);
                        self.metrics.state_root_iterations_histogram.record(iterations as f64);

                        return Ok(StateRootComputeOutcome {
                            state_root: (state_root, trie_updates),
                            total_time,
                            time_from_last_update,
                        });
                    }

                    StateRootMessage::ProofCalculationError(e) => {
                        return Err(ParallelStateRootError::Other(format!(
                            "could not calculate multiproof: {e:?}"
                        )))
                    }
                    StateRootMessage::RootCalculationError(e) => {
                        return Err(ParallelStateRootError::Other(format!(
                            "could not calculate state root: {e:?}"
                        )))
                    }
                },
                Err(_) => {
                    // this means our internal message channel is closed, which shouldn't happen
                    // in normal operation since we hold both ends
                    error!(
                        target: "engine::root",
                        "Internal message channel closed unexpectedly"
                    );
                    return Err(ParallelStateRootError::Other(
                        "Internal message channel closed unexpectedly".into(),
                    ));
                }
            }
        }
    }
}

/// Convenience params struct to pass to [`check_end_condition`].
struct CheckEndConditionParams<'a> {
    proofs_processed: u64,
    updates_received: u64,
    prefetch_proofs_received: u64,
    updates_finished: bool,
    proof_sequencer: &'a ProofSequencer,
}

// Returns true if all state updates finished and all profs processed.
fn check_end_condition(
    CheckEndConditionParams {
        proofs_processed,
        updates_received,
        prefetch_proofs_received,
        updates_finished,
        proof_sequencer,
    }: CheckEndConditionParams<'_>,
) -> bool {
    let all_proofs_received = proofs_processed >= updates_received + prefetch_proofs_received;
    let no_pending = !proof_sequencer.has_pending();
    debug!(
        target: "engine::root",
        proofs_processed,
        updates_received,
        prefetch_proofs_received,
        no_pending,
        updates_finished,
        "Checking end condition"
    );
    all_proofs_received && no_pending && updates_finished
}

/// Listen to incoming sparse trie updates and update the sparse trie.
///
/// Once the updates receiver channel is dropped, this sends the final state root, trie updates and
/// the number of update iterations to the `task_tx`.
///
/// This takes `task_tx` as an argument so that the state root result can be sent without blocking
/// on any of the `Drop` implementations run at the end of this method.
fn run_sparse_trie<Factory>(
    config: StateRootConfig<Factory>,
    metrics: StateRootTaskMetrics,
    update_rx: mpsc::Receiver<SparseTrieUpdate>,
    task_tx: Sender<StateRootMessage>,
) -> Result<(), ParallelStateRootError>
where
    Factory: DatabaseProviderFactory<Provider: BlockReader> + StateCommitmentProvider,
{
    let provider_ro = config.consistent_view.provider_ro()?;
    let in_memory_trie_cursor = InMemoryTrieCursorFactory::new(
        DatabaseTrieCursorFactory::new(provider_ro.tx_ref()),
        &config.nodes_sorted,
    );
    let blinded_provider_factory = ProofBlindedProviderFactory::new(
        in_memory_trie_cursor.clone(),
        HashedPostStateCursorFactory::new(
            DatabaseHashedCursorFactory::new(provider_ro.tx_ref()),
            &config.state_sorted,
        ),
        config.prefix_sets.clone(),
    );

    let mut num_iterations = 0;
    let mut trie = SparseStateTrie::new(blinded_provider_factory).with_updates(true);

    while let Ok(mut update) = update_rx.recv() {
        num_iterations += 1;
        let mut num_updates = 1;
        while let Ok(next) = update_rx.try_recv() {
            update.extend(next);
            num_updates += 1;
        }

        debug!(
            target: "engine::root",
            num_updates,
            account_proofs = update.multiproof.account_subtree.len(),
            storage_proofs = update.multiproof.storages.len(),
            "Updating sparse trie"
        );

        let elapsed = update_sparse_trie(&mut trie, update).map_err(|e| {
            ParallelStateRootError::Other(format!("could not calculate state root: {e:?}"))
        })?;
        metrics.sparse_trie_update_duration_histogram.record(elapsed);
        trace!(target: "engine::root", ?elapsed, num_iterations, "Root calculation completed");
    }

    debug!(target: "engine::root", num_iterations, "All proofs processed, ending calculation");

    let start = Instant::now();
    let (state_root, trie_updates) = trie.root_with_updates().map_err(|e| {
        ParallelStateRootError::Other(format!("could not calculate state root: {e:?}"))
    })?;
    let elapsed = start.elapsed();
    metrics.sparse_trie_final_update_duration_histogram.record(elapsed);

    let _ = task_tx.send(StateRootMessage::RootCalculated {
        state_root,
        trie_updates,
        iterations: num_iterations,
    });
    Ok(())
}

/// Calculate multiproof for the targets.
#[inline]
fn calculate_multiproof<Factory>(
    thread_pool: Arc<rayon::ThreadPool>,
    config: StateRootConfig<Factory>,
    proof_targets: MultiProofTargets,
) -> ProviderResult<MultiProof>
where
    Factory:
        DatabaseProviderFactory<Provider: BlockReader> + StateCommitmentProvider + Clone + 'static,
{
    Ok(ParallelProof::new(
        config.consistent_view,
        config.nodes_sorted,
        config.state_sorted,
        config.prefix_sets,
        thread_pool,
    )
    .with_branch_node_masks(true)
    .multiproof(proof_targets)?)
}

/// Updates the sparse trie with the given proofs and state, and returns the elapsed time.
fn update_sparse_trie<BPF>(
    trie: &mut SparseStateTrie<BPF>,
    SparseTrieUpdate { state, multiproof }: SparseTrieUpdate,
) -> SparseStateTrieResult<Duration>
where
    BPF: BlindedProviderFactory + Send + Sync,
    BPF::AccountNodeProvider: BlindedProvider + Send + Sync,
    BPF::StorageNodeProvider: BlindedProvider + Send + Sync,
{
    trace!(target: "engine::root::sparse", "Updating sparse trie");
    let started_at = Instant::now();

    // Reveal new accounts and storage slots.
    trie.reveal_multiproof(multiproof)?;

    // Update storage slots with new values and calculate storage roots.
    let (tx, rx) = mpsc::channel();
    state
        .storages
        .into_iter()
        .map(|(address, storage)| (address, storage, trie.take_storage_trie(&address)))
        .par_bridge()
        .map(|(address, storage, storage_trie)| {
            let span = trace_span!(target: "engine::root::sparse", "Storage trie", ?address);
            let _enter = span.enter();
            trace!(target: "engine::root::sparse", "Updating storage");
            let mut storage_trie = storage_trie.ok_or(SparseTrieErrorKind::Blind)?;

            if storage.wiped {
                trace!(target: "engine::root::sparse", "Wiping storage");
                storage_trie.wipe()?;
            }
            for (slot, value) in storage.storage {
                let slot_nibbles = Nibbles::unpack(slot);
                if value.is_zero() {
                    trace!(target: "engine::root::sparse", ?slot, "Removing storage slot");
                    storage_trie.remove_leaf(&slot_nibbles)?;
                } else {
                    trace!(target: "engine::root::sparse", ?slot, "Updating storage slot");
                    storage_trie
                        .update_leaf(slot_nibbles, alloy_rlp::encode_fixed_size(&value).to_vec())?;
                }
            }

            storage_trie.root();

            SparseStateTrieResult::Ok((address, storage_trie))
        })
        .for_each_init(|| tx.clone(), |tx, result| tx.send(result).unwrap());
    drop(tx);
    for result in rx {
        let (address, storage_trie) = result?;
        trie.insert_storage_trie(address, storage_trie);
    }

    // Update accounts with new values
    for (address, account) in state.accounts {
        trace!(target: "engine::root::sparse", ?address, "Updating account");
        trie.update_account(address, account.unwrap_or_default())?;
    }

    trie.calculate_below_level(SPARSE_TRIE_INCREMENTAL_LEVEL);
    let elapsed = started_at.elapsed();

    Ok(elapsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_proof_in_sequence() {
        let mut sequencer = ProofSequencer::new();
        let proof1 = MultiProof::default();
        let proof2 = MultiProof::default();
        sequencer.next_sequence = 2;

        let ready = sequencer.add_proof(0, SparseTrieUpdate::from_multiproof(proof1));
        assert_eq!(ready.len(), 1);
        assert!(!sequencer.has_pending());

        let ready = sequencer.add_proof(1, SparseTrieUpdate::from_multiproof(proof2));
        assert_eq!(ready.len(), 1);
        assert!(!sequencer.has_pending());
    }

    #[test]
    fn test_add_proof_out_of_order() {
        let mut sequencer = ProofSequencer::new();
        let proof1 = MultiProof::default();
        let proof2 = MultiProof::default();
        let proof3 = MultiProof::default();
        sequencer.next_sequence = 3;

        let ready = sequencer.add_proof(2, SparseTrieUpdate::from_multiproof(proof3));
        assert_eq!(ready.len(), 0);
        assert!(sequencer.has_pending());

        let ready = sequencer.add_proof(0, SparseTrieUpdate::from_multiproof(proof1));
        assert_eq!(ready.len(), 1);
        assert!(sequencer.has_pending());

        let ready = sequencer.add_proof(1, SparseTrieUpdate::from_multiproof(proof2));
        assert_eq!(ready.len(), 2);
        assert!(!sequencer.has_pending());
    }

    #[test]
    fn test_add_proof_with_gaps() {
        let mut sequencer = ProofSequencer::new();
        let proof1 = MultiProof::default();
        let proof3 = MultiProof::default();
        sequencer.next_sequence = 3;

        let ready = sequencer.add_proof(0, SparseTrieUpdate::from_multiproof(proof1));
        assert_eq!(ready.len(), 1);

        let ready = sequencer.add_proof(2, SparseTrieUpdate::from_multiproof(proof3));
        assert_eq!(ready.len(), 0);
        assert!(sequencer.has_pending());
    }

    #[test]
    fn test_add_proof_duplicate_sequence() {
        let mut sequencer = ProofSequencer::new();
        let proof1 = MultiProof::default();
        let proof2 = MultiProof::default();

        let ready = sequencer.add_proof(0, SparseTrieUpdate::from_multiproof(proof1));
        assert_eq!(ready.len(), 1);

        let ready = sequencer.add_proof(0, SparseTrieUpdate::from_multiproof(proof2));
        assert_eq!(ready.len(), 0);
        assert!(!sequencer.has_pending());
    }

    #[test]
    fn test_add_proof_batch_processing() {
        let mut sequencer = ProofSequencer::new();
        let proofs: Vec<_> = (0..5).map(|_| MultiProof::default()).collect();
        sequencer.next_sequence = 5;

        sequencer.add_proof(4, SparseTrieUpdate::from_multiproof(proofs[4].clone()));
        sequencer.add_proof(2, SparseTrieUpdate::from_multiproof(proofs[2].clone()));
        sequencer.add_proof(1, SparseTrieUpdate::from_multiproof(proofs[1].clone()));
        sequencer.add_proof(3, SparseTrieUpdate::from_multiproof(proofs[3].clone()));

        let ready = sequencer.add_proof(0, SparseTrieUpdate::from_multiproof(proofs[0].clone()));
        assert_eq!(ready.len(), 5);
        assert!(!sequencer.has_pending());
    }
}
