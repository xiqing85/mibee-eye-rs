//! EEPROM calibration decode + per-frame temperature math (Ta/To).
//!
//! Faithful implementation of MLX90640 datasheet Rev 11 §11, per
//! `docs/mlx90640-temperature-math.md` (formulas, bit fields, worked example
//! and errata all live there). Decode is validated against the datasheet's
//! own worked example (Table 11/12 → To(12,16) ≈ 80.1–80.4 °C) and the real
//! device fixture (Ta ≈ 34 °C on the bench).

use super::mlx90640::{AuxReadings, HEIGHT, PIXELS, WIDTH};

/// N-bit two's-complement conversion: `if v > 2^(n-1)-1 { v - 2^n } else { v }`.
const fn signed(raw: i32, bits: u32) -> i32 {
    if raw > (1 << (bits - 1)) - 1 {
        raw - (1 << bits)
    } else {
        raw
    }
}

#[inline]
fn nibble(word: u16, shift: u32) -> i32 {
    signed(((word >> shift) & 0xF) as i32, 4)
}

/// Decoded EEPROM calibration parameters (datasheet §11.1).
#[derive(Debug, Clone)]
pub struct Eeprom {
    // 11.1.1 VDD params
    pub k_vdd: f64,
    pub vdd_25: f64,
    // 11.1.2 PTAT params
    pub kv_ptat: f64,
    pub kt_ptat: f64,
    pub ptat_25: f64,
    pub alpha_ptat: f64,
    // 11.1.3 offsets
    pub offset_average: f64,
    pub occ_row: [f64; HEIGHT],
    pub occ_col: [f64; WIDTH],
    pub scale_occ_row: u32,
    pub scale_occ_col: u32,
    pub scale_occ_rem: u32,
    pub offset: [f64; PIXELS],
    // 11.1.4 alpha
    pub alpha_ref: f64,
    pub acc_row: [f64; HEIGHT],
    pub acc_col: [f64; WIDTH],
    pub scale_acc_row: u32,
    pub scale_acc_col: u32,
    pub scale_acc_rem: u32,
    pub alpha_pixel: [f64; PIXELS],
    pub alpha_scale: u32,
    // 11.1.5/11.1.6 Kv / Kta per pixel
    pub k_v: [f64; PIXELS],
    pub k_ta: [f64; PIXELS],
    // 11.1.7 gain
    pub gain: f64,
    // 11.1.8 KsTa
    pub ks_ta: f64,
    // 11.1.9 corner temperatures
    pub ct3: f64,
    pub ct4: f64,
    // 11.1.10 KsTo per range
    pub ks_to: [f64; 4], // range1..range4
    /// Exposed for diagnostics (value already folded into `ks_to`).
    #[allow(dead_code)]
    pub ks_to_scale: u32,
    // 11.1.12 CP alpha
    pub alpha_cp_sp0: f64,
    pub alpha_cp_sp1: f64,
    // 11.1.13 CP offsets
    pub off_cp_sp0: f64,
    pub off_cp_sp1: f64,
    // 11.1.14/15 CP Kv/Kta
    pub k_vcp: f64,
    pub k_tacp: f64,
    // 11.1.16 TGC
    pub tgc: f64,
    // 11.1.17 resolution at calibration
    pub resolution_ee: u32,
    // per-pixel validity (datasheet NOTE 1/2: 0x0000 word = dead pixel, bit0 = outlier);
    // the renderer/served API consumes this when M5 lands.
    #[allow(dead_code)]
    pub dead_pixel: [bool; PIXELS],
}

/// Per-frame common quantities (§11.2.2.1–11.2.2.4).
#[derive(Debug, Clone, Copy)]
pub struct CommonParams {
    /// Exposed for diagnostics/logs (folded into `vdd` already).
    #[allow(dead_code)]
    pub res_corr: f64,
    pub vdd: f64,
    pub ta: f64,
    pub k_gain: f64,
}

impl Eeprom {
    /// Decode the full 832-word EEPROM block (0x2400–0x273F, row-major words).
    pub fn from_words(ee: &[u16]) -> Eeprom {
        assert_eq!(ee.len(), 832, "EEPROM block must be 832 words");
        let e = |addr: u16| ee[(addr - 0x2400) as usize];

        // 11.1.1 VDD
        let k_vdd = signed(((e(0x2433) & 0xFF00) >> 8) as i32, 8) as f64 * 32.0;
        let vdd_25 = ((e(0x2433) & 0x00FF) as i32 - 256) as f64 * 32.0 - 8192.0;

        // 11.1.2 PTAT
        let kv_ptat = signed(((e(0x2432) & 0xFC00) >> 10) as i32, 6) as f64 / 4096.0;
        let kt_ptat = signed((e(0x2432) & 0x03FF) as i32, 10) as f64 / 8.0;
        let ptat_25 = signed(e(0x2431) as i32, 16) as f64;
        let alpha_ptat = ((e(0x2410) & 0xF000) >> 12) as f64 / 4.0 + 8.0;

        // 11.1.3 offsets — row/column nibble tables
        let offset_average = signed(e(0x2411) as i32, 16) as f64;
        let scale_occ_row = ((e(0x2410) & 0x0F00) >> 8) as u32;
        let scale_occ_col = ((e(0x2410) & 0x00F0) >> 4) as u32;
        let scale_occ_rem = (e(0x2410) & 0x000F) as u32;
        let mut occ_row = [0.0; HEIGHT];
        for (i, slot) in occ_row.iter_mut().enumerate() {
            let word = e(0x2412 + ((i / 4) as u16));
            *slot = nibble(word, 4 * (i % 4) as u32) as f64;
        }
        let mut occ_col = [0.0; WIDTH];
        for (j, slot) in occ_col.iter_mut().enumerate() {
            let word = e(0x2418 + ((j / 4) as u16));
            *slot = nibble(word, 4 * (j % 4) as u32) as f64;
        }

        // 11.1.4 alpha tables
        let alpha_ref = signed(e(0x2421) as i32, 16) as f64;
        let scale_acc_row = ((e(0x2420) & 0x0F00) >> 8) as u32;
        let scale_acc_col = ((e(0x2420) & 0x00F0) >> 4) as u32;
        let scale_acc_rem = (e(0x2420) & 0x000F) as u32;
        let alpha_scale = ((e(0x2420) & 0xF000) >> 12) as u32 + 30;
        let mut acc_row = [0.0; HEIGHT];
        for (i, slot) in acc_row.iter_mut().enumerate() {
            let word = e(0x2422 + ((i / 4) as u16));
            *slot = nibble(word, 4 * (i % 4) as u32) as f64;
        }
        let mut acc_col = [0.0; WIDTH];
        for (j, slot) in acc_col.iter_mut().enumerate() {
            let word = e(0x2428 + ((j / 4) as u16));
            *slot = nibble(word, 4 * (j % 4) as u32) as f64;
        }

        // 11.1.5 Kv scale + 11.1.6 Kta scales
        let k_v_scale = ((e(0x2438) & 0x0F00) >> 8) as u32;
        let kta_scale_1 = ((e(0x2438) & 0x00F0) >> 4) as u32 + 8;
        let kta_scale_2 = (e(0x2438) & 0x000F) as u32;

        // Per-pixel tables (one word per pixel, 0x2440 + (row-1)*32 + (col-1)).
        let mut offset = [0.0; PIXELS];
        let mut alpha_pixel = [0.0; PIXELS];
        let mut k_v = [0.0; PIXELS];
        let mut k_ta = [0.0; PIXELS];
        let mut dead_pixel = [false; PIXELS];
        let kv_nibbles = [
            ((e(0x2434) & 0xF000) >> 12) as i32,
            ((e(0x2434) & 0x0F00) >> 8) as i32,
            ((e(0x2434) & 0x00F0) >> 4) as i32,
            (e(0x2434) & 0x000F) as i32,
        ];
        let kta_bytes = [
            ((e(0x2436) & 0xFF00) >> 8) as i32,
            (e(0x2436) & 0x00FF) as i32,
            ((e(0x2437) & 0xFF00) >> 8) as i32,
            (e(0x2437) & 0x00FF) as i32,
        ];
        for p in 0..PIXELS {
            let row = p / WIDTH; // 0-based
            let col = p % WIDTH;
            let word = e(0x2440 + p as u16);
            dead_pixel[p] = word == 0x0000 || (word & 0x0001) != 0;
            offset[p] = signed((word >> 10) as i32, 6) as f64;
            alpha_pixel[p] = signed(((word & 0x03F0) >> 4) as i32, 6) as f64;
            let kta_pix = signed(((word & 0x000E) >> 1) as i32, 3) as f64;

            // Kv nibble by (row odd/even, col odd/even) — datasheet numbering is
            // 1-based, so parity comes from row+1/col+1: RO×CO, RE×CO, RO×CE, RE×CE.
            let (r1, c1) = ((row + 1) % 2, (col + 1) % 2);
            let kv = match (r1, c1) {
                (1, 1) => signed(kv_nibbles[0], 4),
                (0, 1) => signed(kv_nibbles[1], 4),
                (1, 0) => signed(kv_nibbles[2], 4),
                _ => signed(kv_nibbles[3], 4),
            } as f64;
            k_v[p] = kv / (1u64 << k_v_scale) as f64;

            // Kta byte by the same parity classes.
            let kta_rc = match (r1, c1) {
                (1, 1) => signed(kta_bytes[0], 8),
                (0, 1) => signed(kta_bytes[1], 8),
                (1, 0) => signed(kta_bytes[2], 8),
                _ => signed(kta_bytes[3], 8),
            } as f64;
            k_ta[p] =
                (kta_rc + kta_pix * (1u64 << kta_scale_2) as f64) / (1u64 << kta_scale_1) as f64;
        }

        // 11.1.7 GAIN, 11.1.8 KsTa
        let gain = signed(e(0x2430) as i32, 16) as f64;
        let ks_ta = signed(((e(0x243C) & 0xFF00) >> 8) as i32, 8) as f64 / 8192.0;

        // 11.1.9 corner temperatures
        let step = ((e(0x243F) & 0x3000) >> 12) as f64 * 10.0;
        let ct3 = ((e(0x243F) & 0x00F0) >> 4) as f64 * step;
        let ct4 = ((e(0x243F) & 0x0F00) >> 8) as f64 * step + ct3;

        // 11.1.10 KsTo per range
        let ks_to_scale = (e(0x243F) & 0x000F) as u32 + 8;
        let d = 1u64 << ks_to_scale;
        let ks_to = [
            signed((e(0x243D) & 0x00FF) as i32, 8) as f64 / d as f64,
            signed(((e(0x243D) & 0xFF00) >> 8) as i32, 8) as f64 / d as f64,
            signed((e(0x243E) & 0x00FF) as i32, 8) as f64 / d as f64,
            signed(((e(0x243E) & 0xFF00) >> 8) as i32, 8) as f64 / d as f64,
        ];

        // 11.1.12 CP alpha
        let alpha_scale_cp = ((e(0x2420) & 0xF000) >> 12) as u32 + 27;
        let alpha_cp_sp0 = (e(0x2439) & 0x03FF) as f64 / (1u64 << alpha_scale_cp) as f64;
        let cp_ratio = signed(((e(0x2439) & 0xFC00) >> 10) as i32, 6) as f64 / 128.0;
        let alpha_cp_sp1 = alpha_cp_sp0 * (1.0 + cp_ratio);

        // 11.1.13 CP offsets
        let off_cp_sp0 = signed((e(0x243A) & 0x03FF) as i32, 10) as f64;
        let off_cp_delta = signed(((e(0x243A) & 0xFC00) >> 10) as i32, 6) as f64;
        let off_cp_sp1 = off_cp_sp0 + off_cp_delta;

        // 11.1.14/15 CP Kv/Kta (same scales as pixels)
        let k_vcp =
            signed(((e(0x243B) & 0xFF00) >> 8) as i32, 8) as f64 / (1u64 << k_v_scale) as f64;
        let k_tacp = signed((e(0x243B) & 0x00FF) as i32, 8) as f64 / (1u64 << kta_scale_1) as f64;

        // 11.1.16 TGC
        let tgc = signed((e(0x243C) & 0x00FF) as i32, 8) as f64 / 32.0;

        // 11.1.17 calibration resolution (bits [13:12])
        let resolution_ee = ((e(0x2438) & 0x3000) >> 12) as u32;

        Eeprom {
            k_vdd,
            vdd_25,
            kv_ptat,
            kt_ptat,
            ptat_25,
            alpha_ptat,
            offset_average,
            occ_row,
            occ_col,
            scale_occ_row,
            scale_occ_col,
            scale_occ_rem,
            offset,
            alpha_ref,
            acc_row,
            acc_col,
            scale_acc_row,
            scale_acc_col,
            scale_acc_rem,
            alpha_pixel,
            alpha_scale,
            k_v,
            k_ta,
            gain,
            ks_ta,
            ct3,
            ct4,
            ks_to,
            ks_to_scale,
            alpha_cp_sp0,
            alpha_cp_sp1,
            off_cp_sp0,
            off_cp_sp1,
            k_vcp,
            k_tacp,
            tgc,
            resolution_ee,
            dead_pixel,
        }
    }

    /// §11.2.2.1–11.2.2.4: resolution correction, Vdd, Ta, K_gain.
    pub fn compute_common(&self, aux: &AuxReadings) -> CommonParams {
        let resolution_reg = (aux.control & 0x0C00) >> 10;
        let res_corr = 2f64.powi(self.resolution_ee as i32 - resolution_reg as i32);

        let vdd_ram = f64::from(aux.vdd);
        let vdd = (res_corr * vdd_ram - self.vdd_25) / self.k_vdd + 3.3;
        let dv = (vdd_ram - self.vdd_25) / self.k_vdd;

        let ptat = f64::from(aux.ptat);
        let vbe = f64::from(aux.vbe);
        let ptat_art = (ptat / (ptat * self.alpha_ptat + vbe)) * 262_144.0;
        let ta = ((ptat_art / (1.0 + self.kv_ptat * dv)) - self.ptat_25) / self.kt_ptat + 25.0;

        let k_gain = self.gain / f64::from(aux.gain);

        CommonParams {
            res_corr,
            vdd,
            ta,
            k_gain,
        }
    }

    /// Chess-mode subpage selector for a 1-based pixel number (§11.2.2.7):
    /// 0 → CP_SP0 / α_CP_SP0, 1 → CP_SP1 / α_CP_SP1.
    fn pattern_chess(pn: usize) -> u32 {
        let p = pn as i32 - 1;
        let row_par = p / 32 - (p / 32) / 2 * 2;
        let col_par = p - (p / 2) * 2;
        (row_par ^ col_par) as u32
    }

    /// §11.2.2.5–11.2.2.9: full per-pixel To array (°C).
    ///
    /// * `emissivity` — user-supplied ε (1.0 for the datasheet example).
    /// * `tr` — reflected ambient temperature (°C); `None` → Ta − 8 per the
    ///   datasheet's fallback.
    pub fn compute_to(
        &self,
        frame: &[i16; PIXELS],
        aux: &AuxReadings,
        emissivity: f64,
        tr: Option<f64>,
    ) -> ([f32; PIXELS], CommonParams) {
        let common = self.compute_common(aux);
        let tr = tr.unwrap_or(common.ta - 8.0);

        // CP path (§11.2.2.6) — same for every pixel this frame.
        let ta_fac = 1.0 + self.k_tacp * (common.ta - 25.0);
        let vdd_fac = 1.0 + self.k_vcp * (common.vdd - 3.3);
        let cp_sp0 = f64::from(aux.cp_sp0) * common.k_gain - self.off_cp_sp0 * ta_fac * vdd_fac;
        let cp_sp1 = f64::from(aux.cp_sp1) * common.k_gain - self.off_cp_sp1 * ta_fac * vdd_fac;

        let ta_k4 = (common.ta + 273.15).powi(4);
        let tr_k4 = (tr + 273.15).powi(4);
        let ta_r = tr_k4 - (tr_k4 - ta_k4) / emissivity;

        let mut out = [0f32; PIXELS];
        for p in 0..PIXELS {
            let pn = p + 1;
            let pattern = Self::pattern_chess(pn);

            // 11.2.2.5 gain → offset/Vdd/Ta → emissivity
            let pix_gain = f64::from(frame[p]) * common.k_gain;
            let os_ref = self.offset_average
                + self.occ_row[p / WIDTH] * (1u64 << self.scale_occ_row) as f64
                + self.occ_col[p % WIDTH] * (1u64 << self.scale_occ_col) as f64
                + self.offset[p] * (1u64 << self.scale_occ_rem) as f64;
            let pix_os = pix_gain
                - os_ref
                    * (1.0 + self.k_ta[p] * (common.ta - 25.0))
                    * (1.0 + self.k_v[p] * (common.vdd - 3.3));
            let v_ir = pix_os / emissivity;

            // 11.2.2.7 gradient compensation via CP
            let cp = if pattern == 0 { cp_sp0 } else { cp_sp1 };
            let v_ir_comp = v_ir - self.tgc * cp;

            // 11.2.2.8 α_comp
            let a = (self.alpha_ref
                + self.acc_row[p / WIDTH] * (1u64 << self.scale_acc_row) as f64
                + self.acc_col[p % WIDTH] * (1u64 << self.scale_acc_col) as f64
                + self.alpha_pixel[p] * (1u64 << self.scale_acc_rem) as f64)
                / (1u64 << self.alpha_scale) as f64;
            let a_cp = if pattern == 0 {
                self.alpha_cp_sp0
            } else {
                self.alpha_cp_sp1
            };
            let alpha_comp = (a - self.tgc * a_cp) * (1.0 + self.ks_ta * (common.ta - 25.0));

            out[p] = self.to_from_ir(v_ir_comp, alpha_comp, &common, ta_r, 1, emissivity) as f32;
        }
        (out, common)
    }

    /// §11.2.2.9 + §11.2.2.9.1: To from compensated IR, with the extended-range
    /// iteration (recompute in the range the previous estimate lands in).
    fn to_from_ir(
        &self,
        v_ir: f64,
        alpha_comp: f64,
        _common: &CommonParams,
        ta_r: f64,
        range_hint: usize,
        _emissivity: f64,
    ) -> f64 {
        // Basic range = 2 (0 °C..CT3). range_hint lets the extended-range pass
        // recompute directly with the target range's KsTo/α correction.
        let mut range = range_hint.clamp(1, 4);
        let mut to = 0.0;
        for _ in 0..3 {
            let ks_to = self.ks_to[range - 1];
            // 11.1.11 α corrections normalize sensitivity back to range 2.
            let alpha_eff = alpha_comp / self.alpha_corr(range);
            let sx = ks_to * (alpha_eff.powi(3) * v_ir + alpha_eff.powi(4) * ta_r).powf(0.25);
            let denom = alpha_eff * (1.0 - ks_to * 273.15) + sx;
            to = (v_ir / denom + ta_r).powf(0.25) - 273.15;
            let next = self.classify(to);
            if next == range {
                break;
            }
            range = next;
        }
        to
    }

    /// 1-based range index for a To estimate (§11.2.2.9.1 boundaries).
    fn classify(&self, to: f64) -> usize {
        if to < 0.0 {
            1
        } else if to < self.ct3 {
            2
        } else if to < self.ct4 {
            3
        } else {
            4
        }
    }

    /// 11.1.11 α correction factors per 1-based range.
    fn alpha_corr(&self, range: usize) -> f64 {
        match range {
            1 => 1.0 / (1.0 + self.ks_to[0] * 40.0),
            2 => 1.0,
            3 => 1.0 + self.ks_to[1] * self.ct3,
            _ => (1.0 + self.ks_to[1] * self.ct3) * (1.0 + self.ks_to[2] * (self.ct4 - self.ct3)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rebuild the datasheet worked-example EEPROM (Table 12) plus the
    /// pixel (12,16) word (offset=2, α=10, Kt4=0 from the §11.2 chain).
    fn datasheet_ee() -> Vec<u16> {
        let mut ee = vec![0u16; 832];
        let mut set = |addr: u16, v: u16| ee[(addr - 0x2400) as usize] = v;
        set(0x2410, 0x4210);
        set(0x2411, 0xFFBB);
        set(0x2412, 0x0202);
        set(0x2413, 0xF202);
        set(0x2414, 0xF2F2);
        set(0x2415, 0xE2E2);
        set(0x2416, 0xD1E1);
        set(0x2417, 0xB1D1);
        set(0x2418, 0xF10F);
        set(0x2419, 0xF00F);
        set(0x241A, 0xE0EF);
        set(0x241B, 0xE0EF);
        set(0x241C, 0xE1E1);
        set(0x241D, 0xF3F2);
        set(0x241E, 0xF404);
        set(0x241F, 0xE504);
        set(0x2420, 0x79A6);
        set(0x2421, 0x2F44);
        set(0x2422, 0xFFDD);
        set(0x2423, 0x2210);
        set(0x2424, 0x3333);
        set(0x2425, 0x2233);
        set(0x2426, 0xEF01);
        set(0x2427, 0x9ACC);
        set(0x2428, 0xEEDC);
        set(0x2429, 0x10FF);
        set(0x242A, 0x2221);
        set(0x242B, 0x3333);
        set(0x242C, 0x2333);
        set(0x242D, 0x0112);
        set(0x242E, 0xEEFF);
        set(0x242F, 0xBBDD);
        set(0x2430, 0x18EF);
        set(0x2431, 0x2FF1);
        set(0x2432, 0x5952);
        set(0x2433, 0x9D68);
        set(0x2434, 0x5454);
        set(0x2435, 0x0994);
        set(0x2436, 0x6956);
        set(0x2437, 0x5354);
        set(0x2438, 0x2363);
        set(0x2439, 0xE446);
        set(0x243A, 0xFBB5);
        set(0x243B, 0x044B);
        set(0x243C, 0xF020);
        set(0x243D, 0x9797);
        set(0x243E, 0x9797);
        set(0x243F, 0x2889);
        // pixel (12,16): pn = 368, word @0x25AF = offset(2)<<10 | α(10)<<4 | 0<<1
        set(0x2440 + 367, (2 << 10) | (10 << 4));
        ee
    }

    /// Table 11 measurement inputs.
    fn table11_aux() -> AuxReadings {
        AuxReadings {
            vbe: 19442,
            cp_sp0: -54,
            cp_sp1: -56,
            gain: 6273,
            ptat: 1711,
            vdd: -13115,
            control: 0x0901, // resolution bits [11:10] = 2
            status: 0,
        }
    }

    fn close(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() < tol
    }

    #[test]
    fn decode_globals_match_table12() {
        let ee = Eeprom::from_words(&datasheet_ee());
        assert_eq!(ee.k_vdd, -3168.0);
        assert_eq!(ee.vdd_25, -13056.0);
        assert!(close(ee.kv_ptat, 0.005371094, 1e-9));
        assert_eq!(ee.kt_ptat, 42.25);
        assert_eq!(ee.ptat_25, 12273.0);
        assert_eq!(ee.alpha_ptat, 9.0);
        assert_eq!(ee.offset_average, -69.0);
        assert_eq!(ee.occ_row[11], -1.0); // row 12
        assert_eq!(ee.occ_col[15], -2.0); // column 16
        assert_eq!(ee.alpha_ref, 12100.0);
        assert_eq!(ee.acc_row[11], 3.0);
        assert_eq!(ee.acc_col[15], 3.0);
        assert_eq!(ee.alpha_scale, 37);
        assert_eq!(ee.gain, 6383.0);
        assert!(close(ee.ks_ta, -0.001953125, 1e-12));
        assert_eq!(ee.ct3, 160.0);
        assert_eq!(ee.ct4, 320.0);
        assert!(close(ee.ks_to[0], -0.00080108642578125, 1e-15));
        assert!(close(ee.alpha_cp_sp0, 4.0745362639427e-09, 1e-20));
        assert!(close(ee.alpha_cp_sp1, 3.85171006200835e-09, 1e-20));
        assert_eq!(ee.off_cp_sp0, -75.0);
        assert_eq!(ee.off_cp_sp1, -77.0);
        assert_eq!(ee.k_vcp, 0.5);
        assert!(close(ee.k_tacp, 0.00457763671875, 1e-14));
        assert_eq!(ee.tgc, 1.0);
        assert_eq!(ee.resolution_ee, 2);
    }

    #[test]
    fn per_pixel_params_match_worked_example() {
        let ee = Eeprom::from_words(&datasheet_ee());
        let p = 11 * 32 + 15; // pixel (12,16)
        assert_eq!(ee.offset[p], 2.0);
        assert_eq!(ee.alpha_pixel[p], 10.0);
        assert!(close(ee.k_ta[p], 0.005126953125, 1e-12));
        assert_eq!(ee.k_v[p], 0.5);
        // pix_OS_ref(12,16) = -69 + (-1)*4 + (-2)*2 + 2*1 = -75
        let os_ref = ee.offset_average
            + ee.occ_row[11] * (1 << ee.scale_occ_row) as f64
            + ee.occ_col[15] * (1 << ee.scale_occ_col) as f64
            + ee.offset[p] * (1 << ee.scale_occ_rem) as f64;
        assert_eq!(os_ref, -75.0);
        // α(12,16) = (12100 + 3*2^9 + 3*2^10 + 10*2^6)/2^37
        let a = (ee.alpha_ref
            + ee.acc_row[11] * (1 << ee.scale_acc_row) as f64
            + ee.acc_col[15] * (1 << ee.scale_acc_col) as f64
            + ee.alpha_pixel[p] * (1 << ee.scale_acc_rem) as f64)
            / (1u64 << ee.alpha_scale) as f64;
        assert!(close(a, 1.26223312690854e-07, 1e-19));
    }

    #[test]
    fn pattern_chess_pixel_368_is_subpage0() {
        // Datasheet worked example: pn=368 → chess Pattern = 0.
        assert_eq!(Eeprom::pattern_chess(368), 0);
        assert_eq!(Eeprom::pattern_chess(367), 1);
        assert_eq!(Eeprom::pattern_chess(1), 0);
    }

    #[test]
    fn common_params_match_worked_example() {
        let ee = Eeprom::from_words(&datasheet_ee());
        let c = ee.compute_common(&table11_aux());
        assert_eq!(c.res_corr, 1.0);
        assert!(close(c.vdd, 3.3186, 1e-3));
        assert!(close(c.ta, 39.184, 5e-3));
        assert!(close(c.k_gain, 1.01753546947234, 1e-12));
    }

    #[test]
    fn full_to_chain_matches_worked_example() {
        let ee = Eeprom::from_words(&datasheet_ee());
        let mut frame = [0i16; PIXELS];
        frame[367] = 609; // pixel (12,16) = RAM[0x056F]
        let (to, c) = ee.compute_to(&frame, &table11_aux(), 1.0, Some(31.0));
        assert!(close(c.ta, 39.184, 5e-3));
        // Formula-faithful result (erratum #8 in the math notes: the datasheet
        // example mixes CP_SP1 into the IR path at Pattern=0, printing 80.36;
        // the formulas give ≈80.1). Accept the band around the true 80 °C.
        let got = f64::from(to[367]);
        assert!(
            got > 79.8 && got < 80.6,
            "To(12,16) = {got}, expected ≈80.1 (formula-faithful) / 80.36 (datasheet print)"
        );
        // Untouched pixels: raw 0 → gain 0 → v_ir = -os_ref*… (large negative
        // IR) → very low To; must remain finite, never NaN.
        for v in to.iter() {
            assert!(v.is_finite());
        }
    }

    #[test]
    fn real_device_fixture_decodes_and_bench_ta_sane() {
        // Real EEPROM from the bench sensor (ID 0x1901) + the aux readings
        // measured over bit-bang on 2026-11-03: PTAT=+1599 (healthy!).
        let blob = include_bytes!("../../tests/fixtures/mlx-ee-0x1901.bin");
        assert_eq!(blob.len(), 1664);
        let words: Vec<u16> = blob
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        let ee = Eeprom::from_words(&words);
        let aux = AuxReadings {
            vbe: 18942,
            cp_sp0: -51,
            cp_sp1: -49,
            gain: 5750,
            ptat: 1599,
            vdd: -12217,
            control: 0x1901,
            status: 0,
        };
        let c = ee.compute_common(&aux);
        // Measured on the bench: Vdd ≈ 3.287 V, Ta ≈ 34.2 °C.
        assert!(close(c.vdd, 3.287, 0.01), "vdd = {}", c.vdd);
        assert!(
            c.ta > 30.0 && c.ta < 38.0,
            "bench Ta = {} should sit in 30..38 °C",
            c.ta
        );
        // Full To frame over a synthetic flat frame must stay finite and in
        // a physically plausible band.
        let frame = [-70i16; PIXELS];
        let (to, _) = ee.compute_to(&frame, &aux, 1.0, None);
        assert!(to.iter().all(|v| v.is_finite()));
        assert!(to.iter().all(|v| *v > -50.0 && *v < 120.0));
    }

    #[test]
    fn extended_ranges_classify_and_correct() {
        let ee = Eeprom::from_words(&datasheet_ee());
        assert_eq!(ee.classify(-10.0), 1);
        assert_eq!(ee.classify(25.0), 2);
        assert_eq!(ee.classify(200.0), 3);
        assert_eq!(ee.classify(400.0), 4);
        // α_corr(2) == 1; range1/range3 corrections move alpha off it.
        assert_eq!(ee.alpha_corr(2), 1.0);
        assert!((ee.alpha_corr(1) - 1.0).abs() > 1e-9);
        assert!((ee.alpha_corr(3) - 1.0).abs() > 1e-9);
    }
}
