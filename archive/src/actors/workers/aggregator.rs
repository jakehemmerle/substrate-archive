// Copyright 2017-2019 Parity Technologies (UK) Ltd.
// This file is part of substrate-archive.

// substrate-archive is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// substrate-archive is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with substrate-archive.  If not, see <http://www.gnu.org/licenses/>.

use super::ActorContext;
use crate::{
    backend::BlockChanges,
    error::ArchiveResult,
    threadpools::BlockData,
    types::{BatchBlock, Block, Storage},
};
use flume::Sender;
use itertools::{EitherOrBoth, Itertools};
use sp_runtime::traits::{Block as BlockT, NumberFor};
use std::{iter::FromIterator, time::Duration};
use xtra::prelude::*;

/// how often to check threadpools for finished work (in milli-seconds)
pub const SYSTEM_TICK: u64 = 1000;

// channels are used to avoid putting mutex on a VecDeque
/// Actor that combines individual types into sequences
/// results in batch inserts into the database (better perf)
/// also easier telemetry/logging
/// Handles sending and receiving messages from threadpools
pub struct Aggregator<B>
where
    B: BlockT,
    NumberFor<B>: Into<u32>,
{
    senders: Senders<B>,
    recvs: Option<Receivers<B>>,
    /// actor which inserts blocks into the database
    db_addr: Address<super::Database>,
    /// Actor which manages getting the runtime metadata for blocks
    /// and sending them to the database actor
    meta_addr: Address<super::Metadata>,
    /// Pooled Postgres Database Connections
    exec: Sender<BlockData<B>>,
    /// just a switch so we know not to print redundant messages
    last_count_was_0: bool,
}

fn queues<B>() -> (Senders<B>, Receivers<B>)
where
    B: BlockT,
    NumberFor<B>: Into<u32>,
{
    let (storage_tx, storage_rx) = flume::unbounded();
    let (block_tx, block_rx) = flume::unbounded();
    (
        Senders {
            storage_queue: storage_tx,
            block_queue: block_tx,
        },
        Receivers {
            storage_recv: storage_rx,
            block_recv: block_rx,
        },
    )
}

/// Internal struct representing a queue built around message-passing
/// Sending/Receiving ends of queues to send batches of data to actors
struct Senders<B: BlockT> {
    /// sending end of an internal queue to send batches of storage to actors
    storage_queue: Sender<BlockChanges<B>>,
    /// sending end of an internal queue to send batches of blocks to actors
    block_queue: Sender<Block<B>>,
}

struct Receivers<B: BlockT> {
    /// receiving end of an internal queue to send batches of storage to actors
    storage_recv: flume::Receiver<BlockChanges<B>>,
    /// receiving end of an internal queue to send batches of blocks to actors
    block_recv: flume::Receiver<Block<B>>,
}

enum BlockOrStorage<B: BlockT> {
    Block(Block<B>),
    BatchBlock(BatchBlock<B>),
    Storage(BlockChanges<B>),
}

impl<B> Senders<B>
where
    B: BlockT,
    NumberFor<B>: Into<u32>,
{
    fn push_back(&self, t: BlockOrStorage<B>) -> ArchiveResult<()> {
        match t {
            BlockOrStorage::Block(b) => self.block_queue.send(b)?,
            BlockOrStorage::Storage(s) => self.storage_queue.send(s)?,
            BlockOrStorage::BatchBlock(v) => {
                for b in v.inner.into_iter() {
                    self.block_queue.send(b)?;
                }
            }
        }
        Ok(())
    }
}

impl<B> Aggregator<B>
where
    B: BlockT,
    NumberFor<B>: Into<u32>,
    NumberFor<B>: From<u32>,
{
    pub async fn new(
        ctx: ActorContext<B>,
        tx: Sender<BlockData<B>>,
        pool: &sqlx::PgPool,
    ) -> ArchiveResult<Self> {
        let (psql_url, rpc_url) = (ctx.psql_url().to_string(), ctx.rpc_url().to_string());
        let db_addr = super::Database::new(psql_url).await?.spawn();
        let meta_addr = super::Metadata::new(rpc_url, &pool, db_addr.clone()).spawn();
        let (senders, recvs) = queues();

        Ok(Self {
            senders,
            recvs: Some(recvs),
            db_addr,
            meta_addr,
            exec: tx,
            last_count_was_0: false,
        })
    }
}

impl<B: BlockT> Message for BlockChanges<B> {
    type Result = ArchiveResult<()>;
}

impl<B> Actor for Aggregator<B>
where
    B: BlockT,
    NumberFor<B>: Into<u32>,
{
    fn started(&mut self, ctx: &mut Context<Self>) {
        if self.recvs.is_none() {
            let (sends, recvs) = queues();
            self.senders = sends;
            self.recvs = Some(recvs);
        }
        let this = self.recvs.take().expect("checked for none; qed");
        ctx.notify_interval(Duration::from_millis(SYSTEM_TICK), move || {
            this.storage_recv
                .drain()
                .map(Storage::from)
                .zip_longest(this.block_recv.drain())
                .collect::<BlockStorageCombo<B>>()
        });
    }
}

impl<B> SyncHandler<BlockChanges<B>> for Aggregator<B>
where
    B: BlockT,
    NumberFor<B>: Into<u32>,
{
    fn handle(&mut self, changes: BlockChanges<B>, _: &mut Context<Self>) -> ArchiveResult<()> {
        self.senders.push_back(BlockOrStorage::Storage(changes))
    }
}

impl<B> SyncHandler<Block<B>> for Aggregator<B>
where
    B: BlockT,
    NumberFor<B>: Into<u32>,
{
    fn handle(&mut self, block: Block<B>, _: &mut Context<Self>) -> ArchiveResult<()> {
        self.exec.send(BlockData::Single(block.clone()))?;
        self.senders.push_back(BlockOrStorage::Block(block))
    }
}

impl<B> SyncHandler<BatchBlock<B>> for Aggregator<B>
where
    B: BlockT,
    NumberFor<B>: Into<u32>,
{
    fn handle(&mut self, blocks: BatchBlock<B>, _: &mut Context<Self>) -> ArchiveResult<()> {
        self.exec.send(BlockData::Batch(blocks.inner.clone()))?;
        self.senders.push_back(BlockOrStorage::BatchBlock(blocks))
    }
}

struct BlockStorageCombo<B: BlockT>(BatchBlock<B>, super::msg::VecStorageWrap<B>);

impl<B: BlockT> Message for BlockStorageCombo<B> {
    type Result = ();
}

impl<B: BlockT> FromIterator<EitherOrBoth<Storage<B>, Block<B>>> for BlockStorageCombo<B> {
    fn from_iter<I: IntoIterator<Item = EitherOrBoth<Storage<B>, Block<B>>>>(iter: I) -> Self {
        let mut storage = Vec::new();
        let mut blocks = Vec::new();
        for i in iter {
            match i {
                EitherOrBoth::Left(s) => storage.push(s),
                EitherOrBoth::Right(b) => blocks.push(b),
                EitherOrBoth::Both(s, b) => {
                    storage.push(s);
                    blocks.push(b);
                }
            }
        }
        BlockStorageCombo(BatchBlock::new(blocks), super::msg::VecStorageWrap(storage))
    }
}

impl<B> SyncHandler<BlockStorageCombo<B>> for Aggregator<B>
where
    B: BlockT,
    NumberFor<B>: Into<u32>,
{
    fn handle(&mut self, data: BlockStorageCombo<B>, ctx: &mut Context<Self>) {
        let (blocks, storage) = (data.0, data.1);

        let (b, s) = (blocks.inner().len(), storage.0.len());
        let r = || -> ArchiveResult<()> {
            match (b, s) {
                (0, 0) => {
                    if !self.last_count_was_0 {
                        log::info!("Waiting on node, nothing left to index ...");
                        self.last_count_was_0 = true;
                    }
                }
                (b, 0) => {
                    self.meta_addr.do_send(blocks)?;
                    log::info!("Indexing Blocks {} bps", b);
                    self.last_count_was_0 = false;
                }
                (0, s) => {
                    self.db_addr.do_send(storage)?;
                    log::info!("Indexing Storage {} bps", s);
                    self.last_count_was_0 = false;
                }
                (b, s) => {
                    self.db_addr.do_send(storage)?;
                    self.meta_addr.do_send(blocks)?;
                    log::info!("Indexing Blocks {} bps, Indexing Storage {} bps", b, s);
                    self.last_count_was_0 = false;
                }
            };
            Ok(())
        };
        // receivers have dropped which means the system is stopping
        if let Err(_) = r() {
            ctx.stop()
        }
    }
}
