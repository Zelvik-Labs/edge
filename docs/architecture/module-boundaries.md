# Edge 模块边界

> 文档状态：`draft`  
> Owner：Edge Runtime  
> 最后验证：2026-09-04

| 模块 | 职责 |
| --- | --- |
| proxy | 协议接入、连接和转发 |
| cache | key、存储、TTL、回源和 purge |
| firewall | WAF、UAM、CC 和 IP 策略 |
| routing | 位置、权重和 upstream 选择 |
| node_control | desired state、patch、ACK 和事件 |
| memory | 预算、admission、回收和 OOM 保护 |
| metrics | 指标、日志和诊断 |

模块之间通过窄接口协作。NodeControl 负责接收控制面意图，但不得承载 API 的账务、用户权限或 Provider 逻辑。

