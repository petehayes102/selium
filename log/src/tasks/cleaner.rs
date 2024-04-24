use crate::{config::SharedLogConfig, error::Result, segment::SharedSegmentList};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
pub struct CleanerTask {
    segments: SharedSegmentList,
    config: SharedLogConfig,
    cancellation_token: CancellationToken,
}

impl CleanerTask {
    pub fn start(config: SharedLogConfig, segments: SharedSegmentList) -> Arc<Self> {
        let cancellation_token = CancellationToken::new();

        let cleaner = Arc::new(Self {
            segments,
            config,
            cancellation_token,
        });

        tokio::spawn({
            let cleaner = cleaner.clone();
            async move {
                cleaner.run().await.unwrap();
            }
        });

        cleaner
    }

    async fn run(&self) -> Result<()> {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(self.config.cleaner_interval) => {
                    self.remove_stale_segments().await?;
                },
                _ = self.cancellation_token.cancelled() => {
                    break Ok(());
                }
            }
        }
    }

    async fn remove_stale_segments(&self) -> Result<()> {
        let mut segments = self.segments.write().await;

        let stale_segments = segments
            .find_stale_segments(self.config.retention_period)
            .await?;

        segments.remove_segments(stale_segments).await?;

        Ok(())
    }
}

impl Drop for CleanerTask {
    fn drop(&mut self) {
        self.cancellation_token.cancel();
    }
}
