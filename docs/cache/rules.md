# Edge 缓存规则与实现原理

> 文档状态：`draft`  
> Owner：Edge Cache  
> 最后验证：2026-09-04

## 实现组成

当前缓存由 [`src/cache_manager.rs`](../../src/cache_manager.rs) 和 [`src/cache_hybrid.rs`](../../src/cache_hybrid.rs) 提供管理与混合存储能力，包含内存层、文件层、内存治理、Bloom/元数据和 purge 操作。

```mermaid
flowchart LR
  request[Request] --> normalize[Normalize_Cache_Key]
  normalize --> l1[L1_Memory]
  l1 -->|miss| l2[L2_File]
  l2 -->|miss| origin[Origin]
  origin --> fill[L2_Write_And_L1_Admission]
  fill --> response[Response]
```

## 强制规则

### Key

Key 必须由规范化后的 scheme、host、port、path、query、Vary 维度和配置版本组成。不同安装实例、站点和配置版本不得共享 key。任何 key 变更必须有兼容/清理策略。

### 命中与回源

1. 先检查 L1；命中且未过期时直接返回。
2. L1 miss 后检查 L2 文件存储。
3. 两层 miss 才能回源。
4. 回源成功后按内存预算写入 L2，并依据对象大小和 admission class 决定是否进入 L1。
5. 回源失败只有在响应明确允许 `stale-if-error` 时才能返回 stale。

### 淘汰与资源治理

- L1 必须受内存 governor 和单对象上限限制。
- L2 写入必须受磁盘可用空间、临时文件和原子 rename 约束。
- purge 必须支持精确 key、前缀和配置版本。
- 热 key 保护使用请求合并/锁，禁止无限等待。
- `Authorization`、私密 Cookie、敏感响应和不可共享的 Range 响应默认不进入共享缓存。

### 观测

每次请求至少记录 hit/miss、层级、回源、stale、purge、淘汰原因、对象大小和配置版本。禁止记录完整 Cookie、Authorization 或缓存敏感 payload。

