# 热像 GB 通道（rs#3 "A 方案"）与温度越限告警（M4）

双光谱设备的国标接入形态：**同一进程内跑两个 GB28181 设备实例**——主实例
（`[gb28181]`，可见光相机）+ 热像实例（`[gb28181_thermal]`，`PhotoelectricImagingType = 2`）。
平台视角是两台独立设备（各自 REGISTER/心跳/Catalog/INVITE），天然兼容
"每通道一相机"的 NVR 模型，无需库侧多通道改造。

## 数据流

```
MLX90640 poll (thermal/mod.rs, ~1s/pass, hflip/vflip 已应用)
  └─ ThermalService.grid() ──┬─ web /api/thermal/* (SPEC §3.7)
                             ├─ SSE thermal.temperature
                             ├─ M4 alarm engine (thermal/alarm.rs, 1 Hz)
                             └─ thermal/gb_stream.rs 编码线程
                                 render_rgb_annotated (ironbow + max/min/中心 OSD)
                                 → rgb_to_i420 → openh264 320×240@4fps/300kbps/GOP12
                                 → 专属 AuHub → 第二 Gb28181Server (SIP 5061)
                                    → RTP/PS 出流 + IFrameCmd 强 IDR
```

- 传感器丢失：画面冻结在最后一帧 grid（不掐流）；payload 走 `trusted:false`
  + `last_known_good`（SPEC §3.7 M4 dialect）。
- 编码器失败：按 tick 重建（秒级恢复）。

## 配置

```toml
[thermal]                # 传感器本体（SPEC §3.7）
enabled = true
bus = { type = "auto" }

[gb28181_thermal]        # 第二设备实例；平台地址/域/口令继承 [gb28181]
enabled = true
#device_id = "34020000001320000003"     # 默认复用退役 mibee-eye-thermal 台
#channel_id = "34020000001310000003"    # 设备…132→通道…131，同主实例惯例
#local_sip_port = 5061
#width/height/fps/bitrate_bps/i_period = 320/240/4/300000/12   # NVR 约定档

[thermal.alarm]          # M4 温度越限（需要 [gb28181_thermal]）
#enabled = false
#threshold_c = 60.0  #hysteresis_c = 3.0  #sustain_secs = 5
#cooldown_secs = 60  #priority = 1
```

## M4 线格式（GB/T 28181-2022 A.2.5.5 代码表考证）

NOTIFY 的 `AlarmType` 列表**没有独立"火警"码**；温度告警的标准载体是
`AlarmMethod = 2`（设备报警）下的 **`AlarmType = 4`（高温报警）**
（5 = 低温报警）。`AlarmPriority` 1..=4，默认 1（一级警情，火警级）。

规则引擎（`thermal/alarm.rs` 纯状态机，单测钉板）：

| 机制 | 参数 | 语义 |
| --- | --- | --- |
| 持续确认 | `sustain_secs` | 连续越限 N 秒才发（挥手/反射瞬态不触发） |
| 回差带 | `hysteresis_c` | 触发后降到 `threshold - hysteresis` 才复位（防边界抖动） |
| 冷却 | `cooldown_secs` | 两次告警最小间隔（防风暴） |
| 传感器丢失 | — | untrusted 期间**冻结状态机**：不误报、也不静默清除已活告警 |

## 运维要点

- 两实例各自优雅注销：SIGTERM/`POST /api/system/restart` 时按组等待
  （每实例 5s、总 10s 上限），先 REGISTER `Expires: 0` 再退出。
- 依赖 `software-encoder` feature（默认开）；无该 feature 的构建在
  `[gb28181_thermal] enabled = true` 时启动日志明确拒绝，不静默降级。
- `.104` 实况：M5（192.168.63.30）auto-enroll 后即刻 INVITE 两个通道；
  thermal 实例同样被 SUBSCRIBE Catalog+Alarm（重启后 ~1s 内重订）。

## 时间线

- 2026-11-05：通道 + M4 上线（rs d5ec711 + ae68d01）；13:32:04 实弹告警
  （阈值临时压 25°C，max 29.9°C，priority 1）。
