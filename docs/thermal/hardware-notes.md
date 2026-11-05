# Hardware notes — Orange Pi Zero 3 + MLX90640

Field findings from the bring-up on `orangepizero3` (H618, Armbian 26.8 trixie,
kernel 6.18-current-sunxi). Everything below was verified on real hardware
unless marked otherwise.

## The one thing that bites: header I2C vs gigabit ethernet

**The Zero 3 multiplexes almost its entire GPIO header across the same SoC pins
as the RGMII ethernet PHY.** From the stock device tree:

```
rgmii-pins (emac0): PI0-PI5, PI7-PI15      <- gigabit PHY (YT8531)
i2c0-pins  (TWI0):  PI5, PI6   <- header pins 3/5
i2c1-pi    (TWI1):  PI7, PI8
i2c2-pi    (TWI2):  PI9, PI10
```

Enabling any of the `*-pi` I2C overlays steals RGMII pins. With `i2c0-pi`
(PI5/PI6 → header pins 3/5) the MAC cannot claim its pin group and the whole
`end0` interface disappears:

```
sun50i-h616-pinctrl: pin PI5 already requested by 5002000.i2c; cannot claim for 5020000.ethernet
dwmac-sun8i 5020000.ethernet: Error applying setting, reverse things back
```

Additionally the PHY actively drives its RGMII lines once out of reset, so a
sensor wired to pins 3/5 sees bus contention — a device may ACK for the first
seconds after power-on and then go silent (observed: phantom `0x3d` on TWI0).

**The only usable header pair is PH2/PH3 = physical pins 8/10** (the classic
Pi UART position, silk TX/RX), muxed to **TWI2**:

```text
sensor SDA -> pin 8  (PH2)     sensor SCL -> pin 10 (PH3)
VCC 3.3 V  -> pin 1  (or pin 2 = 5 V if the module has an onboard LDO)
GND        -> pin 6
```

Enable with `overlays=i2c2-ph` in `/boot/armbianEnv.txt` (stock overlay).
Ethernet and TWI2 coexist — verified across reboots.

## Bus speed vs pull-ups (current limiting factor)

Measured SDA/SCL rise times on this wiring: **240–510 µs** (healthy I2C is
< 1 µs) — the module provides no usable pull-ups, the SoC's internal ones are
too weak for the wire capacitance.

| Bus                     | Speed      | Result                                     |
|-------------------------|------------|--------------------------------------------|
| hardware TWI2 (stock)   | 100 kHz    | clean NAK on every address                 |
| hardware TWI2 (overlay) | 25 kHz     | clean NAK on every address                 |
| sysfs-GPIO bit-bang     | ~1.6 kHz   | **ACK at 0x33, full 768-word frames**      |

Until external pull-ups (2× 4.7 kΩ to 3.3 V on SDA/SCL) are added, the only
working path is the slow bit-bang (`--bus gpio:226,227`, ~40 s/frame).
With pull-ups in place the hardware TWI unlocks 100–400 kHz and 2–8 fps.

GPIO numbers on this board: PH2 = 226, PH3 = 227 (gpiochip0 `300b000.pinctrl`,
base 0, PH bank = 7×32). Slow-speed overlay sources live in
`deploy/overlays/`.

## Data-integrity caveat (open)

With the current marginal wiring the sensor ACKs and returns complete,
spatially coherent frames — but every word reads `0xFFxx` and PTAT is negative
(`-47`). Pattern matches an undervolted analog section: if the module carries
an onboard LDO (5 V input), it **must** be fed from pin 2 (5 V), not pin 1
(3.3 V). Absolute temperatures are untrusted until this is resolved; relative
per-frame structure is real (stable across frames, responds to scene layout).

## Wifi fallback path (for remote bring-up)

`/etc/netplan/20-wifi-backup.yaml` keeps a second (wlan0) path alive alongside
`end0` — invaluable when an overlay experiment kills the wired interface:

```yaml
network:
  version: 2
  renderer: networkd
  wifis:
    wlan0:
      dhcp4: true
      optional: true
      access-points:
        MickeyMiRoute: { password: "…" }
```

## Probe tools

`tools/mlx-bitscan.py` — bit-bang address scan, both SDA/SCL polarities
(settles "did I wire it backwards?" without touching a wire).

`tools/mlx-bitread.py` — register reads over bit-bang (16-bit MLX90640
addressing, repeated-start semantics).

`tools/mlx-frame.py` — full 768-word frame dump + 8x6 downsample.

All three need the pins free: unbind TWI2 first
(`echo 5002800.i2c > /sys/bus/platform/drivers/mv64xxx_i2c/unbind`), rebind
after.
