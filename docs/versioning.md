# 版本号规划

数据面 crate 当前仍是 **SemVer `1.2.2`**，与导入基线一致，避免安装脚本、二进制名和兼容矩阵一起断裂。

## 建议：控制面 / 产品线用 CalVer，数据面继续 SemVer

长期不要把「年份」当成这条 Rust 节点的 major。

| 对象 | 方案 | 例子 | 原因 |
| --- | --- | --- | --- |
| **Edge 节点（本仓库）** | 继续 **SemVer `MAJOR.MINOR.PATCH`** | `1.2.2` → `1.3.0` / `2.0.0` | Cargo、GitHub Release、`v*` tag、与 gRPC/JSON 兼容性一一对应。破坏性协议变更才加 MAJOR。 |
| **CloudSidra 全局控制面 / 套件营销版本** | **CalVer `YYYY.MINOR.PATCH`** | `2026.1.0` | 运维按自然年理解平台代次；与节点 SemVer 解耦。 |
| **兼容声明** | 独立矩阵，不塞进版本数字 | Edge `>= 1.2.2` 对接控制面 `2026.1` | 年份无法表达「这个 RPC 还能不能用」。 |

不要用「2026 + 小版本」替代节点 SemVer：一年内会有多次不兼容的协议/行为变化，年份无法表达 breaking，最终还是要另写兼容表。CalVer 适合发版节奏和产品代次，不适合作为 crate 兼容合同。

本仓库后续 tag 继续使用 `v1.x.x`（或将来的 `v2.0.0`），与 `.github/workflows/release.yml` 的 `v*` 触发保持一致。
