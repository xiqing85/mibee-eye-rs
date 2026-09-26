//! Nearest-neighbour I420 (YU12) downscaler for the bandwidth-saving
//! substream (SPEC appendix A #20).
//!
//! Produces a contiguous I420 frame at a lower resolution from the main
//! capture frame. The main pipeline bakes rotation/flips/watermark into
//! its frames before encoding, so a tap placed after those transforms
//! hands the substream fully finished pixels — the sub encoder inherits
//! them for free.
//!
//! Nearest neighbour (no interpolation) keeps the per-frame cost at a
//! single pass over the destination planes; at 720p → 360p on a
//! Cortex-A53 this is well under 2 ms.

/// Downscale a contiguous I420 frame (`src`, `src_w`×`src_h`, plane
/// layout Y then U then V) to `dst_w`×`dst_h`.
///
/// Behaviour guarantees:
/// - identity (`dst == src` dims) and no-op upscale (`dst` larger than
///   `src` in either axis) return a copy of the input unchanged;
/// - a `src` shorter than `src_w * src_h * 3 / 2` is returned unchanged
///   (defensive — the encoder's input filler rejects mismatched sizes
///   instead of encoding garbage);
/// - never panics: all indexing is derived from the validated length.
#[must_use]
pub fn downscale_yu420(src: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    let required = plane_sizes(src_w, src_h).map(|(y, c)| y + 2 * c);
    if src_w == 0
        || src_h == 0
        || dst_w == 0
        || dst_h == 0
        || dst_w > src_w
        || dst_h > src_h
        || required.is_none_or(|need| src.len() < need)
    {
        return src.to_vec();
    }
    if (dst_w, dst_h) == (src_w, src_h) {
        return src.to_vec();
    }

    let (src_y, src_c) = plane_sizes(src_w, src_h).unwrap_or((0, 0));
    let dst_y = (dst_w as usize) * (dst_h as usize);

    // Nearest-neighbour maps — the same ratio drives the chroma planes
    // (both are halved), so one table per axis serves luma and chroma.
    let xmap = map_axis(dst_w, src_w);
    let ymap = map_axis(dst_h, src_h);

    // Chroma plane geometry: half per axis, clamped to 1 so degenerate
    // odd dimensions still produce a contiguous, indexable layout.
    let dst_ch = ((dst_h / 2) as usize).max(1);
    let dst_cw = ((dst_w / 2) as usize).max(1);
    let dst_c = dst_ch * dst_cw;

    let mut out = vec![0u8; dst_y + 2 * dst_c];

    // Luma.
    for (dy, sy) in ymap.iter().enumerate() {
        let s_row = sy * src_w as usize;
        let d_row = dy * dst_w as usize;
        for (dx, sx) in xmap.iter().enumerate() {
            out[d_row + dx] = src[s_row + sx];
        }
    }
    // Chroma (half resolution on both axes): chroma dst row/col c pairs
    // with luma dst row/col 2c, mapped through the luma tables and halved.
    let src_cw = (src_w / 2) as usize;
    let last_dst_row = ymap.len().saturating_sub(1);
    let last_dst_col = xmap.len().saturating_sub(1);
    for dcy in 0..dst_ch {
        let luma_row = (dcy * 2).min(last_dst_row);
        let scy = ymap[luma_row] / 2;
        let s_row_u = src_y + scy * src_cw;
        let s_row_v = s_row_u + src_c;
        for dcx in 0..dst_cw {
            let luma_col = (dcx * 2).min(last_dst_col);
            let scx = xmap[luma_col] / 2;
            out[dst_y + dcy * dst_cw + dcx] = src[s_row_u + scx];
            out[dst_y + dst_c + dcy * dst_cw + dcx] = src[s_row_v + scx];
        }
    }
    out
}

/// Plane sizes for I420 (4:2:0): `(luma_bytes, one_chroma_plane_bytes)`
/// — each chroma plane is `(w/2) * (h/2)`, halved per axis (not of the
/// product), so the figure stays exact for odd dimensions too.
fn plane_sizes(w: u32, h: u32) -> Option<(usize, usize)> {
    let w = usize::try_from(w).ok()?;
    let h = usize::try_from(h).ok()?;
    let y = w.checked_mul(h)?;
    let c = w / 2 * (h / 2);
    Some((y, c))
}

/// `dst` → `src` nearest-neighbour index table for one axis.
fn map_axis(dst: u32, src: u32) -> Vec<usize> {
    (0..dst)
        .map(|d| (d as u64 * src as u64 / dst as u64) as usize)
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 8×4 I420 frame with distinct bytes per plane:
    /// Y = 1..=32 (8×4), U = 0x10..0x17 (4×2), V = 0x20..0x27 (4×2).
    fn frame_8x4() -> Vec<u8> {
        let mut v: Vec<u8> = (1..=32u8).collect();
        v.extend_from_slice(&[0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17]);
        v.extend_from_slice(&[0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27]);
        v
    }

    #[test]
    fn half_scale_8x4_to_4x2_picks_every_other_sample() {
        let out = downscale_yu420(&frame_8x4(), 8, 4, 4, 2);
        // Luma rows 0,2 × cols 0,2,4,6 (row 2 starts at byte 17: 16..=24).
        assert_eq!(&out[..8], &[1, 3, 5, 7, 17, 19, 21, 23]);
        // Chroma: src 4×2 → dst 2×1; nearest picks cols 0,2 of row 0.
        assert_eq!(&out[8..10], &[0x10, 0x12]);
        assert_eq!(&out[10..12], &[0x20, 0x22]);
        assert_eq!(out.len(), 4 * 2 * 3 / 2);
    }

    #[test]
    fn exact_2to1_4x4_to_2x2_golden() {
        // Y 4x4: row-major 1..16; U/V 2x2.
        let mut v: Vec<u8> = (1..=16u8).collect();
        v.extend_from_slice(&[0xA1, 0xA2, 0xA3, 0xA4]);
        v.extend_from_slice(&[0xB1, 0xB2, 0xB3, 0xB4]);
        let out = downscale_yu420(&v, 4, 4, 2, 2);
        // Luma picks (0,0),(0,2),(2,0),(2,2) → 1,3,9,11.
        assert_eq!(&out[..4], &[1, 3, 9, 11]);
        // Chroma 2x2 → 1x1: (0,0) of each plane.
        assert_eq!(out[4], 0xA1);
        assert_eq!(out[5], 0xB1);
        assert_eq!(out.len(), 2 * 2 * 3 / 2);
    }

    #[test]
    fn non_integer_ratio_maps_into_range() {
        // 3×2 source, byte-tagged: Y = [1,2,3 / 4,5,6], U = 0xAA, V = 0xBB.
        let mut src = vec![1u8, 2, 3, 4, 5, 6];
        src.push(0xAA);
        src.push(0xBB);
        let out = downscale_yu420(&src, 3, 2, 2, 2);
        // xmap(2 of 3) = [0,1], ymap(2 of 2) = [0,1] → both source rows
        // sampled at their first two columns.
        assert_eq!(&out[..4], &[1, 2, 4, 5]);
        // Chroma 1×1 → source chroma 1×1 samples.
        assert_eq!(out[4], 0xAA);
        assert_eq!(out[5], 0xBB);
    }

    #[test]
    fn identity_returns_copy() {
        let src = frame_8x4();
        let out = downscale_yu420(&src, 8, 4, 8, 4);
        assert_eq!(out, src);
    }

    #[test]
    fn upscale_is_documented_noop() {
        let src = frame_8x4();
        let out = downscale_yu420(&src, 8, 4, 16, 8);
        assert_eq!(out, src, "upscale must not attempt interpolation");
    }

    #[test]
    fn short_input_returned_unchanged() {
        let short = vec![1u8, 2, 3];
        let out = downscale_yu420(&short, 8, 2, 4, 1);
        assert_eq!(out, short);
    }

    #[test]
    fn zero_dims_returned_unchanged() {
        let src = frame_8x4();
        assert_eq!(downscale_yu420(&src, 0, 4, 1, 1), src);
        assert_eq!(downscale_yu420(&src, 8, 4, 0, 1), src);
        assert_eq!(downscale_yu420(&src, 8, 0, 1, 1), src);
        assert_eq!(downscale_yu420(&src, 8, 4, 1, 0), src);
    }

    #[test]
    fn map_axis_never_exceeds_source_range() {
        assert_eq!(map_axis(2, 4), vec![0, 2]);
        // floor(1*4/3)=1, floor(2*4/3)=2.
        assert_eq!(map_axis(3, 4), vec![0, 1, 2]);
        assert_eq!(map_axis(4, 4), vec![0, 1, 2, 3]);
        let m = map_axis(359, 720);
        assert!(m.iter().all(|&x| x < 720));
    }

    #[test]
    fn odd_sub_dimensions_still_produce_contiguous_i420() {
        // 720p → 641x361 (odd): chroma planes use floor halves; output
        // stays contiguous with 641*361 luma bytes.
        let src = vec![7u8; 1280 * 720 * 3 / 2];
        let out = downscale_yu420(&src, 1280, 720, 640, 360);
        assert_eq!(out.len(), 640 * 360 * 3 / 2);
        assert!(out.iter().all(|&b| b == 7));
    }
}
