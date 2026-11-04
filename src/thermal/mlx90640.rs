//! MLX90640 32x24 far-infrared array — register map, identity, raw frames.
//!
//! Register map corrected against datasheet Rev 11 (Figure 11/12/15):
//! the configuration window lives at 0x8000+ (status/control), the RAM
//! special area holds Vbe/CP/GAIN/PTAT/VDD. See
//! `docs/mlx90640-temperature-math.md` §1 for the full map and the
//! hard-won lesson about 0x0708 being CP(SP0), *not* PTAT.

use super::error::Result;
use super::i2c::MlxBus;

pub const WIDTH: usize = 32;
pub const HEIGHT: usize = 24;
pub const PIXELS: usize = WIDTH * HEIGHT;

pub const DEFAULT_ADDR: u8 = 0x33;

// Register map (MLX90640 datasheet Rev 11, word-addressed big-endian).
pub const RAM_IR_BASE: u16 = 0x0400;
pub const REG_STATUS: u16 = 0x8000;
pub const REG_CONTROL: u16 = 0x800D;
// RAM special data area (datasheet Figure 15).
pub const REG_VBE: u16 = 0x0700;
pub const REG_CP_SP0: u16 = 0x0708;
pub const REG_GAIN: u16 = 0x070A;
pub const REG_PTAT: u16 = 0x0720;
pub const REG_CP_SP1: u16 = 0x0728;
pub const REG_VDD: u16 = 0x072A;
/// Calibration block — one word per pixel plus 64 global words.
pub const EE_BASE: u16 = 0x2400;
pub const EE_WORDS: usize = 832;
pub const REG_ID: u16 = 0x240C;

/// Auxiliary (per-frame, common) sensor readings from the RAM special area.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AuxReadings {
    pub vbe: i16,
    pub cp_sp0: i16,
    pub cp_sp1: i16,
    pub gain: i16,
    pub ptat: i16,
    pub vdd: i16,
    /// Control register 1 (0x800D); resolution bits [11:10] feed Res_corr.
    pub control: u16,
    pub status: u16,
}

pub struct Mlx90640<B: MlxBus> {
    bus: B,
}

impl<B: MlxBus> Mlx90640<B> {
    pub fn new(bus: B) -> Self {
        Self { bus }
    }

    /// Identity register. A NAK here means wiring/address trouble; a plausible
    /// non-0xFFFF value means the protocol layer is alive.
    pub fn read_id(&mut self) -> Result<u16> {
        self.bus.read_reg16(REG_ID)
    }

    pub fn read_status(&mut self) -> Result<u16> {
        self.bus.read_reg16(REG_STATUS)
    }

    pub fn read_control(&mut self) -> Result<u16> {
        self.bus.read_reg16(REG_CONTROL)
    }

    /// Die temperature proxy at the *correct* address (0x0720). Positive in
    /// the hundreds-to-thousands on a healthy sensor. (0x0708 is CP(SP0) and
    /// is *supposed* to read as a small negative number.)
    pub fn read_ptat(&mut self) -> Result<i16> {
        Ok(self.bus.read_reg16(REG_PTAT)? as i16)
    }

    /// Read one full IR frame (768 raw signed words, row-major 32x24).
    pub fn read_frame_raw(&mut self) -> Result<[i16; PIXELS]> {
        let mut words = [0u16; PIXELS];
        self.bus.read_words(RAM_IR_BASE, &mut words)?;
        Ok(words.map(|w| w as i16))
    }

    /// Read the RAM special area + control registers needed by the
    /// temperature math (§11.2.2.1–11.2.2.4).
    pub fn read_aux(&mut self) -> Result<AuxReadings> {
        Ok(AuxReadings {
            vbe: self.bus.read_reg16(REG_VBE)? as i16,
            cp_sp0: self.bus.read_reg16(REG_CP_SP0)? as i16,
            cp_sp1: self.bus.read_reg16(REG_CP_SP1)? as i16,
            gain: self.bus.read_reg16(REG_GAIN)? as i16,
            ptat: self.bus.read_reg16(REG_PTAT)? as i16,
            vdd: self.bus.read_reg16(REG_VDD)? as i16,
            control: self.bus.read_reg16(REG_CONTROL)?,
            status: self.bus.read_reg16(REG_STATUS)?,
        })
    }

    /// Full calibration block (832 words, 0x2400–0x273F).
    pub fn read_eeprom(&mut self) -> Result<Vec<u16>> {
        let mut words = vec![0u16; EE_WORDS];
        self.bus.read_words(EE_BASE, &mut words)?;
        Ok(words)
    }

    /// Coarse 8x6 view of a frame (block means) — the same projection used
    /// during bring-up, handy for quick eyeballing in logs.
    pub fn downsample_8x6(frame: &[i16; PIXELS]) -> [[i32; 8]; 6] {
        let mut out = [[0i32; 8]; 6];
        for (gy, row) in out.iter_mut().enumerate() {
            for (gx, cell) in row.iter_mut().enumerate() {
                let mut acc = 0i32;
                for y in gy * 4..gy * 4 + 4 {
                    for x in gx * 4..gx * 4 + 4 {
                        acc += i32::from(frame[y * WIDTH + x]);
                    }
                }
                *cell = acc / 16;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downsample_block_means() {
        let mut f = [0i16; PIXELS];
        f[0] = 16; // top-left pixel only
        f[PIXELS - 1] = 32; // bottom-right pixel only
        let d = Mlx90640::<super::super::i2c::Twi>::downsample_8x6(&f);
        assert_eq!(d[0][0], 1);
        assert_eq!(d[5][7], 2);
        assert_eq!(d[2][3], 0);
    }

    #[test]
    fn register_addresses_match_datasheet() {
        // Pinned against datasheet Rev 11 (see docs/mlx90640-temperature-math.md §1).
        assert_eq!(REG_STATUS, 0x8000);
        assert_eq!(REG_CONTROL, 0x800D);
        assert_eq!(REG_VBE, 0x0700);
        assert_eq!(REG_CP_SP0, 0x0708);
        assert_eq!(REG_GAIN, 0x070A);
        assert_eq!(REG_PTAT, 0x0720);
        assert_eq!(REG_CP_SP1, 0x0728);
        assert_eq!(REG_VDD, 0x072A);
        assert_eq!(EE_BASE + EE_WORDS as u16 - 1, 0x273F);
    }
}
