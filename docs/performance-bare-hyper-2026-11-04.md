# Hyper 与四产品的 HTTP 基础对照

累计改动的 192 格节点对照没有显著改变性能差距。已有裸 Pingora 控制把框架基础与 Edge 业务路径分开，但仍不足以判断更换 HTTP 核心的收益空间。本轮加入最小 Hyper 1.11.0 代理，同时测固定 ba474 Edge、裸 Pingora、nginx 和 OpenResty。

五目标按五轮 Latin 位置平衡，H1/H1-TLS/H2 OFF/连接复用，共 75 格，1KiB、32 并发、每格 5 秒。每格全新进程、源站与 tmpfs，代理/客户端/源站 1/2/1 CPU。构建全部完成后再计时；记录每响应状态/协议/长度、前后正文/TLS、源站请求/连接计数、cgroup CPU、P99、进程 starttime/CPU 与资源瓶颈；另要求 50 项 wrong-CA 控制。

Hyper 使用 root-lock 同版本依赖、mimalloc/rustls AWS-LC、x86-64-v2、fat LTO/单 codegen unit，显式 Tokio worker 数等于代理 CPU 配额，上游池 256/120s idle、15s connect。只保留固定源站转发与 hop-by-hop 头处理，不包含 Edge 缓存/WAF/配置热更新/统计或持久化。这是框架控制，不是功能等价替换。

保存实际新 Hyper 与原裸 Pingora ELF、锁文件/构建器/源码；核对实际运行 Edge/Hyper/Pingora 的 /proc/PID/exe，并保存 nginx/OpenResty 实际主进程 ELF。原 Edge/controller/client 从已证明的固定归档下载，历史 C 客户端源明确记录 a19f6e2。任何失败保留，不重试替换。

状态：准备 GitHub Actions 构建和测量，尚无 Hyper 数值结论。不能提前宣称超过 nginx/OpenResty，也不能把基础库差值当可直接删除的业务成本。

限制：固定小响应/OFF/复用与短窗口。Tokio/HTTP 栈实现、连接服务方式和功能均存在差异；本轮目的为架构方向，不单独隔离某个库函数的因果。源站或客户端饱和的 RPS 不作为代理容量上限；错误路径/大包/长期稳定性及生产能力未覆盖，未替换生产核心、未部署。

经审批的功能降级：无。
