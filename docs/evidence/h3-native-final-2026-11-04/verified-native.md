# native: 已核验协议/缓存/连接对比

RPS、P99、建连数与 CPU/request 已从逐传输记录及计数器重算。全部重复通过才展示可比较的中位数；失败和缺失样本仍在 JSON/CSV 中。
P99 为逐轮 P99 的中位数。cgroup 内存峰值包含文件页、tmpfs 和测试控制器，不能当作 Edge 进程 RSS。
预期每格 3 个独立样本。源提交：`562d49c1b1dc2f754acaa33f842bb266911fbcfb`.

| 正文 B | 协议 | 缓存 | 连接 | quinn-shared | quinn-native | tokio-quiche-native |
|---:|---|---|---|---:|---:|---:|
| 1024 | h3 | hit | fresh | 768.95 | 831.95 | 627.37 |
| 1024 | h3 | hit | multiplex | 11,052.01 | 11,251.77 | 8,852.96 |
| 1024 | h3 | hit | reuse | 11,162.33 | 11,904.48 | 7,801.63 |
| 1024 | h3 | miss | fresh | 667.39 | 668.53 | 529.76 |
| 1024 | h3 | miss | multiplex | 3,251.68 | 3,251.43 | 2,805.63 |
| 1024 | h3 | miss | reuse | 2,805.93 | 2,799.13 | 2,200.76 |
| 1024 | h3 | off | fresh | 715.14 | 768.75 | 591.26 |
| 1024 | h3 | off | multiplex | 7,326.32 | 7,619.86 | 5,933.25 |
| 1024 | h3 | off | reuse | 7,006.03 | 7,384.94 | 5,208.18 |
| 32768 | h3 | hit | fresh | 627.05 | 失败（见原始记录） | 590.59 |
| 32768 | h3 | hit | multiplex | 2,866.28 | 3,648.64 | 3,385.07 |
| 32768 | h3 | hit | reuse | 2,517.92 | 4,775.15 | 4,349.63 |
| 32768 | h3 | miss | fresh | 529.52 | 590.92 | 479.77 |
| 32768 | h3 | miss | multiplex | 1,532.70 | 1,982.16 | 1,809.33 |
| 32768 | h3 | miss | reuse | 1,253.23 | 1,810.38 | 1,536.27 |
| 32768 | h3 | off | fresh | 590.35 | 667.96 | 558.26 |
| 32768 | h3 | off | multiplex | 2,379.71 | 3,148.01 | 2,928.22 |
| 32768 | h3 | off | reuse | 2,093.73 | 3,873.76 | 3,223.61 |
| 131072 | h3 | hit | fresh | 387.77 | 558.21 | 477.23 |
| 131072 | h3 | hit | multiplex | 781.70 | 944.70 | 981.81 |
| 131072 | h3 | hit | reuse | 827.39 | 1,268.14 | 1,246.65 |
| 131072 | h3 | miss | fresh | 325.82 | 458.16 | 388.46 |
| 131072 | h3 | miss | multiplex | 546.76 | 746.66 | 795.10 |
| 131072 | h3 | miss | reuse | 532.78 | 956.91 | 873.33 |
| 131072 | h3 | off | fresh | 360.85 | 失败（见原始记录） | 457.65 |
| 131072 | h3 | off | multiplex | 696.15 | 918.18 | 911.33 |
| 131072 | h3 | off | reuse | 692.46 | 1,150.25 | 1,138.37 |

| 正文/协议/缓存/连接 | 后端 | P99 ms | CPU µs/成功请求 | cgroup峰值 MiB | 客户端CPU % | 源站CPU % | 最大核steal % |
|---|---|---:|---:|---:|---:|---:|---:|
| 1024/h3/hit/fresh | quinn-shared | 42.81 | 1,230.55 | 149.84 | 77.37 | 3.50 | 0.00 |
| 1024/h3/hit/fresh | quinn-native | 40.92 | 1,187.79 | 153.09 | 82.40 | 3.76 | 0.00 |
| 1024/h3/hit/fresh | tokio-quiche-native | 56.68 | 1,516.28 | 145.38 | 66.54 | 2.82 | 0.00 |
| 1024/h3/hit/multiplex | quinn-shared | 3.31 | 76.55 | 138.96 | 94.55 | 0.78 | 0.00 |
| 1024/h3/hit/multiplex | quinn-native | 3.46 | 72.71 | 141.05 | 95.14 | 0.76 | 0.00 |
| 1024/h3/hit/multiplex | tokio-quiche-native | 4.54 | 97.70 | 134.90 | 94.62 | 0.77 | 0.00 |
| 1024/h3/hit/reuse | quinn-shared | 3.51 | 81.28 | 145.62 | 95.95 | 0.77 | 0.00 |
| 1024/h3/hit/reuse | quinn-native | 3.28 | 74.33 | 145.02 | 96.56 | 0.76 | 0.00 |
| 1024/h3/hit/reuse | tokio-quiche-native | 5.09 | 115.79 | 137.17 | 93.67 | 0.77 | 0.00 |
| 1024/h3/miss/fresh | quinn-shared | 56.38 | 1,477.28 | 153.47 | 76.15 | 6.27 | 0.00 |
| 1024/h3/miss/fresh | quinn-native | 56.35 | 1,439.04 | 162.14 | 76.82 | 6.41 | 0.00 |
| 1024/h3/miss/fresh | tokio-quiche-native | 71.46 | 1,826.24 | 153.75 | 65.79 | 4.97 | 0.00 |
| 1024/h3/miss/multiplex | quinn-shared | 20.44 | 303.35 | 155.99 | 75.56 | 17.21 | 0.00 |
| 1024/h3/miss/multiplex | quinn-native | 18.43 | 281.43 | 159.43 | 66.55 | 16.89 | 0.00 |
| 1024/h3/miss/multiplex | tokio-quiche-native | 25.14 | 343.58 | 156.09 | 80.40 | 15.75 | 0.00 |
| 1024/h3/miss/reuse | quinn-shared | 46.53 | 347.26 | 155.29 | 69.02 | 15.17 | 0.00 |
| 1024/h3/miss/reuse | quinn-native | 45.95 | 344.54 | 161.02 | 67.78 | 15.03 | 0.00 |
| 1024/h3/miss/reuse | tokio-quiche-native | 57.32 | 446.34 | 156.99 | 82.65 | 12.31 | 0.00 |
| 1024/h3/off/fresh | quinn-shared | 47.71 | 1,283.55 | 146.12 | 77.73 | 6.89 | 0.00 |
| 1024/h3/off/fresh | quinn-native | 47.41 | 1,249.07 | 152.45 | 81.69 | 7.26 | 0.00 |
| 1024/h3/off/fresh | tokio-quiche-native | 61.77 | 1,592.72 | 144.11 | 67.50 | 5.28 | 0.00 |
| 1024/h3/off/multiplex | quinn-shared | 5.22 | 124.39 | 137.31 | 91.67 | 22.90 | 0.00 |
| 1024/h3/off/multiplex | quinn-native | 5.12 | 114.43 | 139.82 | 90.84 | 23.46 | 0.00 |
| 1024/h3/off/multiplex | tokio-quiche-native | 6.31 | 152.69 | 138.54 | 92.38 | 19.64 | 0.00 |
| 1024/h3/off/reuse | quinn-shared | 5.75 | 131.02 | 144.54 | 89.33 | 21.94 | 0.00 |
| 1024/h3/off/reuse | quinn-native | 5.46 | 122.53 | 150.33 | 88.16 | 22.69 | 0.00 |
| 1024/h3/off/reuse | tokio-quiche-native | 7.43 | 176.06 | 139.00 | 89.54 | 16.99 | 0.00 |
| 32768/h3/hit/fresh | quinn-shared | 73.54 | 1,500.73 | 145.27 | 83.67 | 2.82 | 0.00 |
| 32768/h3/hit/fresh | quinn-native | 失败（见原始记录） | 失败（见原始记录） | 失败（见原始记录） | 失败（见原始记录） | 失败（见原始记录） | 失败（见原始记录） |
| 32768/h3/hit/fresh | tokio-quiche-native | 70.87 | 1,644.61 | 145.01 | 72.01 | 2.72 | 0.00 |
| 32768/h3/hit/multiplex | quinn-shared | 14.70 | 332.31 | 139.34 | 94.68 | 0.76 | 0.00 |
| 32768/h3/hit/multiplex | quinn-native | 12.96 | 193.20 | 139.96 | 93.39 | 0.77 | 0.00 |
| 32768/h3/hit/multiplex | tokio-quiche-native | 13.31 | 223.72 | 136.99 | 91.67 | 0.75 | 0.00 |
| 32768/h3/hit/reuse | quinn-shared | 17.00 | 364.60 | 145.21 | 93.46 | 0.77 | 0.00 |
| 32768/h3/hit/reuse | quinn-native | 10.98 | 171.38 | 150.34 | 96.56 | 0.76 | 0.00 |
| 32768/h3/hit/reuse | tokio-quiche-native | 11.65 | 206.53 | 141.45 | 95.91 | 0.76 | 0.00 |
| 32768/h3/miss/fresh | quinn-shared | 84.66 | 1,792.81 | 166.25 | 78.50 | 5.54 | 0.00 |
| 32768/h3/miss/fresh | quinn-native | 63.72 | 1,614.99 | 177.34 | 79.35 | 6.20 | 0.00 |
| 32768/h3/miss/fresh | tokio-quiche-native | 80.28 | 2,028.13 | 169.40 | 69.35 | 4.96 | 0.00 |
| 32768/h3/miss/multiplex | quinn-shared | 37.95 | 616.29 | 185.95 | 83.02 | 9.47 | 0.00 |
| 32768/h3/miss/multiplex | quinn-native | 26.84 | 449.49 | 191.77 | 78.63 | 11.79 | 0.00 |
| 32768/h3/miss/multiplex | tokio-quiche-native | 32.73 | 511.01 | 184.88 | 79.18 | 11.28 | 0.00 |
| 32768/h3/miss/reuse | quinn-shared | 75.45 | 721.53 | 184.44 | 77.66 | 8.68 | 0.00 |
| 32768/h3/miss/reuse | quinn-native | 55.60 | 502.96 | 192.28 | 72.78 | 11.93 | 0.00 |
| 32768/h3/miss/reuse | tokio-quiche-native | 64.89 | 609.64 | 184.03 | 79.97 | 9.65 | 0.00 |
| 32768/h3/off/fresh | quinn-shared | 82.29 | 1,577.05 | 145.75 | 82.15 | 6.29 | 0.00 |
| 32768/h3/off/fresh | quinn-native | 54.73 | 1,374.32 | 154.80 | 82.37 | 6.84 | 0.00 |
| 32768/h3/off/fresh | tokio-quiche-native | 68.45 | 1,762.60 | 146.55 | 73.67 | 5.49 | 0.00 |
| 32768/h3/off/multiplex | quinn-shared | 17.53 | 395.54 | 140.10 | 91.78 | 9.18 | 0.00 |
| 32768/h3/off/multiplex | quinn-native | 14.42 | 248.91 | 142.44 | 89.11 | 11.49 | 0.00 |
| 32768/h3/off/multiplex | tokio-quiche-native | 15.13 | 294.84 | 140.38 | 88.83 | 11.02 | 0.00 |
| 32768/h3/off/reuse | quinn-shared | 20.60 | 441.13 | 144.70 | 90.14 | 9.76 | 0.00 |
| 32768/h3/off/reuse | quinn-native | 14.02 | 227.60 | 151.67 | 91.63 | 14.03 | 0.00 |
| 32768/h3/off/reuse | tokio-quiche-native | 14.28 | 289.20 | 144.82 | 92.49 | 11.59 | 0.00 |
| 131072/h3/hit/fresh | quinn-shared | 136.56 | 2,316.05 | 154.17 | 86.37 | 1.73 | 0.00 |
| 131072/h3/hit/fresh | quinn-native | 68.82 | 1,620.90 | 163.53 | 85.48 | 2.45 | 0.00 |
| 131072/h3/hit/fresh | tokio-quiche-native | 81.59 | 1,981.01 | 149.66 | 77.12 | 2.20 | 0.00 |
| 131072/h3/hit/multiplex | quinn-shared | 358.49 | 1,182.40 | 150.84 | 94.20 | 0.74 | 0.00 |
| 131072/h3/hit/multiplex | quinn-native | 227.41 | 706.34 | 155.01 | 92.31 | 0.76 | 0.00 |
| 131072/h3/hit/multiplex | tokio-quiche-native | 48.14 | 687.50 | 141.43 | 92.16 | 0.76 | 0.00 |
| 131072/h3/hit/reuse | quinn-shared | 198.56 | 1,117.36 | 152.98 | 92.16 | 0.76 | 0.00 |
| 131072/h3/hit/reuse | quinn-native | 80.18 | 548.83 | 160.48 | 89.88 | 0.76 | 0.00 |
| 131072/h3/hit/reuse | tokio-quiche-native | 83.55 | 605.88 | 149.35 | 87.92 | 0.75 | 0.00 |
| 131072/h3/miss/fresh | quinn-shared | 170.97 | 2,841.12 | 153.37 | 81.87 | 4.47 | 0.00 |
| 131072/h3/miss/fresh | quinn-native | 93.03 | 2,087.66 | 165.19 | 81.39 | 5.74 | 0.00 |
| 131072/h3/miss/fresh | tokio-quiche-native | 100.74 | 2,503.45 | 158.81 | 73.39 | 4.83 | 0.00 |
| 131072/h3/miss/multiplex | quinn-shared | 357.24 | 1,721.86 | 163.66 | 91.25 | 5.37 | 0.00 |
| 131072/h3/miss/multiplex | quinn-native | 223.95 | 1,103.48 | 172.43 | 87.33 | 6.80 | 0.00 |
| 131072/h3/miss/multiplex | tokio-quiche-native | 65.30 | 1,165.51 | 160.12 | 84.99 | 7.00 | 0.00 |
| 131072/h3/miss/reuse | quinn-shared | 136.32 | 1,723.72 | 159.61 | 87.29 | 5.45 | 0.00 |
| 131072/h3/miss/reuse | quinn-native | 81.53 | 986.18 | 163.49 | 85.07 | 8.48 | 0.00 |
| 131072/h3/miss/reuse | tokio-quiche-native | 88.49 | 1,109.12 | 167.96 | 81.93 | 7.23 | 0.00 |
| 131072/h3/off/fresh | quinn-shared | 164.38 | 2,453.47 | 144.08 | 83.93 | 4.88 | 0.00 |
| 131072/h3/off/fresh | quinn-native | 失败（见原始记录） | 失败（见原始记录） | 失败（见原始记录） | 失败（见原始记录） | 失败（见原始记录） | 失败（见原始记录） |
| 131072/h3/off/fresh | tokio-quiche-native | 88.15 | 2,129.28 | 151.22 | 78.19 | 5.63 | 0.00 |
| 131072/h3/off/multiplex | quinn-shared | 304.67 | 1,330.07 | 148.10 | 92.99 | 5.61 | 0.00 |
| 131072/h3/off/multiplex | quinn-native | 166.62 | 779.64 | 154.85 | 90.32 | 6.49 | 0.00 |
| 131072/h3/off/multiplex | tokio-quiche-native | 53.57 | 813.13 | 145.66 | 90.77 | 6.38 | 0.00 |
| 131072/h3/off/reuse | quinn-shared | 104.63 | 1,318.50 | 146.42 | 92.83 | 6.05 | 0.00 |
| 131072/h3/off/reuse | quinn-native | 86.82 | 658.20 | 149.69 | 88.28 | 8.47 | 0.00 |
| 131072/h3/off/reuse | tokio-quiche-native | 85.72 | 729.99 | 146.86 | 86.97 | 8.02 | 0.00 |

短时云环境试验；少于 1 秒的限量批次、客户端/源站饱和、宿主机 steal 和跨轮波动需要逐格判断。未完成 24/72 小时稳定性、受控公网 RTT/丢包或业务 Lua/WAF 规则对照。原始失败不能视为 0 QPS。
