# native: 已核验协议/缓存/连接对比

RPS、P99、建连数与 CPU/request 已从逐传输记录及计数器重算。全部重复通过才展示可比较的中位数；失败和缺失样本仍在 JSON/CSV 中。
P99 为逐轮 P99 的中位数。cgroup 内存峰值包含文件页、tmpfs 和测试控制器，不能当作 Edge 进程 RSS。
预期每格 3 个独立样本。源提交：`b30f0cfa1cdaa044e295ee50c77ee1d9c09979b6`.

| 正文 B | 协议 | 缓存 | 连接 | quinn-shared | quinn-native | tokio-quiche-native |
|---:|---|---|---|---:|---:|---:|
| 1024 | h3 | hit | fresh | 529.03 | 556.86 | 503.35 |
| 1024 | h3 | hit | multiplex | 7,021.71 | 7,082.28 | 5,503.44 |
| 1024 | h3 | hit | reuse | 6,984.36 | 7,077.63 | 4,717.93 |
| 1024 | h3 | miss | fresh | 438.21 | 437.72 | 456.77 |
| 1024 | h3 | miss | multiplex | 2,193.70 | 2,197.05 | 1,982.93 |
| 1024 | h3 | miss | reuse | 1,980.70 | 1,983.34 | 1,661.65 |
| 1024 | h3 | off | fresh | 479.64 | 502.92 | 479.60 |
| 1024 | h3 | off | multiplex | 4,949.31 | 5,110.01 | 3,966.12 |
| 1024 | h3 | off | reuse | 4,863.99 | 5,015.63 | 3,322.77 |
| 32768 | h3 | hit | fresh | 403.48 | 478.87 | 456.39 |
| 32768 | h3 | hit | multiplex | 1,629.97 | 1,888.35 | 1,752.05 |
| 32768 | h3 | hit | reuse | 1,524.87 | 2,211.85 | 2,087.76 |
| 32768 | h3 | miss | fresh | 336.63 | 401.67 | 402.65 |
| 32768 | h3 | miss | multiplex | 955.98 | 1,247.78 | 1,178.34 |
| 32768 | h3 | miss | reuse | 839.30 | 1,332.70 | 1,113.02 |
| 32768 | h3 | off | fresh | 374.06 | 437.90 | 419.55 |
| 32768 | h3 | off | multiplex | 1,373.04 | 1,698.11 | 1,555.81 |
| 32768 | h3 | off | reuse | 1,285.87 | 1,879.76 | 1,755.44 |
| 131072 | h3 | hit | fresh | 246.61 | 失败（见原始记录） | 358.93 |
| 131072 | h3 | hit | multiplex | 437.09 | 482.32 | 486.86 |
| 131072 | h3 | hit | reuse | 490.41 | 549.32 | 557.74 |
| 131072 | h3 | miss | fresh | 206.87 | 305.36 | 296.49 |
| 131072 | h3 | miss | multiplex | 332.22 | 439.64 | 430.00 |
| 131072 | h3 | miss | reuse | 361.64 | 518.07 | 505.87 |
| 131072 | h3 | off | fresh | 230.14 | 335.95 | 324.50 |
| 131072 | h3 | off | multiplex | 403.94 | 461.69 | 453.15 |
| 131072 | h3 | off | reuse | 446.14 | 528.52 | 512.01 |

| 正文/协议/缓存/连接 | 后端 | P99 ms | CPU µs/成功请求 | cgroup峰值 MiB | 客户端CPU % | 源站CPU % | 最大核steal % |
|---|---|---:|---:|---:|---:|---:|---:|
| 1024/h3/hit/fresh | quinn-shared | 64.39 | 1,884.58 | 146.51 | 65.53 | 3.04 | 0.00 |
| 1024/h3/hit/fresh | quinn-native | 63.02 | 1,818.69 | 152.18 | 67.56 | 3.29 | 0.00 |
| 1024/h3/hit/fresh | tokio-quiche-native | 75.02 | 1,916.43 | 144.54 | 70.14 | 3.01 | 0.00 |
| 1024/h3/hit/multiplex | quinn-shared | 4.89 | 122.45 | 138.11 | 94.88 | 0.96 | 0.00 |
| 1024/h3/hit/multiplex | quinn-native | 5.09 | 119.72 | 139.49 | 94.93 | 0.98 | 0.00 |
| 1024/h3/hit/multiplex | tokio-quiche-native | 6.98 | 158.85 | 136.36 | 94.55 | 1.02 | 0.00 |
| 1024/h3/hit/reuse | quinn-shared | 5.68 | 131.41 | 142.30 | 95.15 | 0.97 | 0.00 |
| 1024/h3/hit/reuse | quinn-native | 5.77 | 127.19 | 144.47 | 95.21 | 0.99 | 0.00 |
| 1024/h3/hit/reuse | tokio-quiche-native | 8.81 | 196.40 | 138.54 | 94.03 | 0.98 | 0.00 |
| 1024/h3/miss/fresh | quinn-shared | 89.04 | 2,271.02 | 152.14 | 66.02 | 6.19 | 0.00 |
| 1024/h3/miss/fresh | quinn-native | 89.80 | 2,191.00 | 156.24 | 65.17 | 6.20 | 0.00 |
| 1024/h3/miss/fresh | tokio-quiche-native | 89.52 | 2,142.31 | 149.27 | 67.57 | 6.35 | 0.00 |
| 1024/h3/miss/multiplex | quinn-shared | 29.31 | 442.36 | 149.72 | 76.79 | 20.50 | 0.00 |
| 1024/h3/miss/multiplex | quinn-native | 27.62 | 428.31 | 159.25 | 72.51 | 20.06 | 0.00 |
| 1024/h3/miss/multiplex | tokio-quiche-native | 31.85 | 497.56 | 153.90 | 84.86 | 19.07 | 0.00 |
| 1024/h3/miss/reuse | quinn-shared | 71.78 | 504.04 | 156.48 | 69.47 | 17.50 | 0.00 |
| 1024/h3/miss/reuse | quinn-native | 72.48 | 506.27 | 161.88 | 67.57 | 17.28 | 0.00 |
| 1024/h3/miss/reuse | tokio-quiche-native | 72.77 | 581.44 | 153.20 | 83.83 | 14.86 | 0.00 |
| 1024/h3/off/fresh | quinn-shared | 75.04 | 2,002.70 | 144.31 | 68.76 | 6.76 | 0.00 |
| 1024/h3/off/fresh | quinn-native | 75.62 | 1,964.47 | 150.93 | 70.03 | 6.83 | 0.00 |
| 1024/h3/off/fresh | tokio-quiche-native | 73.28 | 2,006.95 | 145.11 | 68.50 | 6.48 | 0.00 |
| 1024/h3/off/multiplex | quinn-shared | 7.88 | 183.08 | 137.36 | 91.28 | 32.62 | 0.00 |
| 1024/h3/off/multiplex | quinn-native | 7.64 | 166.76 | 137.04 | 90.42 | 33.21 | 0.00 |
| 1024/h3/off/multiplex | tokio-quiche-native | 9.71 | 229.63 | 134.15 | 91.94 | 27.50 | 0.00 |
| 1024/h3/off/reuse | quinn-shared | 8.66 | 189.38 | 141.65 | 90.25 | 31.26 | 0.00 |
| 1024/h3/off/reuse | quinn-native | 8.26 | 181.52 | 148.27 | 89.26 | 32.34 | 0.00 |
| 1024/h3/off/reuse | tokio-quiche-native | 12.74 | 275.77 | 140.59 | 90.87 | 22.17 | 0.00 |
| 32768/h3/hit/fresh | quinn-shared | 122.52 | 2,371.11 | 143.01 | 74.69 | 2.35 | 0.00 |
| 32768/h3/hit/fresh | quinn-native | 71.30 | 2,028.89 | 151.69 | 74.89 | 2.79 | 0.00 |
| 32768/h3/hit/fresh | tokio-quiche-native | 88.68 | 2,118.31 | 146.85 | 75.67 | 2.77 | 0.00 |
| 32768/h3/hit/multiplex | quinn-shared | 27.35 | 585.29 | 142.09 | 94.22 | 0.99 | 0.00 |
| 32768/h3/hit/multiplex | quinn-native | 26.41 | 358.12 | 143.95 | 89.72 | 0.99 | 0.00 |
| 32768/h3/hit/multiplex | tokio-quiche-native | 27.23 | 407.51 | 138.00 | 89.10 | 0.99 | 0.00 |
| 32768/h3/hit/reuse | quinn-shared | 30.34 | 619.86 | 144.13 | 95.56 | 1.01 | 0.00 |
| 32768/h3/hit/reuse | quinn-native | 47.09 | 308.61 | 148.63 | 95.83 | 0.98 | 0.00 |
| 32768/h3/hit/reuse | tokio-quiche-native | 48.99 | 370.00 | 139.68 | 95.12 | 0.97 | 0.00 |
| 32768/h3/miss/fresh | quinn-shared | 145.90 | 2,868.38 | 169.39 | 72.28 | 5.19 | 0.00 |
| 32768/h3/miss/fresh | quinn-native | 95.09 | 2,379.59 | 171.91 | 73.61 | 6.04 | 0.00 |
| 32768/h3/miss/fresh | tokio-quiche-native | 104.76 | 2,408.10 | 156.12 | 73.03 | 6.10 | 0.00 |
| 32768/h3/miss/multiplex | quinn-shared | 59.83 | 992.17 | 183.90 | 85.44 | 9.92 | 0.00 |
| 32768/h3/miss/multiplex | quinn-native | 42.78 | 697.05 | 187.45 | 78.42 | 12.71 | 0.00 |
| 32768/h3/miss/multiplex | tokio-quiche-native | 48.40 | 802.16 | 185.29 | 82.71 | 12.63 | 0.00 |
| 32768/h3/miss/reuse | quinn-shared | 115.16 | 1,112.50 | 183.64 | 83.02 | 9.00 | 0.00 |
| 32768/h3/miss/reuse | quinn-native | 84.49 | 739.70 | 190.67 | 81.30 | 13.97 | 0.00 |
| 32768/h3/miss/reuse | tokio-quiche-native | 93.30 | 865.39 | 187.03 | 82.68 | 11.11 | 0.00 |
| 32768/h3/off/fresh | quinn-shared | 122.27 | 2,511.87 | 143.00 | 74.46 | 5.64 | 0.00 |
| 32768/h3/off/fresh | quinn-native | 84.72 | 2,139.19 | 155.09 | 73.41 | 6.38 | 0.00 |
| 32768/h3/off/fresh | tokio-quiche-native | 89.73 | 2,262.87 | 148.77 | 73.92 | 6.51 | 0.00 |
| 32768/h3/off/multiplex | quinn-shared | 31.75 | 686.12 | 142.56 | 90.47 | 10.56 | 0.00 |
| 32768/h3/off/multiplex | quinn-native | 28.94 | 429.12 | 141.38 | 87.54 | 12.29 | 0.00 |
| 32768/h3/off/multiplex | tokio-quiche-native | 31.74 | 508.50 | 138.84 | 86.68 | 11.60 | 0.00 |
| 32768/h3/off/reuse | quinn-shared | 59.33 | 735.13 | 145.38 | 93.52 | 10.46 | 0.00 |
| 32768/h3/off/reuse | quinn-native | 54.81 | 396.69 | 149.19 | 92.15 | 14.27 | 0.00 |
| 32768/h3/off/reuse | tokio-quiche-native | 56.50 | 493.64 | 141.54 | 91.10 | 12.80 | 0.00 |
| 131072/h3/hit/fresh | quinn-shared | 217.48 | 3,884.98 | 154.12 | 85.90 | 1.44 | 0.00 |
| 131072/h3/hit/fresh | quinn-native | 失败（见原始记录） | 失败（见原始记录） | 失败（见原始记录） | 失败（见原始记录） | 失败（见原始记录） | 失败（见原始记录） |
| 131072/h3/hit/fresh | tokio-quiche-native | 122.61 | 2,655.00 | 149.22 | 83.69 | 2.04 | 0.00 |
| 131072/h3/hit/multiplex | quinn-shared | 627.41 | 2,127.04 | 149.41 | 91.30 | 0.98 | 0.00 |
| 131072/h3/hit/multiplex | quinn-native | 469.46 | 1,355.06 | 153.62 | 88.71 | 0.95 | 0.00 |
| 131072/h3/hit/multiplex | tokio-quiche-native | 98.06 | 1,329.10 | 142.84 | 88.93 | 0.98 | 0.00 |
| 131072/h3/hit/reuse | quinn-shared | 383.55 | 1,905.39 | 153.59 | 91.10 | 0.96 | 0.00 |
| 131072/h3/hit/reuse | quinn-native | 170.00 | 1,007.89 | 155.08 | 87.20 | 1.00 | 0.00 |
| 131072/h3/hit/reuse | tokio-quiche-native | 184.70 | 1,073.32 | 150.48 | 85.18 | 0.95 | 0.00 |
| 131072/h3/miss/fresh | quinn-shared | 256.94 | 4,507.06 | 155.39 | 80.97 | 4.58 | 0.00 |
| 131072/h3/miss/fresh | quinn-native | 146.76 | 3,078.75 | 164.28 | 80.26 | 5.81 | 0.00 |
| 131072/h3/miss/fresh | tokio-quiche-native | 153.98 | 3,256.09 | 157.18 | 80.62 | 6.22 | 0.00 |
| 131072/h3/miss/multiplex | quinn-shared | 675.18 | 2,813.88 | 162.93 | 90.50 | 5.47 | 0.00 |
| 131072/h3/miss/multiplex | quinn-native | 409.79 | 1,728.45 | 169.45 | 85.28 | 6.69 | 0.00 |
| 131072/h3/miss/multiplex | tokio-quiche-native | 113.02 | 1,905.24 | 161.66 | 84.77 | 7.04 | 0.00 |
| 131072/h3/miss/reuse | quinn-shared | 253.98 | 2,579.18 | 161.04 | 86.83 | 5.40 | 0.00 |
| 131072/h3/miss/reuse | quinn-native | 174.75 | 1,457.04 | 164.82 | 80.42 | 7.60 | 0.00 |
| 131072/h3/miss/reuse | tokio-quiche-native | 173.90 | 1,644.69 | 162.45 | 78.46 | 7.22 | 0.00 |
| 131072/h3/off/fresh | quinn-shared | 238.34 | 4,013.62 | 143.59 | 85.68 | 4.91 | 0.00 |
| 131072/h3/off/fresh | quinn-native | 131.76 | 2,738.01 | 153.65 | 83.57 | 6.20 | 0.00 |
| 131072/h3/off/fresh | tokio-quiche-native | 131.15 | 2,978.47 | 149.96 | 82.87 | 6.70 | 0.00 |
| 131072/h3/off/multiplex | quinn-shared | 575.85 | 2,292.82 | 149.25 | 91.19 | 6.09 | 0.00 |
| 131072/h3/off/multiplex | quinn-native | 503.49 | 1,463.40 | 150.09 | 88.40 | 6.41 | 0.00 |
| 131072/h3/off/multiplex | tokio-quiche-native | 104.97 | 1,497.10 | 144.98 | 88.30 | 6.38 | 0.00 |
| 131072/h3/off/reuse | quinn-shared | 304.35 | 2,098.19 | 143.02 | 90.03 | 6.09 | 0.00 |
| 131072/h3/off/reuse | quinn-native | 170.29 | 1,149.50 | 147.39 | 83.29 | 7.15 | 0.00 |
| 131072/h3/off/reuse | tokio-quiche-native | 189.69 | 1,268.56 | 145.98 | 85.22 | 7.05 | 0.00 |

短时云环境试验；少于 1 秒的限量批次、客户端/源站饱和、宿主机 steal 和跨轮波动需要逐格判断。未完成 24/72 小时稳定性、受控公网 RTT/丢包或业务 Lua/WAF 规则对照。原始失败不能视为 0 QPS。
