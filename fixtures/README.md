# fixtures —— 合成 BLF / DBC 测试素材

由 `cargo run -p fast_blf_core --example make_fixtures` 生成，幂等（可重复运行覆盖）。

## 文件

| 文件 | 说明 |
|---|---|
| `multi_channel.blf` | 5 秒合成数据；Ch1 两报文 + Ch2 一波形，含 1 帧 CAN FD |
| `demo.dbc` | 三条报文的 DBC 定义 |
| `README.md` | 本文件 |

## 通道 / 报文 / 周期 / 信号

| 通道 | 报文 | ID | 周期 | 帧数 | 信号 |
|---|---|---|---|---|---|
| Ch1 | `VehSpd` | `0x123`（标准帧） | 10 ms | 500 | `Spd`（Intel, unsigned, start=0, len=16, factor=0.01, unit km/h）、`SpdValid`（Intel, unsigned, start=16, len=1） |
| Ch1 | `BattTemp` | `0x1A2`（标准帧） | 100 ms | 50 | `Temp1`（Motorola, signed, start=7, len=8）、`Temp2`（Motorola, unsigned, start=4, len=12） |
| Ch2 | `Insulation` | `0x200`（标准帧） | 1000 ms | 5 | `Res`（Intel, unsigned, start=0, len=32, factor=1.0, unit Ohm） |

- 时间戳 = `i × 周期`，写文件时按 BLF 规范量化到纳秒。
- `multi_channel.blf` 里 **最后 1 帧 `Insulation` 是 CAN FD**（`is_fd = true`，DLC 12），用于覆盖 FD 解析路径。
- 注入值：`VehSpd.Spd` 的第 `i` 帧原始值 = `i`（物理值 `i × 0.01`）；`BattTemp.Temp1` 恒为 `0xF6`（有符号 = `-10.0`）；`Insulation.Res` 第 `i` 帧 = `i`。

## 预期子表行数

| 子表 | 行数 |
|---|---|
| `Ch1_VehSpd` | 500 |
| `Ch1_BattTemp` | 50 |
| `Ch2_Insulation` | 5 |
| 合计 | 555 |
