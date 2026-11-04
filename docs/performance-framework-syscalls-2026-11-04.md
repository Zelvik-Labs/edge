# 四目标系统调用计数：H1/H2 无缓存复用

[Actions 41802130079](https://github.com/Zelvik-Labs/edge/actions/runs/41802130079) 完成并复核：16 格、332,075 个成功响应、零 HTTP 错误。Edge 固定 ba474，裸 Pingora 固定 3b25，同一历史客户端源 a19f6e2；本轮没有编译或修改被测实现，不代表最新 head 容量。

四目标独立运行 H1/H2 OFF、1KiB、32 并发、每格 10 秒、两轮正反顺序。代理/客户端/源站 1/2/1 CPU；strace 在单独 1 CPU/256MiB、无网络容器，与客户端的一个 CPU 共享。每格核对所有选定进程的实际 ELF 身份、已观察线程 TracerPid 前后覆盖、负载时间位于附加窗口内、主动 SIGINT 后全部线程脱离，逐响应控制和系统调用明细总和均复算。跟踪会改变调度和吞吐，**这里只报告计数，不做性能排名或把 strace 墙钟时间当 CPU 时间。**

## 每有效响应的调用次数

数值为两轮各自“调用次数/有效响应”的中位数；不是未插桩调用率。read/write 和 recvfrom/sendto 是系统调用名，分别不等价于文件 IO/UDP。futex 还未区分 WAIT/WAKE 或调用栈。原始逐格、全部 syscall 名称及错误数保留于摘要。

| 目标/协议 | 所有调用 | futex | recvfrom | sendto | read | write | writev | epoll_wait | getsockopt |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| edge h1 | 8.468 | 1.932 | 2.005 | 3.000 | 0.149 | 0.004 | 0.001 | 0.082 | 0.004 |
| edge h2 | 7.356 | 1.813 | 2.007 | 1.000 | 0.141 | 0.004 | 1.011 | 0.098 | 0.004 |
| nginx h1 | 5.080 | 0.000 | 2.001 | 0.000 | 0.000 | 0.000 | 2.000 | 0.065 | 1.000 |
| nginx h2 | 6.107 | 0.000 | 1.002 | 0.000 | 2.008 | 1.007 | 1.000 | 0.066 | 1.000 |
| openresty h1 | 5.082 | 0.000 | 2.001 | 0.000 | 0.000 | 0.000 | 2.000 | 0.065 | 1.000 |
| openresty h2 | 6.117 | 0.000 | 1.002 | 0.000 | 2.016 | 1.007 | 1.000 | 0.066 | 1.000 |
| pingora h1 | 5.156 | 0.000 | 2.001 | 2.000 | 0.000 | 0.000 | 0.000 | 0.091 | 0.002 |
| pingora h2 | 5.240 | 0.003 | 2.005 | 2.006 | 0.000 | 0.002 | 0.002 | 0.102 | 0.003 |


## 可以定位的差异与待证假设

Edge H1/H2 分别约 1.93/1.81 次 futex/响应，而裸 Pingora 接近零；H1 的 sendto 是 3.00 对 2.00。nginx/OpenResty H1 使用约两次 writev、两次 recvfrom，且约一次 getsockopt。H2 有 TLS/帧处理，不能把各库选择的 syscall 名字直接解释为优劣。

这把后续优先级收敛到两处：futex 的 WAIT/WAKE 来源和线程间调度；额外写调用对应的上游/下游方向及头/正文刷新边界。现有数据没有调用栈或 fd 方向，不能把 futex 直接归因统计队列、内存锁或 runtime，也不能据此宣称每次请求都阻塞。当前源码的维度统计消费者已批量 try_recv，但仍可能跨 runtime 唤醒，须另测后再决定批量通知或线程归属调整。

HTTP Kernel 上游连接从 UpstreamL4Stream 直接拆出 Pingora TcpStream，已核查的该入口并未经过通用枚举的 vectored IO 默认实现，因此不能将这里的差距归咎该包装缺少 write_vectored。裸框架也有基础成本，减少系统调用并不自动消除用户态头处理与分配差距。

系统调用 errors 含非阻塞 EAGAIN 等正常结果，与 HTTP errors 不同。短窗口和观察器额外调度会改变计数/响应关系；没有把这些数据乘以旧 CPU 结果推算可回收耗时。

## 原失败与修复范围

首轮 [41799556407](https://github.com/Zelvik-Labs/edge/actions/runs/41799556407) 完成 Edge/裸 Pingora 四格（74,819 响应），随后 nginx 两格在负载前因 docker exec 无法读取降权 worker 的 /proc/7/exe 失败，剩余未运行；不是四目标通过。原归档 13306289576 和 [失败复核器](evidence/framework-syscalls-2026-11-04/first-failure/verify.py) 保留。修复将 ELF 读取移到已有 CAP_SYS_PTRACE 的观察器，在负载前后核对，未修改服务器权限、worker 用户或业务。

新归档 13307814955：16,965,774 字节、382 文件、SHA-256 `e9242a3ea4e242435953bd4b139a858b57cd68090fd780f4efdc8922b231548e`。保存 nginx/OpenResty 的实际进程 ELF，并与原 Edge/controller/client 和裸 Pingora 下载的实际 ELF 核对。[冻结复核器](evidence/framework-syscalls-2026-11-04/pass/verify.py) 与 [完整计数](evidence/framework-syscalls-2026-11-04/pass/verified-syscalls.json) 可复算。

历史 CPU profile 的客户端 ELF 一直核对正确，但其 manifest 的 client_source_sha256 曾误标当前 harness C 源。本轮明确记录历史源 revision/SHA，并另存 harness_client_source_sha256；旧归档不重写，不能把该旧字段视为历史源证明。

已修复：诊断观察器的 worker 身份读取。仍未解决：业务层 futex/额外写的调用源及整体容量差距。设计限制：只覆盖 H1/H2 OFF/复用、固定旧 Edge、受 ptrace 扰动；不包含 H3、真实 NIC、长期稳定性或未插桩收益。经审批的功能降级：无。
