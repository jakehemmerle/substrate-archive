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

//! Network Actors
//! These aggregate data for child actors to work with
//! they mostly wait on network IO

use crate::actors::{
    self,
    scheduler::{Algorithm, Scheduler},
    workers,
};
use crate::{
    backend::ReadOnlyBackend,
    error::Error as ArchiveError,
    types::{NotSignedBlock, Substrate, System},
};
use bastion::prelude::*;
use jsonrpsee::client::Subscription;
use sp_runtime::generic::BlockId;
use sp_runtime::traits::Header as _;
use sqlx::PgConnection;
use std::sync::Arc;

/// Subscribe to new blocks via RPC
/// this is a worker that never stops
pub fn actor<T>(
    backend: Arc<ReadOnlyBackend<NotSignedBlock<T>>>,
    pool: sqlx::Pool<PgConnection>,
    url: String,
) -> Result<ChildrenRef, ArchiveError>
where
    T: Substrate + Send + Sync,
    <T as System>::BlockNumber: Into<u32>,
    <T as System>::Header: serde::de::DeserializeOwned,
{
    let meta_workers = workers::metadata::<T>(url.clone(), pool)?;
    // actor which produces work in the form of collecting blocks
    Bastion::children(|children| {
        children.with_exec(move |ctx: BastionContext| {
            let meta_workers = meta_workers.clone();
            let url: String = url.clone();
            let client = backend.clone();
            async move {
                let mut sched = Scheduler::new(Algorithm::RoundRobin, &ctx);
                sched.add_worker("meta", &meta_workers);
                match entry::<T>(&mut sched, client, url.as_str()).await {
                    Ok(_) => (),
                    Err(e) => log::error!("{:?}", e),
                };
                Bastion::stop();
                Ok(())
            }
        })
    })
    .map_err(|_| ArchiveError::from("Could not instantiate network generator"))
}

async fn entry<T>(
    sched: &mut Scheduler<'_>,
    client: Arc<ReadOnlyBackend<NotSignedBlock<T>>>,
    url: &str,
) -> Result<(), ArchiveError>
where
    T: Substrate + Send + Sync,
    <T as System>::BlockNumber: Into<u32>,
    <T as System>::Header: serde::de::DeserializeOwned,
{
    let rpc = actors::connect::<T>(url).await;
    let mut subscription = rpc
        .subscribe_finalized_heads()
        .await
        .map_err(ArchiveError::from)?;
    loop {
        if handle_shutdown::<T, _>(sched.context(), &mut subscription).await {
            break;
        }
        let head = subscription.next().await;
        let block = client.block(&BlockId::Number(*head.number()));
        if let Some(b) = block {
            log::trace!("{:?}", b);
            sched.tell_next("meta", b)?
        } else {
            log::warn!("Block does not exist!");
        }
    }
    Ok(())
}

async fn handle_shutdown<T, N>(ctx: &BastionContext, subscription: &mut Subscription<N>) -> bool
where
    T: Substrate + Send + Sync,
{
    if let Some(msg) = ctx.try_recv().await {
        msg! {
            msg,
            ref broadcast: super::Broadcast => {
                match broadcast {
                    super::Broadcast::Shutdown => {
                        // dropping a jsonrpsee::Subscription unsubscribes
                        std::mem::drop(subscription);
                        return true;
                    }
                }
            };
            e: _ => log::warn!("Received unknown message: {:?}", e);
        };
    }
    false
}
