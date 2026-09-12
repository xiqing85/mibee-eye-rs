//! V4L2 bindings。
//!
//! Raspberry Pi 向け V4L2 M2M (Memory-to-Memory) デバイスへのバインディング。

mod buffer;
mod converter;
mod decoder;
mod device;
mod encoder;
mod error;
mod format;
mod poller;
mod queue;
pub(crate) mod sys;

/// V4L2 M2M (Memory-to-Memory) を使った H.264 エンコード/デコード。
///
/// Raspberry Pi の `/dev/video11` (エンコーダー) と `/dev/video10` (デコーダー) を
/// 操作するための汎用的な V4L2 M2M ラッパー。WebRTC には依存しない。
pub mod v4l2_m2m {
    pub use crate::converter::{
        ConvertCallbackOutput, ConvertInput, ConvertedFrame, ConverterConfig, ImageConverter,
    };
    pub use crate::decoder::{
        DecodeHandler, DecodeInput, DecodedFrame, DecoderConfig, FnDecodeHandler, H264Decoder,
    };
    pub use crate::encoder::{
        EncodeHandler, EncodeInput, EncodedFrame, EncoderConfig, FnEncodeHandler, H264Encoder,
        H264Level, H264Profile,
    };
    pub use crate::error::{Error, Result};
    pub use crate::format::{Memory, PixelFormat, Resolution};
}
