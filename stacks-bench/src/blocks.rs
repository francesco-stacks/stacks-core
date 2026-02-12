use std::fmt::Display;

use anyhow::{Result, anyhow, bail};
use stacks_common::types::chainstate::StacksBlockId;

use crate::StacksBlockHeader;

#[derive(Clone, Debug)]
pub struct BlockRef {
    pub id: StacksBlockId,
    pub height: u64,
}

impl Display for BlockRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.id, self.height)
    }
}

/// Trait for caching and retrieving block ancestors to speed up chain walking.
pub trait ChainCache {
    /// Finds the closest known ancestor of `tip` that has a height >= `target_height`.
    /// Returns `Some((block_id, height))` if found.
    fn find_closest_ancestor(
        &self,
        tip: &StacksBlockId,
        target_height: u64,
    ) -> impl Future<Output = Result<Option<(StacksBlockId, u64)>>>;

    /// Caches a known ancestor for a given tip.
    fn cache_ancestor(
        &mut self,
        tip: &StacksBlockId,
        height: u64,
        block: &StacksBlockId,
    ) -> impl Future<Output = Result<()>>;
}

pub struct NoopChainCache;
impl ChainCache for NoopChainCache {
    async fn find_closest_ancestor(
        &self,
        _tip: &StacksBlockId,
        _target_height: u64,
    ) -> Result<Option<(StacksBlockId, u64)>> {
        Ok(None)
    }

    async fn cache_ancestor(
        &mut self,
        _tip: &StacksBlockId,
        _height: u64,
        _block: &StacksBlockId,
    ) -> Result<()> {
        Ok(())
    }
}

/// Implement ChainCache for mutable references so we can pass `&mut AppDb`
impl<T: ChainCache + ?Sized> ChainCache for &mut T {
    fn find_closest_ancestor(
        &self,
        tip: &StacksBlockId,
        target_height: u64,
    ) -> impl Future<Output = Result<Option<(StacksBlockId, u64)>>> {
        (**self).find_closest_ancestor(tip, target_height)
    }

    fn cache_ancestor(
        &mut self,
        tip: &StacksBlockId,
        height: u64,
        block: &StacksBlockId,
    ) -> impl Future<Output = Result<()>> {
        (**self).cache_ancestor(tip, height, block)
    }
}

pub trait BlockHeaderProvider: Send {
    fn get_header(
        &self,
        id: &StacksBlockId,
    ) -> impl Future<Output = Result<Option<StacksBlockHeader>>>;
}

pub struct BackwardsBlockStream<P, C = NoopChainCache> {
    provider: P,
    current_id: StacksBlockId,
    cache: C,
}

impl<P, C> BackwardsBlockStream<P, C> {
    pub fn into_inner(self) -> P {
        self.provider
    }
}

impl<P: BlockHeaderProvider> BackwardsBlockStream<P, NoopChainCache> {
    pub fn new(provider: P, start_id: StacksBlockId) -> Self {
        Self {
            provider,
            current_id: start_id,
            cache: NoopChainCache,
        }
    }
}

impl<P: BlockHeaderProvider, C: ChainCache> BackwardsBlockStream<P, C> {
    /// Transforms the stream to use a different cache provider.
    /// This is useful for chaining: `BackwardsBlockStream::new(...).with_cache(app_db)`
    pub fn with_cache<NewC: ChainCache>(self, cache: NewC) -> BackwardsBlockStream<P, NewC> {
        BackwardsBlockStream {
            provider: self.provider,
            current_id: self.current_id,
            cache,
        }
    }

    pub async fn next_block(&mut self) -> Result<Option<StacksBlockHeader>> {
        let header_opt = self.provider.get_header(&self.current_id).await?;
        match header_opt {
            Some(header) => {
                self.current_id = header.parent_id.clone();

                if Self::should_cache_block(header.height) {
                    let _ = self
                        .cache
                        .cache_ancestor(&header.id, header.height, &self.current_id)
                        .await;
                }
                Ok(Some(header))
            }
            None => Ok(None),
        }
    }

    pub async fn seek_to_height(
        &mut self,
        target_height: u64,
        anchor_tip: &StacksBlockId,
    ) -> Result<StacksBlockHeader> {
        // 1. Get current state
        let mut header = self
            .provider
            .get_header(&self.current_id)
            .await?
            .ok_or_else(|| anyhow!("Missing header for {}", self.current_id))?;

        let mut curr_h = header.height;

        if curr_h == target_height {
            return Ok(header);
        }

        // 2. Try cache jump
        if let Ok(Some((cached_id, cached_h))) = self
            .cache
            .find_closest_ancestor(anchor_tip, target_height)
            .await
            && cached_h < curr_h
            && cached_h >= target_height
        {
            println!("  [Cache Hit] Jumping from height {curr_h} to {cached_h} ({cached_id})");
            self.current_id = cached_id;
            // Fetch header for new location
            header = self
                .provider
                .get_header(&self.current_id)
                .await?
                .ok_or_else(|| anyhow!("Missing header for {}", self.current_id))?;
            curr_h = header.height;
        }

        // 3. Walk back
        while curr_h > target_height {
            // We already have 'header' for 'curr_h'.
            // Move to parent.
            self.current_id = header.parent_id;
            let next_h = curr_h.saturating_sub(1);

            // Populate cache
            if Self::should_cache_block(next_h) {
                let _ = self
                    .cache
                    .cache_ancestor(anchor_tip, next_h, &self.current_id)
                    .await;
                eprint!(".");
            }

            // Fetch next header
            header = self
                .provider
                .get_header(&self.current_id)
                .await?
                .ok_or_else(|| anyhow!("Missing header for {}", self.current_id))?;

            curr_h = header.height;
        }

        println!(); // Newline after dots

        // Cache final result
        let _ = self
            .cache
            .cache_ancestor(anchor_tip, curr_h, &self.current_id)
            .await;

        if curr_h != target_height {
            bail!("Failed to seek to height {target_height}: ended at {curr_h}");
        }

        Ok(header)
    }

    fn should_cache_block(height: u64) -> bool {
        height.is_multiple_of(1_000)
    }
}
