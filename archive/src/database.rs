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

//! IO for the PostgreSQL database connected to Substrate Archive Node
//! Handles inserting of data into the database

mod batch;
pub mod models;

use async_trait::async_trait;
use batch::Batch;
use codec::Encode;
use sp_runtime::traits::{Block as BlockT, Header as _, NumberFor};
use sqlx::{PgPool, Postgres};

use self::models::*;
use crate::{
    error::{ArchiveResult, Error as ArchiveError},
    types::*,
};

pub type DbReturn = Result<u64, ArchiveError>;
pub type DbConn = sqlx::pool::PoolConnection<Postgres>;

#[async_trait]
pub trait Insert: Sync {
    async fn insert(mut self, mut conn: DbConn) -> DbReturn
    where
        Self: Sized;
}

pub struct Database {
    /// pool of database connections
    pool: PgPool,
    url: String,
}

// clones a database connection
impl Clone for Database {
    fn clone(&self) -> Self {
        Database {
            pool: self.pool.clone(),
            url: self.url.clone(),
        }
    }
}

impl Database {
    /// Connect to the database
    pub async fn new(url: String) -> ArchiveResult<Self> {
        let pool = PgPool::builder()
            .min_size(16)
            .max_size(32)
            .build(url.as_str())
            .await?;
        Ok(Self { pool, url })
    }

    pub fn pool(&self) -> &sqlx::Pool<Postgres> {
        &self.pool
    }

    pub async fn insert(&self, data: impl Insert) -> ArchiveResult<u64> {
        let conn = self.pool.acquire().await?;
        data.insert(conn).await
    }
}

#[async_trait]
impl<B> Insert for Block<B>
where
    B: BlockT,
    NumberFor<B>: Into<u32>,
{
    async fn insert(mut self, mut conn: DbConn) -> DbReturn {
        log::info!("Inserting single block");
        log::trace!(
            "block_num = {:?}, hash = {:X?}",
            self.inner.block.header().number(),
            hex::encode(self.inner.block.header().hash().as_ref())
        );
        let query = sqlx::query(
            r#"
            INSERT INTO blocks (parent_hash, hash, block_num, state_root, extrinsics_root, digest, ext, spec)
            VALUES($1, $2, $3, $4, $5, $6, $7, $8)
            ON CONFLICT DO NOTHING
        "#,
        );
        let parent_hash = self.inner.block.header().parent_hash().as_ref();
        let hash = self.inner.block.header().hash();
        let block_num: u32 = (*self.inner.block.header().number()).into();
        let state_root = self.inner.block.header().state_root().as_ref();
        let extrinsics_root = self.inner.block.header().extrinsics_root().as_ref();
        let digest = self.inner.block.header().digest().encode();
        let extrinsics = self.inner.block.extrinsics().encode();

        query
            .bind(parent_hash)
            .bind(hash.as_ref())
            .bind(block_num)
            .bind(state_root)
            .bind(extrinsics_root)
            .bind(digest.as_slice())
            .bind(extrinsics.as_slice())
            .bind(self.spec)
            .execute(&mut conn)
            .await
            .map_err(Into::into)
    }
}

#[async_trait]
impl<B: BlockT> Insert for StorageModel<B> {
    async fn insert(mut self, mut conn: DbConn) -> DbReturn {
        log::info!("Inserting Single Storage");
        sqlx::query(
            r#"
                INSERT INTO storage (
                    block_num, hash, is_full, key, storage
                ) VALUES (#1, $2, $3, $4, $5)
                ON CONFLICT (hash, key, md5(storage)) DO UPDATE SET
                    hash = EXCLUDED.hash,
                    key = EXCLUDED.key,
                    storage = EXCLUDED.storage,
                    is_full = EXCLUDED.is_full
            "#,
        )
        .bind(self.block_num())
        .bind(self.hash().as_ref())
        .bind(self.is_full())
        .bind(self.key().0.as_slice())
        .bind(self.data().map(|d| d.0.as_slice()))
        .execute(&mut conn)
        .await
        .map_err(Into::into)
    }
}

#[async_trait]
impl<B: BlockT> Insert for Vec<StorageModel<B>> {
    async fn insert(mut self, mut conn: DbConn) -> DbReturn {
        let mut batch = Batch::new(
            "storage",
            r#"
            INSERT INTO "storage" (
                block_num, hash, is_full, key, storage
            ) VALUES
            "#,
            r#"
            ON CONFLICT (hash, key, md5(storage)) DO UPDATE SET
                hash = EXCLUDED.hash,
                key = EXCLUDED.key,
                storage = EXCLUDED.storage,
                is_full = EXCLUDED.is_full
            "#,
        );

        for s in self.into_iter() {
            batch.reserve(5)?;
            if batch.current_num_arguments() > 0 {
                batch.append(",");
            }
            batch.append("(");
            batch.bind(s.block_num())?;
            batch.append(",");
            batch.bind(s.hash().as_ref())?;
            batch.append(",");
            batch.bind(s.is_full())?;
            batch.append(",");
            batch.bind(s.key().0.as_slice())?;
            batch.append(",");
            batch.bind(s.data().map(|d| d.0.as_slice()))?;
            batch.append(")");
        }
        batch.execute(&mut conn).await?;
        Ok(0)
    }
}

#[async_trait]
impl Insert for Metadata {
    async fn insert(mut self, mut conn: DbConn) -> DbReturn {
        log::info!("Inserting Metadata");
        sqlx::query(
            r#"
            INSERT INTO metadata (version, meta)
            VALUES($1, $2)
            ON CONFLICT DO NOTHING
        "#,
        )
        .bind(self.version())
        .bind(self.meta())
        .execute(&mut conn)
        .await
        .map_err(Into::into)
    }
}

#[async_trait]
impl<B> Insert for BatchBlock<B>
where
    B: BlockT,
    NumberFor<B>: Into<u32>,
{
    async fn insert(mut self, mut conn: DbConn) -> DbReturn {
        let mut batch = Batch::new(
            "blocks",
            r#"
            INSERT INTO "blocks" (
                parent_hash, hash, block_num, state_root, extrinsics_root, digest, ext, spec
            ) VALUES
            "#,
            r#"
            ON CONFLICT DO NOTHING
            "#,
        );
        for b in self.inner.into_iter() {
            batch.reserve(8)?;
            if batch.current_num_arguments() > 0 {
                batch.append(",");
            }
            let parent_hash = b.inner.block.header().parent_hash().as_ref();
            let hash = b.inner.block.header().hash();
            let block_num: u32 = (*b.inner.block.header().number()).into();
            let state_root = b.inner.block.header().state_root().as_ref();
            let extrinsics_root = b.inner.block.header().extrinsics_root().as_ref();
            let digest = b.inner.block.header().digest().encode();
            let extrinsics = b.inner.block.extrinsics().encode();
            batch.append("(");
            batch.bind(parent_hash)?;
            batch.append(",");
            batch.bind(hash.as_ref())?;
            batch.append(",");
            batch.bind(block_num)?;
            batch.append(",");
            batch.bind(state_root)?;
            batch.append(",");
            batch.bind(extrinsics_root)?;
            batch.append(",");
            batch.bind(digest.as_slice())?;
            batch.append(",");
            batch.bind(extrinsics.as_slice())?;
            batch.append(",");
            batch.bind(b.spec)?;
            batch.append(")");
        }
        batch.execute(&mut conn).await?;
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    //! Must be connected to a local database
    use super::*;
}
