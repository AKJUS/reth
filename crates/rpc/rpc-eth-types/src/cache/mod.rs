//! Async caching support for eth RPC

use super::{EthStateCacheConfig, MultiConsumerLruCache};
use alloy_consensus::BlockHeader;
use alloy_eips::BlockHashOrNumber;
use alloy_primitives::B256;
use futures::{future::Either, stream::FuturesOrdered, Stream, StreamExt};
use reth_chain_state::CanonStateNotification;
use reth_errors::{ProviderError, ProviderResult};
use reth_execution_types::Chain;
use reth_primitives_traits::{Block, BlockBody, NodePrimitives, RecoveredBlock};
use reth_storage_api::{BlockReader, TransactionVariant};
use reth_tasks::{TaskSpawner, TokioTaskExecutor};
use schnellru::{ByLength, Limiter};
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::sync::{
    mpsc::{unbounded_channel, UnboundedSender},
    oneshot, Semaphore,
};
use tokio_stream::wrappers::UnboundedReceiverStream;

pub mod config;
pub mod db;
pub mod metrics;
pub mod multi_consumer;

/// The type that can send the response to a requested [`RecoveredBlock`]
type BlockTransactionsResponseSender<T> = oneshot::Sender<ProviderResult<Option<Vec<T>>>>;

/// The type that can send the response to a requested [`RecoveredBlock`]
type BlockWithSendersResponseSender<B> =
    oneshot::Sender<ProviderResult<Option<Arc<RecoveredBlock<B>>>>>;

/// The type that can send the response to the requested receipts of a block.
type ReceiptsResponseSender<R> = oneshot::Sender<ProviderResult<Option<Arc<Vec<R>>>>>;

type CachedBlockResponseSender<B> = oneshot::Sender<Option<Arc<RecoveredBlock<B>>>>;

type CachedBlockAndReceiptsResponseSender<B, R> =
    oneshot::Sender<(Option<Arc<RecoveredBlock<B>>>, Option<Arc<Vec<R>>>)>;

/// The type that can send the response to a requested header
type HeaderResponseSender<H> = oneshot::Sender<ProviderResult<H>>;

/// The type that can send the response with a chain of cached blocks
type CachedParentBlocksResponseSender<B> = oneshot::Sender<Vec<Arc<RecoveredBlock<B>>>>;

type BlockLruCache<B, L> = MultiConsumerLruCache<
    B256,
    Arc<RecoveredBlock<B>>,
    L,
    Either<
        BlockWithSendersResponseSender<B>,
        BlockTransactionsResponseSender<<<B as Block>::Body as BlockBody>::Transaction>,
    >,
>;

type ReceiptsLruCache<R, L> =
    MultiConsumerLruCache<B256, Arc<Vec<R>>, L, ReceiptsResponseSender<R>>;

type HeaderLruCache<H, L> = MultiConsumerLruCache<B256, H, L, HeaderResponseSender<H>>;

/// Provides async access to cached eth data
///
/// This is the frontend for the async caching service which manages cached data on a different
/// task.
#[derive(Debug)]
pub struct EthStateCache<N: NodePrimitives> {
    to_service: UnboundedSender<CacheAction<N::Block, N::Receipt>>,
}

impl<N: NodePrimitives> Clone for EthStateCache<N> {
    fn clone(&self) -> Self {
        Self { to_service: self.to_service.clone() }
    }
}

impl<N: NodePrimitives> EthStateCache<N> {
    /// Creates and returns both [`EthStateCache`] frontend and the memory bound service.
    fn create<Provider, Tasks>(
        provider: Provider,
        action_task_spawner: Tasks,
        max_blocks: u32,
        max_receipts: u32,
        max_headers: u32,
        max_concurrent_db_operations: usize,
    ) -> (Self, EthStateCacheService<Provider, Tasks>)
    where
        Provider: BlockReader<Block = N::Block, Receipt = N::Receipt>,
    {
        let (to_service, rx) = unbounded_channel();
        let service = EthStateCacheService {
            provider,
            full_block_cache: BlockLruCache::new(max_blocks, "blocks"),
            receipts_cache: ReceiptsLruCache::new(max_receipts, "receipts"),
            headers_cache: HeaderLruCache::new(max_headers, "headers"),
            action_tx: to_service.clone(),
            action_rx: UnboundedReceiverStream::new(rx),
            action_task_spawner,
            rate_limiter: Arc::new(Semaphore::new(max_concurrent_db_operations)),
        };
        let cache = Self { to_service };
        (cache, service)
    }

    /// Creates a new async LRU backed cache service task and spawns it to a new task via
    /// [`tokio::spawn`].
    ///
    /// See also [`Self::spawn_with`]
    pub fn spawn<Provider>(provider: Provider, config: EthStateCacheConfig) -> Self
    where
        Provider: BlockReader<Block = N::Block, Receipt = N::Receipt> + Clone + Unpin + 'static,
    {
        Self::spawn_with(provider, config, TokioTaskExecutor::default())
    }

    /// Creates a new async LRU backed cache service task and spawns it to a new task via the given
    /// spawner.
    ///
    /// The cache is memory limited by the given max bytes values.
    pub fn spawn_with<Provider, Tasks>(
        provider: Provider,
        config: EthStateCacheConfig,
        executor: Tasks,
    ) -> Self
    where
        Provider: BlockReader<Block = N::Block, Receipt = N::Receipt> + Clone + Unpin + 'static,
        Tasks: TaskSpawner + Clone + 'static,
    {
        let EthStateCacheConfig {
            max_blocks,
            max_receipts,
            max_headers,
            max_concurrent_db_requests,
        } = config;
        let (this, service) = Self::create(
            provider,
            executor.clone(),
            max_blocks,
            max_receipts,
            max_headers,
            max_concurrent_db_requests,
        );
        executor.spawn_critical("eth state cache", Box::pin(service));
        this
    }

    /// Requests the  [`RecoveredBlock`] for the block hash
    ///
    /// Returns `None` if the block does not exist.
    pub async fn get_recovered_block(
        &self,
        block_hash: B256,
    ) -> ProviderResult<Option<Arc<RecoveredBlock<N::Block>>>> {
        let (response_tx, rx) = oneshot::channel();
        let _ = self.to_service.send(CacheAction::GetBlockWithSenders { block_hash, response_tx });
        rx.await.map_err(|_| CacheServiceUnavailable)?
    }

    /// Requests the receipts for the block hash
    ///
    /// Returns `None` if the block was not found.
    pub async fn get_receipts(
        &self,
        block_hash: B256,
    ) -> ProviderResult<Option<Arc<Vec<N::Receipt>>>> {
        let (response_tx, rx) = oneshot::channel();
        let _ = self.to_service.send(CacheAction::GetReceipts { block_hash, response_tx });
        rx.await.map_err(|_| CacheServiceUnavailable)?
    }

    /// Fetches both receipts and block for the given block hash.
    pub async fn get_block_and_receipts(
        &self,
        block_hash: B256,
    ) -> ProviderResult<Option<(Arc<RecoveredBlock<N::Block>>, Arc<Vec<N::Receipt>>)>> {
        let block = self.get_recovered_block(block_hash);
        let receipts = self.get_receipts(block_hash);

        let (block, receipts) = futures::try_join!(block, receipts)?;

        Ok(block.zip(receipts))
    }

    /// Retrieves receipts and blocks from cache if block is in the cache, otherwise only receipts.
    pub async fn get_receipts_and_maybe_block(
        &self,
        block_hash: B256,
    ) -> ProviderResult<Option<(Arc<Vec<N::Receipt>>, Option<Arc<RecoveredBlock<N::Block>>>)>> {
        let (response_tx, rx) = oneshot::channel();
        let _ = self.to_service.send(CacheAction::GetCachedBlock { block_hash, response_tx });

        let receipts = self.get_receipts(block_hash);

        let (receipts, block) = futures::join!(receipts, rx);

        let block = block.map_err(|_| CacheServiceUnavailable)?;
        Ok(receipts?.map(|r| (r, block)))
    }

    /// Retrieves both block and receipts from cache if available.
    pub async fn maybe_cached_block_and_receipts(
        &self,
        block_hash: B256,
    ) -> ProviderResult<(Option<Arc<RecoveredBlock<N::Block>>>, Option<Arc<Vec<N::Receipt>>>)> {
        let (response_tx, rx) = oneshot::channel();
        let _ = self
            .to_service
            .send(CacheAction::GetCachedBlockAndReceipts { block_hash, response_tx });
        rx.await.map_err(|_| CacheServiceUnavailable.into())
    }

    /// Streams cached receipts and blocks for a list of block hashes, preserving input order.
    #[allow(clippy::type_complexity)]
    pub fn get_receipts_and_maybe_block_stream<'a>(
        &'a self,
        hashes: Vec<B256>,
    ) -> impl Stream<
        Item = ProviderResult<
            Option<(Arc<Vec<N::Receipt>>, Option<Arc<RecoveredBlock<N::Block>>>)>,
        >,
    > + 'a {
        let futures = hashes.into_iter().map(move |hash| self.get_receipts_and_maybe_block(hash));

        futures.collect::<FuturesOrdered<_>>()
    }

    /// Requests the header for the given hash.
    ///
    /// Returns an error if the header is not found.
    pub async fn get_header(&self, block_hash: B256) -> ProviderResult<N::BlockHeader> {
        let (response_tx, rx) = oneshot::channel();
        let _ = self.to_service.send(CacheAction::GetHeader { block_hash, response_tx });
        rx.await.map_err(|_| CacheServiceUnavailable)?
    }

    /// Retrieves a chain of connected blocks from the cache, starting from the given block hash
    /// and traversing down through parent hashes. Returns blocks in descending order (newest
    /// first).
    /// This is useful for efficiently retrieving a sequence of blocks that might already be in
    /// cache without making separate database requests.
    /// Returns `None` if no blocks are found in the cache, otherwise returns `Some(Vec<...>)`
    /// with at least one block.
    pub async fn get_cached_parent_blocks(
        &self,
        block_hash: B256,
        max_blocks: usize,
    ) -> Option<Vec<Arc<RecoveredBlock<N::Block>>>> {
        let (response_tx, rx) = oneshot::channel();
        let _ = self.to_service.send(CacheAction::GetCachedParentBlocks {
            block_hash,
            max_blocks,
            response_tx,
        });

        let blocks = rx.await.unwrap_or_default();
        if blocks.is_empty() {
            None
        } else {
            Some(blocks)
        }
    }
}
/// Thrown when the cache service task dropped.
#[derive(Debug, thiserror::Error)]
#[error("cache service task stopped")]
pub struct CacheServiceUnavailable;

impl From<CacheServiceUnavailable> for ProviderError {
    fn from(err: CacheServiceUnavailable) -> Self {
        Self::other(err)
    }
}

/// A task that manages caches for data required by the `eth` rpc implementation.
///
/// It provides a caching layer on top of the given
/// [`StateProvider`](reth_storage_api::StateProvider) and keeps data fetched via the provider in
/// memory in an LRU cache. If the requested data is missing in the cache it is fetched and inserted
/// into the cache afterwards. While fetching data from disk is sync, this service is async since
/// requests and data is shared via channels.
///
/// This type is an endless future that listens for incoming messages from the user facing
/// [`EthStateCache`] via a channel. If the requested data is not cached then it spawns a new task
/// that does the IO and sends the result back to it. This way the caching service only
/// handles messages and does LRU lookups and never blocking IO.
///
/// Caution: The channel for the data is _unbounded_ it is assumed that this is mainly used by the
/// `reth_rpc::EthApi` which is typically invoked by the RPC server, which already uses
/// permits to limit concurrent requests.
#[must_use = "Type does nothing unless spawned"]
pub(crate) struct EthStateCacheService<
    Provider,
    Tasks,
    LimitBlocks = ByLength,
    LimitReceipts = ByLength,
    LimitHeaders = ByLength,
> where
    Provider: BlockReader,
    LimitBlocks: Limiter<B256, Arc<RecoveredBlock<Provider::Block>>>,
    LimitReceipts: Limiter<B256, Arc<Vec<Provider::Receipt>>>,
    LimitHeaders: Limiter<B256, Provider::Header>,
{
    /// The type used to lookup data from disk
    provider: Provider,
    /// The LRU cache for full blocks grouped by their block hash.
    full_block_cache: BlockLruCache<Provider::Block, LimitBlocks>,
    /// The LRU cache for block receipts grouped by the block hash.
    receipts_cache: ReceiptsLruCache<Provider::Receipt, LimitReceipts>,
    /// The LRU cache for headers.
    ///
    /// Headers are cached because they are required to populate the environment for execution
    /// (evm).
    headers_cache: HeaderLruCache<Provider::Header, LimitHeaders>,
    /// Sender half of the action channel.
    action_tx: UnboundedSender<CacheAction<Provider::Block, Provider::Receipt>>,
    /// Receiver half of the action channel.
    action_rx: UnboundedReceiverStream<CacheAction<Provider::Block, Provider::Receipt>>,
    /// The type that's used to spawn tasks that do the actual work
    action_task_spawner: Tasks,
    /// Rate limiter for spawned fetch tasks.
    ///
    /// This restricts the max concurrent fetch tasks at the same time.
    rate_limiter: Arc<Semaphore>,
}

impl<Provider, Tasks> EthStateCacheService<Provider, Tasks>
where
    Provider: BlockReader + Clone + Unpin + 'static,
    Tasks: TaskSpawner + Clone + 'static,
{
    fn on_new_block(
        &mut self,
        block_hash: B256,
        res: ProviderResult<Option<Arc<RecoveredBlock<Provider::Block>>>>,
    ) {
        if let Some(queued) = self.full_block_cache.remove(&block_hash) {
            // send the response to queued senders
            for tx in queued {
                match tx {
                    Either::Left(block_with_senders) => {
                        let _ = block_with_senders.send(res.clone());
                    }
                    Either::Right(transaction_tx) => {
                        let _ = transaction_tx.send(res.clone().map(|maybe_block| {
                            maybe_block.map(|block| block.body().transactions().to_vec())
                        }));
                    }
                }
            }
        }

        // cache good block
        if let Ok(Some(block)) = res {
            self.full_block_cache.insert(block_hash, block);
        }
    }

    fn on_new_receipts(
        &mut self,
        block_hash: B256,
        res: ProviderResult<Option<Arc<Vec<Provider::Receipt>>>>,
    ) {
        if let Some(queued) = self.receipts_cache.remove(&block_hash) {
            // send the response to queued senders
            for tx in queued {
                let _ = tx.send(res.clone());
            }
        }

        // cache good receipts
        if let Ok(Some(receipts)) = res {
            self.receipts_cache.insert(block_hash, receipts);
        }
    }

    fn on_reorg_block(
        &mut self,
        block_hash: B256,
        res: ProviderResult<Option<RecoveredBlock<Provider::Block>>>,
    ) {
        let res = res.map(|b| b.map(Arc::new));
        if let Some(queued) = self.full_block_cache.remove(&block_hash) {
            // send the response to queued senders
            for tx in queued {
                match tx {
                    Either::Left(block_with_senders) => {
                        let _ = block_with_senders.send(res.clone());
                    }
                    Either::Right(transaction_tx) => {
                        let _ = transaction_tx.send(res.clone().map(|maybe_block| {
                            maybe_block.map(|block| block.body().transactions().to_vec())
                        }));
                    }
                }
            }
        }
    }

    fn on_reorg_receipts(
        &mut self,
        block_hash: B256,
        res: ProviderResult<Option<Arc<Vec<Provider::Receipt>>>>,
    ) {
        if let Some(queued) = self.receipts_cache.remove(&block_hash) {
            // send the response to queued senders
            for tx in queued {
                let _ = tx.send(res.clone());
            }
        }
    }

    /// Shrinks the queues but leaves some space for the next requests
    fn shrink_queues(&mut self) {
        let min_capacity = 2;
        self.full_block_cache.shrink_to(min_capacity);
        self.receipts_cache.shrink_to(min_capacity);
        self.headers_cache.shrink_to(min_capacity);
    }

    fn update_cached_metrics(&self) {
        self.full_block_cache.update_cached_metrics();
        self.receipts_cache.update_cached_metrics();
        self.headers_cache.update_cached_metrics();
    }
}

impl<Provider, Tasks> Future for EthStateCacheService<Provider, Tasks>
where
    Provider: BlockReader + Clone + Unpin + 'static,
    Tasks: TaskSpawner + Clone + 'static,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        loop {
            let Poll::Ready(action) = this.action_rx.poll_next_unpin(cx) else {
                // shrink queues if we don't have any work to do
                this.shrink_queues();
                return Poll::Pending;
            };

            match action {
                None => {
                    unreachable!("can't close")
                }
                Some(action) => {
                    match action {
                        CacheAction::GetCachedBlock { block_hash, response_tx } => {
                            let _ =
                                response_tx.send(this.full_block_cache.get(&block_hash).cloned());
                        }
                        CacheAction::GetCachedBlockAndReceipts { block_hash, response_tx } => {
                            let block = this.full_block_cache.get(&block_hash).cloned();
                            let receipts = this.receipts_cache.get(&block_hash).cloned();
                            let _ = response_tx.send((block, receipts));
                        }
                        CacheAction::GetBlockWithSenders { block_hash, response_tx } => {
                            if let Some(block) = this.full_block_cache.get(&block_hash).cloned() {
                                let _ = response_tx.send(Ok(Some(block)));
                                continue
                            }

                            // block is not in the cache, request it if this is the first consumer
                            if this.full_block_cache.queue(block_hash, Either::Left(response_tx)) {
                                let provider = this.provider.clone();
                                let action_tx = this.action_tx.clone();
                                let rate_limiter = this.rate_limiter.clone();
                                let mut action_sender =
                                    ActionSender::new(CacheKind::Block, block_hash, action_tx);
                                this.action_task_spawner.spawn_blocking(Box::pin(async move {
                                    // Acquire permit
                                    let _permit = rate_limiter.acquire().await;
                                    // Only look in the database to prevent situations where we
                                    // looking up the tree is blocking
                                    let block_sender = provider
                                        .sealed_block_with_senders(
                                            BlockHashOrNumber::Hash(block_hash),
                                            TransactionVariant::WithHash,
                                        )
                                        .map(|maybe_block| maybe_block.map(Arc::new));
                                    action_sender.send_block(block_sender);
                                }));
                            }
                        }
                        CacheAction::GetReceipts { block_hash, response_tx } => {
                            // check if block is cached
                            if let Some(receipts) = this.receipts_cache.get(&block_hash).cloned() {
                                let _ = response_tx.send(Ok(Some(receipts)));
                                continue
                            }

                            // block is not in the cache, request it if this is the first consumer
                            if this.receipts_cache.queue(block_hash, response_tx) {
                                let provider = this.provider.clone();
                                let action_tx = this.action_tx.clone();
                                let rate_limiter = this.rate_limiter.clone();
                                let mut action_sender =
                                    ActionSender::new(CacheKind::Receipt, block_hash, action_tx);
                                this.action_task_spawner.spawn_blocking(Box::pin(async move {
                                    // Acquire permit
                                    let _permit = rate_limiter.acquire().await;
                                    let res = provider
                                        .receipts_by_block(block_hash.into())
                                        .map(|maybe_receipts| maybe_receipts.map(Arc::new));

                                    action_sender.send_receipts(res);
                                }));
                            }
                        }
                        CacheAction::GetHeader { block_hash, response_tx } => {
                            // check if the header is cached
                            if let Some(header) = this.headers_cache.get(&block_hash).cloned() {
                                let _ = response_tx.send(Ok(header));
                                continue
                            }

                            // it's possible we have the entire block cached
                            if let Some(block) = this.full_block_cache.get(&block_hash) {
                                let _ = response_tx.send(Ok(block.clone_header()));
                                continue
                            }

                            // header is not in the cache, request it if this is the first
                            // consumer
                            if this.headers_cache.queue(block_hash, response_tx) {
                                let provider = this.provider.clone();
                                let action_tx = this.action_tx.clone();
                                let rate_limiter = this.rate_limiter.clone();
                                let mut action_sender =
                                    ActionSender::new(CacheKind::Header, block_hash, action_tx);
                                this.action_task_spawner.spawn_blocking(Box::pin(async move {
                                    // Acquire permit
                                    let _permit = rate_limiter.acquire().await;
                                    let header = provider.header(&block_hash).and_then(|header| {
                                        header.ok_or_else(|| {
                                            ProviderError::HeaderNotFound(block_hash.into())
                                        })
                                    });
                                    action_sender.send_header(header);
                                }));
                            }
                        }
                        CacheAction::ReceiptsResult { block_hash, res } => {
                            this.on_new_receipts(block_hash, res);
                        }
                        CacheAction::BlockWithSendersResult { block_hash, res } => match res {
                            Ok(Some(block_with_senders)) => {
                                this.on_new_block(block_hash, Ok(Some(block_with_senders)));
                            }
                            Ok(None) => {
                                this.on_new_block(block_hash, Ok(None));
                            }
                            Err(e) => {
                                this.on_new_block(block_hash, Err(e));
                            }
                        },
                        CacheAction::HeaderResult { block_hash, res } => {
                            let res = *res;
                            if let Some(queued) = this.headers_cache.remove(&block_hash) {
                                // send the response to queued senders
                                for tx in queued {
                                    let _ = tx.send(res.clone());
                                }
                            }

                            // cache good header
                            if let Ok(data) = res {
                                this.headers_cache.insert(block_hash, data);
                            }
                        }
                        CacheAction::CacheNewCanonicalChain { chain_change } => {
                            for block in chain_change.blocks {
                                this.on_new_block(block.hash(), Ok(Some(Arc::new(block))));
                            }

                            for block_receipts in chain_change.receipts {
                                this.on_new_receipts(
                                    block_receipts.block_hash,
                                    Ok(Some(Arc::new(block_receipts.receipts))),
                                );
                            }
                        }
                        CacheAction::RemoveReorgedChain { chain_change } => {
                            for block in chain_change.blocks {
                                this.on_reorg_block(block.hash(), Ok(Some(block)));
                            }

                            for block_receipts in chain_change.receipts {
                                this.on_reorg_receipts(
                                    block_receipts.block_hash,
                                    Ok(Some(Arc::new(block_receipts.receipts))),
                                );
                            }
                        }
                        CacheAction::GetCachedParentBlocks {
                            block_hash,
                            max_blocks,
                            response_tx,
                        } => {
                            let mut blocks = Vec::new();
                            let mut current_hash = block_hash;

                            // Start with the requested block
                            while blocks.len() < max_blocks {
                                if let Some(block) =
                                    this.full_block_cache.get(&current_hash).cloned()
                                {
                                    // Get the parent hash for the next iteration
                                    current_hash = block.header().parent_hash();
                                    blocks.push(block);
                                } else {
                                    // Break the loop if we can't find the current block
                                    break;
                                }
                            }

                            let _ = response_tx.send(blocks);
                        }
                    };
                    this.update_cached_metrics();
                }
            }
        }
    }
}

/// All message variants sent through the channel
enum CacheAction<B: Block, R> {
    GetBlockWithSenders {
        block_hash: B256,
        response_tx: BlockWithSendersResponseSender<B>,
    },
    GetHeader {
        block_hash: B256,
        response_tx: HeaderResponseSender<B::Header>,
    },
    GetReceipts {
        block_hash: B256,
        response_tx: ReceiptsResponseSender<R>,
    },
    GetCachedBlock {
        block_hash: B256,
        response_tx: CachedBlockResponseSender<B>,
    },
    GetCachedBlockAndReceipts {
        block_hash: B256,
        response_tx: CachedBlockAndReceiptsResponseSender<B, R>,
    },
    BlockWithSendersResult {
        block_hash: B256,
        res: ProviderResult<Option<Arc<RecoveredBlock<B>>>>,
    },
    ReceiptsResult {
        block_hash: B256,
        res: ProviderResult<Option<Arc<Vec<R>>>>,
    },
    HeaderResult {
        block_hash: B256,
        res: Box<ProviderResult<B::Header>>,
    },
    CacheNewCanonicalChain {
        chain_change: ChainChange<B, R>,
    },
    RemoveReorgedChain {
        chain_change: ChainChange<B, R>,
    },
    GetCachedParentBlocks {
        block_hash: B256,
        max_blocks: usize,
        response_tx: CachedParentBlocksResponseSender<B>,
    },
}

struct BlockReceipts<R> {
    block_hash: B256,
    receipts: Vec<R>,
}

/// A change of the canonical chain
struct ChainChange<B: Block, R> {
    blocks: Vec<RecoveredBlock<B>>,
    receipts: Vec<BlockReceipts<R>>,
}

impl<B: Block, R: Clone> ChainChange<B, R> {
    fn new<N>(chain: Arc<Chain<N>>) -> Self
    where
        N: NodePrimitives<Block = B, Receipt = R>,
    {
        let (blocks, receipts): (Vec<_>, Vec<_>) = chain
            .blocks_and_receipts()
            .map(|(block, receipts)| {
                let block_receipts =
                    BlockReceipts { block_hash: block.hash(), receipts: receipts.clone() };
                (block.clone(), block_receipts)
            })
            .unzip();
        Self { blocks, receipts }
    }
}

/// Identifier for the caches.
#[derive(Copy, Clone, Debug)]
enum CacheKind {
    Block,
    Receipt,
    Header,
}

/// Drop aware sender struct that ensures a response is always emitted even if the db task panics
/// before a result could be sent.
///
/// This type wraps a sender and in case the sender is still present on drop emit an error response.
#[derive(Debug)]
struct ActionSender<B: Block, R: Send + Sync> {
    kind: CacheKind,
    blockhash: B256,
    tx: Option<UnboundedSender<CacheAction<B, R>>>,
}

impl<R: Send + Sync, B: Block> ActionSender<B, R> {
    const fn new(kind: CacheKind, blockhash: B256, tx: UnboundedSender<CacheAction<B, R>>) -> Self {
        Self { kind, blockhash, tx: Some(tx) }
    }

    fn send_block(&mut self, block_sender: Result<Option<Arc<RecoveredBlock<B>>>, ProviderError>) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(CacheAction::BlockWithSendersResult {
                block_hash: self.blockhash,
                res: block_sender,
            });
        }
    }

    fn send_receipts(&mut self, receipts: Result<Option<Arc<Vec<R>>>, ProviderError>) {
        if let Some(tx) = self.tx.take() {
            let _ =
                tx.send(CacheAction::ReceiptsResult { block_hash: self.blockhash, res: receipts });
        }
    }

    fn send_header(&mut self, header: Result<<B as Block>::Header, ProviderError>) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(CacheAction::HeaderResult {
                block_hash: self.blockhash,
                res: Box::new(header),
            });
        }
    }
}
impl<R: Send + Sync, B: Block> Drop for ActionSender<B, R> {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let msg = match self.kind {
                CacheKind::Block => CacheAction::BlockWithSendersResult {
                    block_hash: self.blockhash,
                    res: Err(CacheServiceUnavailable.into()),
                },
                CacheKind::Receipt => CacheAction::ReceiptsResult {
                    block_hash: self.blockhash,
                    res: Err(CacheServiceUnavailable.into()),
                },
                CacheKind::Header => CacheAction::HeaderResult {
                    block_hash: self.blockhash,
                    res: Box::new(Err(CacheServiceUnavailable.into())),
                },
            };
            let _ = tx.send(msg);
        }
    }
}

/// Awaits for new chain events and directly inserts them into the cache so they're available
/// immediately before they need to be fetched from disk.
///
/// Reorged blocks are removed from the cache.
pub async fn cache_new_blocks_task<St, N: NodePrimitives>(
    eth_state_cache: EthStateCache<N>,
    mut events: St,
) where
    St: Stream<Item = CanonStateNotification<N>> + Unpin + 'static,
{
    while let Some(event) = events.next().await {
        if let Some(reverted) = event.reverted() {
            let chain_change = ChainChange::new(reverted);

            let _ =
                eth_state_cache.to_service.send(CacheAction::RemoveReorgedChain { chain_change });
        }

        let chain_change = ChainChange::new(event.committed());

        let _ =
            eth_state_cache.to_service.send(CacheAction::CacheNewCanonicalChain { chain_change });
    }
}
