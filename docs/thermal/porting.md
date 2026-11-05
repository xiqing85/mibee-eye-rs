# 硬件适配指南 — 不同模块 × 不同主板

mibee-eye-thermal 的传感器接入按三层解耦设计：**总线传输**（硬件 TWI / sysfs GPIO
位模拟 / 自动探测）、**器件地址**（A1/A0 strapping 可移位）、**引脚号**（每板不同）。
目标：换模块、换板子，跑一次 `scan`、贴一段配置即可。

```sh
# 发现所有可达的 MLX90640（硬件总线 × 0x30..0x3E + GPIO 候选对）
sudo mibee-eye-thermal scan [--gpio-candidates 226,227:8,10]
```

> `sudo`：GPIO 候选对的 sysfs export 与 TWI 控制器 unbind 都是 root-only 写。
> 非 root 运行时 scan 会明确报告 `Permission denied … run scan with sudo`，
> 而**不会**静默漏报。对器件本身 scan 保持只读（每个组合只读一个 EE ID
> 字），对总线上其它芯片（PMIC、HDMI DDC、PHY）无副作用；探 GPIO 候选对
> 前会先做与 `serve` 相同的引脚释放（unbind TWI2 + 清 stale export）。

输出每条命中的 `bus` + `addr` + 即贴即用的配置段：

```
found MLX90640: bus=gpio:226,227 addr=0x33 id=0x1901
config:
[sensor]
bus = { type = "gpio", sda = 226, scl = 227 }
addr = 0x33
```

## Web 端配置与扫描

同一套能力也长在 HTTP API 上（family 信封、需登录/Bearer）——`docs` 之外
的运维不需要 SSH：

- `GET /api/hardware` — 当前总线/地址 + 上次扫描结果
- `POST /api/hardware/scan` — 触发一次扫描（后台任务；可选 body
  `{"gpio_candidates":[[sda,scl],...]}`）；`GET /api/hardware/scan` 轮询结果
- `PUT /api/hardware/config` — 改总线/地址（立即生效 + 写
  `hardware.toml` 覆盖文件，重启不丢）

扫描期间传感器循环让出总线（GB28181 画面停在最后一帧，扫描完自动恢复）；
webui 前端有「硬件接入」卡片（能力键 `thermal.hardware`）可视化以上操作。

也可以让服务开机自探测（硬件 TWI 优先，位模拟兜底）：

```toml
[sensor]
bus = { type = "auto" }   # 或显式 twi:"/dev/i2c-2" / gpio:{sda,scl}
addr = 0x33               # A1/A0 拉线时可移位到 0x30..0x3E —— scan 会告诉你实际值
```

## 换模块（MLX90640 变体）

| 差异点 | 影响 | 适配 |
|---|---|---|
| BAA / BAB（FOV 55°/110°） | 仅光学，寄存器/EE 相同 | 零改动 |
| A1/A0 地址拉线 | 7 位地址在 0x30..=0x3E 移动 | `scan` 报告实际地址 → `sensor.addr` |
| 模块不带/带上拉 | 决定硬件 TWI 可用性 | 见下「总线选择」 |
| 供电 3.3V | 实测健康（Vdd 读数 3.287V） | 无需 5V（旧「欠压」结论为误诊，见 temperature-math 笔记 §6） |

## 换主板 checklist

1. **找 I2C 控制器**：`ls /dev/i2c-*`；每条总线的归属看
   `cat /sys/bus/i2c/devices/i2c-N/name`（别对着 HDMI DDC 或 PMIC 总线调试——
   `scan` 输出里已标注每条总线的 name）。
2. **排针 TWI 启用**：设备树 overlay（如 Armbian `overlays=i2c2-ph`）。**先查
   pinmux 冲突**：Orange Pi Zero 3 上 `*-pi` 系列 overlay 与千兆 PHY 的 RGMII
   引脚物理冲突，启用即丢网口——血案全记录见 `docs/hardware-notes.md`。
3. **上拉评估**：模块自带上拉 → 硬件 TWI（100-400kHz）。无上拉（上升沿
   240-510µs）→ 只能位模拟（~1.6kHz）或外接 2×4.7kΩ 后切 TWI。
4. **位模拟引脚号**：sysfs 全局号 = `gpiochip base + bank 偏移`
   （`gpioinfo`/`cat /sys/class/gpio/gpiochip*/label`）。选**不与板载外设
   复用冲突**的引脚对；位模拟前驱动会自动 unbind 对应 TWI 控制器并做
   **总线恢复**（从机停在事务中按住 SDA 时打 ≤32 个 SCL 时钟冲出）。
5. **验证**：`scan`（应报告一条命中）→ `mibee-eye-thermal ta --bus ...`
   （应输出合理的 Vdd≈3.3V / Ta=室温）→ `serve`。

## 内置的硬件鲁棒性

- **总线恢复**（bitbang.rs）：主机死在块读中途会让 MLX90640（无 SMBus 超时）
  永远按住 SDA；每次开总线先检测卡低并冲时钟 + STOP。
- **EE 像素失效位**（ee.rs）：datasheet NOTE 1-3 — 0x0000 像素字 = 死像素、
  bit0 = 标定超差，`Eeprom.dead_pixel[]` 已暴露给上层替换策略。
- **地址窗口探测**（scan.rs）：0x30..=0x3E 全覆盖 strapping，ID 合理性过滤
  （0x0000/0xFFFF 剔除）。

## 已验证平台

| 板 | 总线 | 状态 |
|---|---|---|
| Orange Pi Zero 3 (H618) | gpio:226,227（PH2/PH3，位模拟） | 生产运行（本仓 .40） |
| Orange Pi Zero 3 (H618) | twi:/dev/i2c-2（i2c2-ph overlay） | 等上拉电阻后切换 |

新平台欢迎按上文 checklist 走一遍并把结果补进本表。
