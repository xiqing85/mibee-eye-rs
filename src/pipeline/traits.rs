use crate::pipeline::error::PipelineError;
use async_trait::async_trait;

/// A frame of video data from a camera or other source.
#[derive(Debug, Clone)]
pub struct Frame {
    pub data: Vec<u8>,
    pub timestamp: std::time::Instant,
    pub is_key_frame: bool,
    pub width: u32,
    pub height: u32,
}

/// Source of video frames (camera, file, network).
#[async_trait]
pub trait FrameSource: Send + Sync {
    /// Get the next frame from this source.
    /// Returns `None` when the stream is exhausted.
    async fn next_frame(&mut self) -> Option<Frame>;
}

/// Consumer of video frames (RTSP, recording, motion detect).
#[async_trait]
pub trait FrameSink: Send + Sync {
    /// Consume a single frame.
    async fn consume(&mut self, frame: Frame) -> Result<(), PipelineError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockFrameSource {
        frames: Vec<Frame>,
        index: usize,
    }

    impl MockFrameSource {
        fn new(frames: Vec<Frame>) -> Self {
            Self { frames, index: 0 }
        }
    }

    #[async_trait]
    impl FrameSource for MockFrameSource {
        async fn next_frame(&mut self) -> Option<Frame> {
            if self.index < self.frames.len() {
                let frame = self.frames[self.index].clone();
                self.index += 1;
                Some(frame)
            } else {
                None
            }
        }
    }

    struct MockFrameSink {
        frames: Vec<Frame>,
    }

    impl MockFrameSink {
        fn new() -> Self {
            Self { frames: Vec::new() }
        }
    }

    #[async_trait]
    impl FrameSink for MockFrameSink {
        async fn consume(&mut self, frame: Frame) -> Result<(), PipelineError> {
            self.frames.push(frame);
            Ok(())
        }
    }

    fn make_frame(data: Vec<u8>, key: bool) -> Frame {
        Frame {
            data,
            timestamp: std::time::Instant::now(),
            is_key_frame: key,
            width: 1920,
            height: 1080,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_mock_source_yields_frames() {
        let frames = vec![
            make_frame(vec![0u8; 10], true),
            make_frame(vec![1u8; 10], false),
        ];
        let mut source = MockFrameSource::new(frames);

        assert_eq!(source.next_frame().await.unwrap().data, vec![0u8; 10]);
        assert_eq!(source.next_frame().await.unwrap().data, vec![1u8; 10]);
        assert!(source.next_frame().await.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_mock_sink_collects_frames() {
        let mut sink = MockFrameSink::new();
        let frame = make_frame(vec![42u8; 100], true);

        sink.consume(frame.clone()).await.unwrap();
        assert_eq!(sink.frames.len(), 1);
        assert_eq!(sink.frames[0].width, 1920);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_source_empty() {
        let mut source = MockFrameSource::new(vec![]);
        assert!(source.next_frame().await.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_sink_error_propagation() {
        struct FailingSink;

        #[async_trait]
        impl FrameSink for FailingSink {
            async fn consume(&mut self, _frame: Frame) -> Result<(), PipelineError> {
                Err(PipelineError::Pipeline("sink failure".to_string()))
            }
        }

        let mut sink = FailingSink;
        let frame = make_frame(vec![0u8; 1], true);
        let result = sink.consume(frame).await;
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().to_string(),
            "Pipeline error: sink failure"
        );
    }
}
