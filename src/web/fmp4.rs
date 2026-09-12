//! Fragmented MP4 (fMP4) muxer for MSE (Media Source Extensions).
//!
//! Converts H.264 NAL units into fMP4 init segment + media fragments
//! for browser playback via MediaSource API over WebSocket.

fn u32be(v: u32, buf: &mut Vec<u8>) {
    buf.extend_from_slice(&v.to_be_bytes());
}
fn u16be(v: u16, buf: &mut Vec<u8>) {
    buf.extend_from_slice(&v.to_be_bytes());
}
fn u24be(v: u32, buf: &mut Vec<u8>) {
    buf.push((v >> 16) as u8);
    buf.push((v >> 8) as u8);
    buf.push(v as u8);
}
fn u64be(v: u64, buf: &mut Vec<u8>) {
    buf.extend_from_slice(&v.to_be_bytes());
}

/// Build a box: [size(4)] [type(4)] [payload].
fn box_(t: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut o = Vec::with_capacity(8 + payload.len());
    u32be(8 + payload.len() as u32, &mut o);
    o.extend_from_slice(t);
    o.extend_from_slice(payload);
    o
}

/// Build a FullBox: [size(4)] [type(4)] [version(1)] [flags(3)] [payload].
fn fullbox(t: &[u8; 4], ver: u8, flags: u32, payload: &[u8]) -> Vec<u8> {
    let mut o = Vec::with_capacity(12 + payload.len());
    u32be(12 + payload.len() as u32, &mut o);
    o.extend_from_slice(t);
    o.push(ver);
    u24be(flags, &mut o);
    o.extend_from_slice(payload);
    o
}

const MATRIX: [u8; 36] = [
    0x00, 0x01, 0x00, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    0x00, 0x01, 0x00, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    0x40, 0x00, 0x00, 0x00,
];

/// Build the fMP4 initialization segment (ftyp + moov).
#[must_use]
pub fn build_init_segment(sps: &[u8], pps: &[u8], width: u32, height: u32) -> Vec<u8> {
    // ftyp
    let mut ftyp = Vec::new();
    ftyp.extend_from_slice(b"iso5");
    u32be(512, &mut ftyp);
    ftyp.extend_from_slice(b"iso5avc1mp42");
    let ftyp_box = box_(b"ftyp", &ftyp);

    // avcC
    let mut avcc = vec![1]; // configurationVersion
    avcc.push(sps[1]); // profile
    avcc.push(sps[2]); // compatibility
    avcc.push(sps[3]); // level
    avcc.push(0xFF); // lengthSizeMinusOne=3 | reserved
    avcc.push(0xE1); // numSPS=1 | reserved
    u16be(sps.len() as u16, &mut avcc);
    avcc.extend_from_slice(sps);
    avcc.push(1); // numPPS
    u16be(pps.len() as u16, &mut avcc);
    avcc.extend_from_slice(pps);
    let avcc_box = box_(b"avcC", &avcc);

    // avc1 sample entry
    let mut avc1 = Vec::new();
    avc1.extend_from_slice(&[0u8; 6]); // reserved
    u16be(1, &mut avc1); // data_reference_index
    avc1.extend_from_slice(&[0u8; 16]); // pre_defined + reserved
    u16be(width as u16, &mut avc1);
    u16be(height as u16, &mut avc1);
    u32be(0x00480000, &mut avc1); // horizresolution 72dpi
    u32be(0x00480000, &mut avc1); // vertresolution
    u32be(0, &mut avc1); // reserved
    u16be(1, &mut avc1); // frame_count
    avc1.extend_from_slice(&[0u8; 32]); // compressorname
    u16be(0x0018, &mut avc1); // depth=24
    avc1.extend_from_slice(&[0xFF, 0xFF]); // pre_defined
    avc1.extend_from_slice(&avcc_box);
    let avc1_box = box_(b"avc1", &avc1);

    // stsd
    let mut stsd = Vec::new();
    u32be(1, &mut stsd);
    stsd.extend_from_slice(&avc1_box);
    let stsd_box = fullbox(b"stsd", 0, 0, &stsd);

    // Empty sample table boxes
    let stts_box = fullbox(b"stts", 0, 0, &[0, 0, 0, 0]);
    let stsc_box = fullbox(b"stsc", 0, 0, &[0, 0, 0, 0]);
    let stsz_box = fullbox(b"stsz", 0, 0, &[0; 8]);
    let stco_box = fullbox(b"stco", 0, 0, &[0, 0, 0, 0]);

    // stbl
    let mut stbl = Vec::new();
    stbl.extend_from_slice(&stsd_box);
    stbl.extend_from_slice(&stts_box);
    stbl.extend_from_slice(&stsc_box);
    stbl.extend_from_slice(&stsz_box);
    stbl.extend_from_slice(&stco_box);
    let stbl_box = box_(b"stbl", &stbl);

    // vmhd
    let vmhd_box = fullbox(b"vmhd", 0, 1, &[0u8; 8]);

    // dinf → dref
    let dref_entry = fullbox(b"url ", 0, 1, &[]);
    let mut dref = Vec::new();
    u32be(1, &mut dref);
    dref.extend_from_slice(&dref_entry);
    let dinf_box = box_(b"dinf", &box_(b"dref", &dref));

    // minf
    let mut minf = Vec::new();
    minf.extend_from_slice(&vmhd_box);
    minf.extend_from_slice(&dinf_box);
    minf.extend_from_slice(&stbl_box);
    let minf_box = box_(b"minf", &minf);

    // mdhd
    let mut mdhd = Vec::new();
    u32be(0, &mut mdhd);
    u32be(0, &mut mdhd);
    u32be(90000, &mut mdhd); // timescale 90kHz
    u32be(0, &mut mdhd);
    u16be(0x55C4, &mut mdhd);
    u16be(0, &mut mdhd);
    let mdhd_box = fullbox(b"mdhd", 0, 0, &mdhd);

    // hdlr
    let mut hdlr = Vec::new();
    u32be(0, &mut hdlr);
    hdlr.extend_from_slice(b"vide");
    hdlr.extend_from_slice(&[0u8; 12]);
    hdlr.extend_from_slice(b"VideoHandler\0");
    let hdlr_box = fullbox(b"hdlr", 0, 0, &hdlr);

    // mdia
    let mut mdia = Vec::new();
    mdia.extend_from_slice(&mdhd_box);
    mdia.extend_from_slice(&hdlr_box);
    mdia.extend_from_slice(&minf_box);
    let mdia_box = box_(b"mdia", &mdia);

    // tkhd
    let mut tkhd = Vec::new();
    u32be(0, &mut tkhd);
    u32be(0, &mut tkhd);
    u32be(1, &mut tkhd); // track_id
    u32be(0, &mut tkhd);
    u32be(0, &mut tkhd);
    tkhd.extend_from_slice(&[0u8; 8]);
    u16be(0, &mut tkhd);
    u16be(0, &mut tkhd);
    u16be(0, &mut tkhd);
    tkhd.extend_from_slice(&[0u8; 2]);
    tkhd.extend_from_slice(&MATRIX);
    u32be(width << 16, &mut tkhd);
    u32be(height << 16, &mut tkhd);
    let tkhd_box = fullbox(b"tkhd", 0, 7, &tkhd);

    // trak
    let mut trak = Vec::new();
    trak.extend_from_slice(&tkhd_box);
    trak.extend_from_slice(&mdia_box);
    let trak_box = box_(b"trak", &trak);

    // mvhd
    let mut mvhd = Vec::new();
    u32be(0, &mut mvhd);
    u32be(0, &mut mvhd);
    u32be(90000, &mut mvhd);
    u32be(0, &mut mvhd);
    u32be(0x00010000, &mut mvhd); // rate=1.0
    u16be(0x0100, &mut mvhd); // volume=1.0
    mvhd.extend_from_slice(&[0u8; 10]);
    mvhd.extend_from_slice(&MATRIX);
    mvhd.extend_from_slice(&[0u8; 24]);
    u32be(2, &mut mvhd); // next_track_id
    let mvhd_box = fullbox(b"mvhd", 0, 0, &mvhd);
    // trex (track extends — default values for fragments)
    let mut trex = Vec::new();
    u32be(1, &mut trex); // track_id
    u32be(1, &mut trex); // default_sample_description_index
    u32be(0, &mut trex); // default_sample_duration
    u32be(0, &mut trex); // default_sample_size
    u32be(0x02000000, &mut trex); // default_sample_flags (independent)
    let trex_box = fullbox(b"trex", 0, 0, &trex);

    // mvex (movie extends — signals fragmented MP4)
    let mvex_box = box_(b"mvex", &trex_box);

    // moov
    let mut moov = Vec::new();
    moov.extend_from_slice(&mvhd_box);
    moov.extend_from_slice(&trak_box);
    moov.extend_from_slice(&mvex_box);
    let moov_box = box_(b"moov", &moov);

    let mut init = Vec::with_capacity(ftyp_box.len() + moov_box.len());
    init.extend_from_slice(&ftyp_box);
    init.extend_from_slice(&moov_box);
    init
}

/// Build a media fragment (moof + mdat) for a single access unit.
#[must_use]
pub fn build_media_segment(
    nalus: &[Vec<u8>],
    sequence: u32,
    timestamp: u64,
    duration: u32,
    is_key: bool,
) -> Vec<u8> {
    // AVCC sample data (4-byte length prefix per NALU, skip AUD type 9)
    let mut mdat_data = Vec::with_capacity(1024);
    for nalu in nalus {
        if !nalu.is_empty() && nalu[0] & 0x1F == 9 {
            continue;
        }
        u32be(nalu.len() as u32, &mut mdat_data);
        mdat_data.extend_from_slice(nalu);
    }
    let sample_size = mdat_data.len() as u32;

    // mfhd
    let mut mfhd = Vec::new();
    u32be(sequence, &mut mfhd);
    let mfhd_box = fullbox(b"mfhd", 0, 0, &mfhd);

    // tfhd (default-base-is-moof)
    let mut tfhd = Vec::new();
    u32be(1, &mut tfhd);
    let tfhd_box = fullbox(b"tfhd", 0, 0x020000, &tfhd);

    // tfdt (version 1 = 64-bit base_media_decode_time)
    let mut tfdt = Vec::new();
    u64be(timestamp, &mut tfdt);
    let tfdt_box = fullbox(b"tfdt", 1, 0, &tfdt);

    // Compute moof total size for data_offset.
    // trun payload: count(4)+offset(4)+dur(4)+size(4)+flags(4) = 20
    // trun fullbox: 12 + 20 = 32
    // traf: 8 + tfhd(16) + tfdt(20) + trun(32) = 76
    // moof: 8 + mfhd(16) + traf(76) = 100
    let data_offset: u32 = 100 + 8; // moof + mdat header

    let mut trun = Vec::new();
    u32be(1, &mut trun); // sample_count
    u32be(data_offset, &mut trun); // data_offset
    u32be(duration, &mut trun); // sample_duration
    u32be(sample_size, &mut trun); // sample_size
    u32be(if is_key { 0x02000000 } else { 0x01010000 }, &mut trun); // sample_flags
    let trun_box = fullbox(b"trun", 0, 0x000701, &trun);

    // traf
    let mut traf = Vec::new();
    traf.extend_from_slice(&tfhd_box);
    traf.extend_from_slice(&tfdt_box);
    traf.extend_from_slice(&trun_box);
    let traf_box = box_(b"traf", &traf);

    // moof
    let mut moof = Vec::new();
    moof.extend_from_slice(&mfhd_box);
    moof.extend_from_slice(&traf_box);
    let moof_box = box_(b"moof", &moof);

    let mdat_box = box_(b"mdat", &mdat_data);

    let mut seg = Vec::with_capacity(moof_box.len() + mdat_box.len());
    seg.extend_from_slice(&moof_box);
    seg.extend_from_slice(&mdat_box);
    seg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_init_segment_structure() {
        let sps = vec![0x67, 0x42, 0x00, 0x1e, 0x00];
        let pps = vec![0x68, 0xCE, 0x38, 0x80];
        let init = build_init_segment(&sps, &pps, 1280, 720);
        assert!(init.windows(4).any(|w| w == b"ftyp"));
        assert!(init.windows(4).any(|w| w == b"moov"));
        assert!(init.windows(4).any(|w| w == b"avcC"));
    }

    #[test]
    fn test_media_segment_structure() {
        let nalus = vec![vec![0x65, 0x88, 0x84, 0x00]];
        let seg = build_media_segment(&nalus, 1, 0, 6000, true);
        assert!(seg.windows(4).any(|w| w == b"moof"));
        assert!(seg.windows(4).any(|w| w == b"mdat"));
    }
}
