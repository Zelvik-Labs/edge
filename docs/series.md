# CloudSidra 产品线

品牌前缀一律 **CloudSidra-**。GitHub 组织 `CloudSidra` 下每个产品一个**私有**仓库，仓库名与展示名相同。Edge 只是其中一块数据面，不是全家桶名字。

| 产品 | 仓库 | 状态 | 职责 |
| --- | --- | --- | --- |
| **CloudSidra-Edge** | `CloudSidra/CloudSidra-Edge` | 本仓库 | 边缘流量数据面（代理、缓存、WAF、日志） |
| **CloudSidra-API** | `CloudSidra/CloudSidra-API` | 待建 | 控制面：节点 gRPC + Admin/User API |
| **CloudSidra-Admin** | `CloudSidra/CloudSidra-Admin` | 待建 | 运营端 |
| **CloudSidra-User** | `CloudSidra/CloudSidra-User` | 待建 | 租户端 |
| **CloudSidra-Portal** | `CloudSidra/CloudSidra-Portal` | 待建 | 宣传门户、文档与受控下载 |
| **CloudSidra-DNS** | `CloudSidra/CloudSidra-DNS` | 规划 | 权威 DNS，与 Edge 并列，不挂在 Edge 名下 |

CLI / systemd / 二进制对外都叫 `CloudSidra-Edge`。默认安装目录 `/opt/CloudSidra-Edge`。私有仓安装走 GitHub Release + token 或 Portal，不用公开 raw 脚本。

版本：各产品 SemVer；套件代次可用 `CloudSidra 2026.1` 兼容矩阵，不把年份写进 Edge crate。
