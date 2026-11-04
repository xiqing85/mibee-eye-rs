//! Transport-agnostic MLX90640 bus contract + both Linux transports.
//!
//! MLX90640 uses 16-bit register addressing with SMBus-style word/block reads:
//! `START, addr+W, regH, regL, RESTART, addr+R, ...data..., STOP`.
//! Two implementations back the trait in this file:
//! - [`Twi`]: kernel hardware I2C (`/dev/i2c-N`) — preferred once the bus
//!   has proper pull-ups (stock on Raspberry Pi headers).
//! - [`BitBang`]: userspace sysfs-GPIO bit-bang — the bring-up path on
//!   boards where the module lacks pull-ups and the bus must run ~1 kHz.

use super::error::{Error, Result};

pub trait MlxBus {
    /// Read a single 16-bit register (big-endian on the wire).
    fn read_reg16(&mut self, reg: u16) -> Result<u16>;

    /// Read `buf.len()` bytes starting at `reg` (auto-incrementing block read).
    fn read_block(&mut self, reg: u16, buf: &mut [u8]) -> Result<()>;

    /// Write a single 16-bit register.
    /// Reserved for the configuration milestone (refresh rate / chess mode);
    /// bring-up is read-only.
    #[allow(dead_code)]
    fn write_reg16(&mut self, reg: u16, val: u16) -> Result<()>;

    /// Read a run of 16-bit words into `out` (block read, chunked).
    fn read_words(&mut self, base: u16, out: &mut [u16]) -> Result<()> {
        // 256-byte chunks validated on real hardware (32x24 full-frame dumps).
        const CHUNK_WORDS: usize = 128;
        let mut byte = 0usize;
        while byte < out.len() * 2 {
            let reg = base + (byte / 2) as u16;
            let take = ((out.len() * 2 - byte) / 2).min(CHUNK_WORDS) * 2;
            let mut tmp = vec![0u8; take];
            self.read_block(reg, &mut tmp)?;
            for (i, w) in tmp.as_chunks::<2>().0.iter().enumerate() {
                out[byte / 2 + i] = u16::from_be_bytes([w[0], w[1]]);
            }
            byte += take;
        }
        Ok(())
    }
}

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;

const I2C_RDWR: libc::c_ulong = 0x0707;
const I2C_M_RD: u16 = 0x0001;

#[repr(C)]
struct I2cMsg {
    addr: u16,
    flags: u16,
    len: u16,
    _pad: u16,
    buf: *mut u8,
}

#[repr(C)]
struct I2cRdwrIoctlData {
    msgs: *mut I2cMsg,
    nmsgs: libc::c_uint,
}

pub struct Twi {
    file: File,
    addr: u8,
}

impl Twi {
    /// `addr` is the 7-bit device address (MLX90640 factory default 0x33).
    pub fn open(path: &str, addr: u8) -> Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Ok(Self { file, addr })
    }

    /// One ioctl: [write regH,regL] + repeated-start + [read n bytes].
    fn wr_rd(&mut self, reg: u16, rd_len: usize, wr_extra: &[u8]) -> Result<Vec<u8>> {
        let mut wr = vec![(reg >> 8) as u8, reg as u8];
        wr.extend_from_slice(wr_extra);
        let mut out = vec![0u8; rd_len];
        let mut msgs = [
            I2cMsg {
                addr: u16::from(self.addr),
                flags: 0,
                len: wr.len() as u16,
                _pad: 0,
                buf: wr.as_mut_ptr(),
            },
            I2cMsg {
                addr: u16::from(self.addr),
                flags: I2C_M_RD,
                len: rd_len as u16,
                _pad: 0,
                buf: out.as_mut_ptr(),
            },
        ];
        let data = I2cRdwrIoctlData {
            msgs: msgs.as_mut_ptr(),
            nmsgs: 2,
        };
        // SAFETY: fd is a live i2c-dev handle; pointers point at the buffers above.
        let rc = unsafe { libc::ioctl(self.file.as_raw_fd(), I2C_RDWR, &data) };
        if rc < 0 {
            return Err(Error::Bus(format!(
                "I2C_RDWR {:#06x} ({}B read): {}",
                reg,
                rd_len,
                std::io::Error::last_os_error()
            )));
        }
        Ok(out)
    }
}

impl MlxBus for Twi {
    fn read_reg16(&mut self, reg: u16) -> Result<u16> {
        let b = self.wr_rd(reg, 2, &[])?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn read_block(&mut self, reg: u16, buf: &mut [u8]) -> Result<()> {
        buf.copy_from_slice(&self.wr_rd(reg, buf.len(), &[])?);
        Ok(())
    }

    fn write_reg16(&mut self, reg: u16, val: u16) -> Result<()> {
        let mut wr = vec![(reg >> 8) as u8, reg as u8];
        wr.extend_from_slice(&val.to_be_bytes());
        let mut msgs = [I2cMsg {
            addr: u16::from(self.addr),
            flags: 0,
            len: wr.len() as u16,
            _pad: 0,
            buf: wr.as_mut_ptr(),
        }];
        let data = I2cRdwrIoctlData {
            msgs: msgs.as_mut_ptr(),
            nmsgs: 1,
        };
        let rc = unsafe { libc::ioctl(self.file.as_raw_fd(), I2C_RDWR, &data) };
        if rc < 0 {
            return Err(Error::Bus(format!(
                "I2C_RDWR write {:#06x}: {}",
                reg,
                std::io::Error::last_os_error()
            )));
        }
        Ok(())
    }
}

use std::io::Write as _;
use std::path::PathBuf;
use std::thread::sleep;
use std::time::Duration;

const D: Duration = Duration::from_micros(400); // ~1.6 kHz SCL

struct Gpio {
    num: u32,
    root: PathBuf,
}

impl Gpio {
    fn new(num: u32) -> Result<Self> {
        let base = PathBuf::from("/sys/class/gpio");
        if !base.join(format!("gpio{num}")).exists() {
            File::create(base.join("export"))?.write_all(num.to_string().as_bytes())?;
            sleep(Duration::from_millis(50));
        }
        let root = base.join(format!("gpio{num}"));
        let g = Self { num, root };
        g.rz()?; // idle: released (input), pulled high
        Ok(g)
    }

    fn w(&self, name: &str, val: &str) -> Result<()> {
        let mut f = OpenOptions::new().write(true).open(self.root.join(name))?;
        f.write_all(val.as_bytes())?;
        Ok(())
    }

    /// Drive the line low (open-drain "0").
    fn lo(&self) -> Result<()> {
        self.w("direction", "out")?;
        self.w("value", "0")
    }

    /// Release the line (input; pull-up brings it high).
    fn rz(&self) -> Result<()> {
        self.w("direction", "in")
    }

    fn rd(&self) -> Result<u8> {
        Ok(std::fs::read_to_string(self.root.join("value"))?
            .trim()
            .parse()
            .unwrap_or(0))
    }
}

impl Drop for Gpio {
    fn drop(&mut self) {
        if let Ok(mut f) = File::create("/sys/class/gpio/unexport") {
            let _ = f.write_all(self.num.to_string().as_bytes());
        }
    }
}

pub struct BitBang {
    addr: u8,
    sda: Gpio,
    scl: Gpio,
}

fn d(n: u32) {
    sleep(D * n);
}

/// Classic I2C bus recovery: toggle SCL with SDA released until the slave
/// lets SDA rise (≤32 clocks covers any reasonable outstanding transfer),
/// then generate a STOP so the slave resynchronizes to an idle bus.
fn recover_bus(sda: &Gpio, scl: &Gpio) -> Result<()> {
    for _ in 0..32 {
        scl.lo()?;
        d(1);
        scl.rz()?;
        d(2); // generous: weak pull-up rise is 240-510 µs
        if sda.rd()? != 0 {
            // SDA released — finish with a STOP condition.
            scl.lo()?;
            d(1);
            sda.lo()?;
            d(1);
            scl.rz()?;
            d(2);
            sda.rz()?;
            d(2);
            return Ok(());
        }
    }
    // Still low after 32 clocks — genuinely stuck hardware.
    Ok(())
}

impl BitBang {
    /// `sda`/`scl` are global sysfs GPIO numbers (PH2=226 / PH3=227 on the
    /// H616/H618 pinctrl at gpiochip0, base 0). `addr` is the 7-bit address
    /// (MLX90640 factory default 0x33).
    pub fn new(sda: u32, scl: u32, addr: u8) -> Result<Self> {
        let sda = Gpio::new(sda)?;
        let scl = Gpio::new(scl)?;
        // Both lines must idle high — a stuck-low SDA usually means the slave
        // is parked mid-transaction (a previous master died mid-block-read;
        // MLX90640 never times out and holds SDA low awaiting clocks). Clock
        // it out before declaring the bus dead: SCL toggles with SDA
        // released until SDA rises, then a STOP condition resyncs the slave.
        d(2);
        if sda.rd()? == 0 {
            recover_bus(&sda, &scl)?;
        }
        if scl.rd()? == 0 {
            return Err(Error::Bus(
                "SCL stuck low: check module VCC/GND and pull-ups".into(),
            ));
        }
        d(2);
        if sda.rd()? == 0 {
            return Err(Error::Bus(
                "SDA stuck low after bus recovery: check module VCC/GND and pull-ups".into(),
            ));
        }
        Ok(Self { addr, sda, scl })
    }

    /// Retarget the probe address without re-opening the bus — `scan` walks
    /// 0x30..=0x3E on one exported pair instead of exporting per address.
    pub fn set_addr(&mut self, addr: u8) {
        self.addr = addr;
    }

    fn start(&mut self) -> Result<()> {
        self.sda.rz()?;
        self.scl.rz()?;
        d(2);
        self.sda.lo()?;
        d(1);
        self.scl.lo()?;
        d(1);
        Ok(())
    }

    fn rstart(&mut self) -> Result<()> {
        self.sda.rz()?;
        d(1);
        self.scl.rz()?;
        d(3);
        self.sda.lo()?;
        d(1);
        self.scl.lo()?;
        d(1);
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        self.sda.lo()?;
        d(1);
        self.scl.rz()?;
        d(3);
        self.sda.rz()?;
        d(2);
        Ok(())
    }

    fn wr_bit(&mut self, b: bool) -> Result<()> {
        if b {
            self.sda.rz()?;
        } else {
            self.sda.lo()?;
        }
        d(1);
        self.scl.rz()?;
        d(3);
        self.scl.lo()?;
        d(1);
        Ok(())
    }

    fn rd_bit(&mut self) -> Result<u8> {
        self.sda.rz()?;
        d(1);
        self.scl.rz()?;
        d(3);
        let v = self.sda.rd()?;
        self.scl.lo()?;
        d(1);
        Ok(v)
    }

    fn send_byte(&mut self, v: u8) -> Result<bool> {
        for i in (0..8).rev() {
            self.wr_bit(v >> i & 1 == 1)?;
        }
        Ok(self.rd_bit()? == 0) // slave ACK
    }

    fn mack(&mut self) -> Result<()> {
        self.sda.lo()?;
        d(1);
        self.scl.rz()?;
        d(3);
        self.scl.lo()?;
        d(1);
        self.sda.rz()
    }

    fn mnak(&mut self) -> Result<()> {
        self.sda.rz()?;
        d(1);
        self.scl.rz()?;
        d(3);
        self.scl.lo()?;
        d(1);
        Ok(())
    }

    fn read_block_at(&mut self, reg: u16, buf: &mut [u8]) -> Result<()> {
        let addr = self.addr;
        self.start()?;
        if !self.send_byte(addr << 1)? {
            self.stop()?;
            return Err(Error::Bus(format!("NAK addr {addr:#04x} (write phase)")));
        }
        if !self.send_byte((reg >> 8) as u8)? {
            self.stop()?;
            return Err(Error::Bus("NAK reg-hi".into()));
        }
        if !self.send_byte(reg as u8)? {
            self.stop()?;
            return Err(Error::Bus("NAK reg-lo".into()));
        }
        self.rstart()?;
        if !self.send_byte((addr << 1) | 1)? {
            self.stop()?;
            return Err(Error::Bus(format!("NAK addr {addr:#04x} (read phase)")));
        }
        let len = buf.len();
        for (i, slot) in buf.iter_mut().enumerate() {
            let mut b = 0u8;
            for _ in 0..8 {
                b = (b << 1) | self.rd_bit()?;
            }
            if i + 1 < len {
                self.mack()?;
            } else {
                self.mnak()?;
            }
            *slot = b;
        }
        self.stop()?;
        Ok(())
    }
}

impl MlxBus for BitBang {
    fn read_reg16(&mut self, reg: u16) -> Result<u16> {
        let mut b = [0u8; 2];
        self.read_block_at(reg, &mut b)?;
        Ok(u16::from_be_bytes(b))
    }

    fn read_block(&mut self, reg: u16, buf: &mut [u8]) -> Result<()> {
        self.read_block_at(reg, buf)
    }

    fn write_reg16(&mut self, _reg: u16, _val: u16) -> Result<()> {
        // Only needed later (refresh-rate / chess-mode configuration); the
        // bring-up path is read-only.
        Err(Error::Usage("bitbang write not implemented yet".into()))
    }
}
