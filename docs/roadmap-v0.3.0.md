# Roadmap — v0.3.0：完整国标设备侧能力包（同步发布）

> **状态**：范围冻结（2026-09-15；同日经两库全量对照调研扩充——见 15-19 行与
> 跟踪 issue）。本文是 v0.3.0 的唯一范围真源（两实现同文）。
>
> **同步发布规范**：MiBee Eye 的两个实现（[mibee-eye-rs](https://github.com/xiqing85/mibee-eye-rs)
> 与 [mibee-eye-go](https://github.com/xiqing85/mibee-eye-go)）**minor 版本
> （x.y.0，能力包）一律同步发布**——同版本号、同日打 tag、发布说明互链；一方能力
> 不齐则整体推迟，新能力只随同步的 minor 出货。**patch 版本（x.y.z，仅修复）放开**：
> 两仓可各自独立发布、版本号不必对齐（go 可在 0.2.1 时 rs 停在 0.2.3）。
> 此前单发的 rs v0.3.0 已按此规范删除，版本号留给本次同步版本。

## 目标

v0.3.0 = **GB/T 28181-2022 设备侧能力全覆盖**能力包：在既有注册/心跳/信息
查询/实时/回放/下载/抓拍/GB 35114 A 级之上，补齐订阅通知、告警、位置上报、
云台控制、语音对讲（补齐短板一侧）与 2022 版本协商。协议语义**一律先落协议库**
（Rust 侧 [gb28181-rs](https://github.com/mickeyzzc/gb28181-rs)、Go 侧
[gb28181-go](https://github.com/mickeyzzc/gb28181-go)，TDD + 线格式 golden），
产品仓只做接线与业务胶水。

## 能力矩阵（设备侧）

| # | GB/T 28181-2022 能力 | rs 现状 | go 现状 | v0.3.0 目标 | 主要落点 |
|---|---|---|---|---|---|
| 1 | 注册 / 心跳 / 摘要认证（MD5/SHA-256, qop） | ✅ | ✅ | 保持 + 回归 | — |
| 2 | 信息查询组：Catalog / DeviceInfo / DeviceStatus / RecordInfo / Keepalive | ✅ | ✅（含 2022 ExtraInfo 语义） | 保持；rs 对齐 2022 ExtraInfo | gb28181-rs |
| 3 | 实时 / 回放 / 下载 INVITE + RTP/PS（UDP/TCP）+ SIP INFO 回放控制 | ✅ | ✅ | 保持 | — |
| 4 | 平台抓拍指令应答（非标强制，事实补充） | ✅ | ✅ | 保持 | — |
| 5 | GB 35114 A 级（SM2 注册认证 + keyed-SM3） | ✅（feature） | ✅（build tag） | 保持 | — |
| 6 | X-GB-Ver 2022 版本协商（附录 I） | ❌ | ✅ | rs 补齐（对齐 gb28181-go #78 语义） | gb28181-rs |
| 7 | SUBSCRIBE/NOTIFY 订阅框架（订阅应答、Expires/续订/过期，NOTIFY 发送） | ❌（盲 200） | ❌（device 侧） | 两库新增；覆盖 Catalog / Alarm / MobilePosition 三类订阅 | 两库 |
| 8 | Alarm 告警订阅与上报（设备→平台 NOTIFY） | ❌ | ❌ | 两库实现；产品把 AI 检测（可选）接为告警源 | 两库 + 产品 |
| 9 | MobilePosition 位置订阅与上报 | ❌ | ❌ | 两库实现；固定机位按配置报静态位置、按订阅节流 | 两库 + 产品 |
| 10 | PTZ/远程控制接收（A.3 PTZCmd A505 解码 → 宿主回调） | ❌（拒绝应答） | ❌（仅 platform 侧有 builder） | 两库 device 侧解码 + 回调接缝；产品接云台（无云台时合规空应答） | 两库 + 产品 |
| 11 | HomePosition / 预置位 / 巡航（2022 查询组 A.2.4.x） | ❌（拒绝应答） | codec 已有、应答未接 | 应答接线；有云台时执行，无云台报空能力 | 两库 + 产品 |
| 12 | 语音广播通知（Broadcast）+ 语音对讲（talkback） | talkback 接收 ✅（G.711 A/μ）；Broadcast 未接 | Broadcast 回调 ✅；talkback ❌ | go 补 talkback（对齐 rs 参考实现与 golden）；两产品接 Broadcast→对讲衔接 | gb28181-go + 产品 |
| 13 | DeviceConfig 最小合规（时间同步 SetTime、远程重启 Restart、ConfigDownload 最小应答） | ❌（拒绝应答） | ❌ | 最小应答 + 执行开关（默认关闭重启类）；完整远程配置明确不做 | 两库 + 产品 |
| 14 | 告警 / 检测事件的 Web 透出（SPEC v1 事件通道） | — | — | Alarm 上报接入 SSE 事件与能力协商（SPEC v1 同版本加法，先改 SPEC.md） | 产品 + mibee-webui SPEC |
| 15 | DeviceControl 全子命令组（IFrameCmd 强制 I 帧、RecordCmd、GuardCmd、AlarmCmd、TeleBoot、DragZoom） | ❌（整族拒答） | ❌（整族拒答；platform 亦无 IFrameCmd builder） | 解码 + 宿主接缝 + 合规应答（IFrameCmd 优先，平台拉流/丢包恢复常用） | 两库（gb28181-rs#58 / gb28181-go#81） |
| 16 | 回放/下载结束 MediaStatus INFO（§9.4.2 Play/Download Finished） | ❌（不发） | ❌（不发；platform 侧已有解析） | 两库设备侧补发，平台可及时关流 | 两库（gb28181-rs#60 / gb28181-go#82） |
| 17 | 2022 信息查询组最小应答（HomePosition / CruiseTrackList / CruiseTrack / PTZPosition / SDCardStatus） | ❌ | ✅（最小应答） | rs 移植对齐 go 语义与 golden | gb28181-rs（#59） |
| 18 | 对讲上行（设备→平台 G.711 发送半边，全双工对讲/广播应答） | ❌（仅接收） | ❌（#80 先补接收） | 库级补齐发送器；产品暂无麦克风管线、后续接线 | 两库（gb28181-rs#61 / gb28181-go#83） |
| 19 | 优雅注销（shutdown 时 REGISTER Expires:0） | ❌（文档明示不做） | ❌（Stop 不注销） | 两库补 helper；平台靠心跳超时感知过慢 | 两库（gb28181-rs#62 / gb28181-go#84） |

**低优先/评估项（不阻塞 v0.3.0 冻结，issue 跟踪）**：rs 的 SIPS（SIP over
TLS）传输对齐 go（gb28181-rs#62，平台侧对设备 rarely 强制）；gb28181-go 平台侧
语音广播下发（gb28181-go#84，NVR 向，非 mibee-eye 接线项）。

**明确不做（写明理由）**：SVAC 与 GB 35114 B/C 级（需 SVAC 硬件编解码，GB/T 25724）；
SIP over WebSocket（非设备侧主流需求）；平台/UAS 角色（产品定位是设备）；
2022 HTTP 媒体面（录像 HTTP 检索）——实现面大，v0.3.0 内做不进，**评估后顺延**
至 v0.4.x，不在本包偷工。

## 验收

- 库侧：golden/单测全绿；gb28181-go 的 conformance loopback 扩展订阅/告警/PTZ
  对拍；gb28181-rs 与 gb28181-go 关键线格式互验（对齐既有 golden 共享做法）。
- 产品侧：web 层集成测试随接线同提交；告警 SSE 走 SPEC v1 加法。
- 真机：.30 测试平台互操作（注册/心跳/查询/订阅/告警/位置/云台/对讲/回放，
  只读验证）；两台 Pi 冒烟全绿。
- 发布：两仓**同日同号**发 v0.3.0，双语说明互链，产物矩阵完整（rs 含 armv7
  首个正式产物）。

## 跟踪

- 库侧：gb28181-rs #57（总）/ #58 / #59 / #60 / #61 / #62；gb28181-go #80（总）/ #81 / #82 / #83 / #84
- 产品总 issue：[xiqing85/mibee-eye-go#8](https://github.com/xiqing85/mibee-eye-go/issues/8)（两实现共用 tracker）

## 工程约束（沿工作区既有规范）

- 协议优先：先库后产品，产品只写调用 + 胶水；线格式 golden 为契约。
- 全面 TDD：测试与代码同提交；bug 修复先有复现测试。
- Web API 变更先改 `mibee-webui/SPEC.md`（同版本只做加法）。
- rs 的 gb28181-rs git pin（ILP32 修复，fc049b8）在库侧能力包发版后回切
  crates.io 版本。
