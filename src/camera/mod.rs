pub mod encoder_probe;
pub mod params;
#[cfg(feature = "software-encoder")]
pub mod software;
pub mod source;
pub mod v4l2_capture;

#[cfg(feature = "v4l2-encoder")]
pub mod v4l2;

/// Consumes a pending on-demand IDR request (`DeviceControl
/// IFrameCmd Send`, GB/T 28181 §9.3.2): atomically reads-and-clears the
/// shared flag the encoder threads check before encoding a frame.
pub fn consume_idr_request(flag: &std::sync::atomic::AtomicBool) -> bool {
    flag.swap(false, std::sync::atomic::Ordering::Relaxed)
}

/// Raises the on-demand IDR request (the GB control handler side of
/// [`consume_idr_request`]).
pub fn raise_idr_request(flag: &std::sync::atomic::AtomicBool) {
    flag.store(true, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(test)]
mod idr_tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn idr_request_is_one_shot() {
        let flag = AtomicBool::new(false);
        assert!(!consume_idr_request(&flag));
        raise_idr_request(&flag);
        assert!(consume_idr_request(&flag));
        assert!(!consume_idr_request(&flag), "one-shot: cleared on read");
    }
}
