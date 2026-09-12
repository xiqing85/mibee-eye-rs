//! エラー定義と Result 型。

use std::io;

/// V4L2 M2M 操作で発生するエラー。
#[derive(Debug)]
pub enum Error {
    /// デバイスファイルのオープンに失敗した。
    DeviceOpen { path: String, source: io::Error },
    /// ioctl 呼び出しに失敗した。
    Ioctl {
        request: &'static str,
        source: io::Error,
    },
    /// mmap に失敗した。
    Mmap { source: io::Error },
    /// poll に失敗した。
    Poll { source: io::Error },
    /// 不正なフォーマット指定。
    InvalidFormat { reason: String },
    /// 利用可能なバッファがない。
    NoAvailableBuffer,
    /// ストリーミングが開始されていない。
    NotStarted,
    /// STREAMON に失敗した。
    StreamOn { source: io::Error },
    /// STREAMOFF に失敗した。
    StreamOff { source: io::Error },
    /// 入力データがバッファ容量を超えている。
    InputTooLarge { size: usize, capacity: usize },
    /// mmap 入力クロージャが入力データを生成しなかった。
    MmapInputNotProduced,
    /// ポーリングスレッドが中断された。
    PollerAborted,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::DeviceOpen { path, source } => {
                write!(f, "デバイスのオープンに失敗しました: {path}: {source}")
            }
            Error::Ioctl { request, source } => {
                write!(f, "ioctl の呼び出しに失敗しました: {request}: {source}")
            }
            Error::Mmap { source } => {
                write!(f, "mmap に失敗しました: {source}")
            }
            Error::Poll { source } => {
                write!(f, "poll に失敗しました: {source}")
            }
            Error::InvalidFormat { reason } => {
                write!(f, "不正なフォーマットです: {reason}")
            }
            Error::NoAvailableBuffer => f.write_str("利用可能なバッファがありません"),
            Error::NotStarted => f.write_str("ストリーミングが開始されていません"),
            Error::StreamOn { source } => {
                write!(f, "STREAMON に失敗しました: {source}")
            }
            Error::StreamOff { source } => {
                write!(f, "STREAMOFF に失敗しました: {source}")
            }
            Error::InputTooLarge { size, capacity } => {
                write!(
                    f,
                    "入力データがバッファ容量を超えています: サイズ {size}, 容量 {capacity}"
                )
            }
            Error::MmapInputNotProduced => {
                f.write_str("mmap 入力クロージャが入力データを生成しませんでした")
            }
            Error::PollerAborted => f.write_str("ポーリングスレッドが中断されました"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::DeviceOpen { source, .. } => Some(source),
            Error::Ioctl { source, .. } => Some(source),
            Error::Mmap { source } => Some(source),
            Error::Poll { source } => Some(source),
            Error::StreamOn { source } => Some(source),
            Error::StreamOff { source } => Some(source),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
