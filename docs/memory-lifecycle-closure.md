# Edge 内存生命周期闭环验收

状态：实施中，尚未完成整体验收，不作为生产就绪证明。

基线：`feat/edge-memory-governance`，`d6e24220fdcb79a3def3cb4960fbe5b94fba3ef9`，2026-10-29。
本轮只在本地功能分支修改、验证和提交；不 push、不合并、不发版、不部署生产。
原有 CI、Dockerfile、OrbStack 脚本修改和 `target.external-link` 不属于本次变更。

## A. Mace 0.2.0

根 crate 与可选 xtask 精确依赖 `mace-kv = "=0.2.0"`，两份 lockfile 同步更新。
上游公开 API 将 `get_bucket` 改为 `open_bucket`；生产 Metrics、Telemetry、测试 adapter 和可选 xtask 同步适配。
0.2.0 metadata format 2 不兼容 0.1.x：普通启动不删除或迁移数据库。Metrics/Telemetry 初始化失败必须中止节点启动，不能继续接受无法可靠记录的业务。

上游依据：crates.io `mace-kv 0.2.0` 源码 `CHANGELOG.md`、`docs/migration/0.1.2-0.2.0/README.md`，以及实际代码；上游发布说明不代替本项目实测。
主要变化包括共享 durable WAL、配置化 writer width、inline 默认值/范围调整、GC/checkpoint 竞争和 abort recovery 修复。生产 Metrics 明确使用上游 `MIN_INLINE_SIZE`：该值从 0.1.1 的 4096 降为 0.2.0 的 256 字节；默认 inline 为 1024、最大 4096。此存储布局变化需要通过实际写入/GC 样本验证，不能假设升级前后配置行为完全相同。
本项目不提供 0.1.x → 0.2.0 格式迁移工具；已有 RocksDB 离线导入工具是另一个路径。

### 开发数据重建范围

当前 checkout `data/metrics.mace`：真实路径 `/Users/moying/Documents/Zelvik/edge/data/metrics.mace`，约 86 MiB；非符号链接。
未设置 `ZELVIK_EDGE_HOME`；`ps` 未发现本地 Edge 进程，`lsof +D` 未发现持有该数据库的进程。
当前 checkout `data/` 无 telemetry 目录。`data/cache`、`data/xdp-state.json` 不属于此次旧库清理。
已执行旧格式拒绝验证：`mace-perf --path .../data/metrics.mace --mode reopen`返回1/Corruption，manifest前后SHA256均为`663f5262320844fab523474a83ea2214aab56fe110a9bafaefed37e48c0c62a8`。
随后再次检查lsof无持有进程、精确目录与realpath一致，删除且仅删除此目录；0.2.0重新初始化后同一命令返回`reopen_ok: true`。没有删除cache或XDP状态。
其他 worktree、其他项目和远程历史 fixture 不在此次删除范围内。

### 身份与可靠性

Spool 空库使用新的 UUID queue boot，sequence 从 1 开始，batch ID 是 `boot:sequence`。
正常重启复用 meta 中的 boot。ACK 必须匹配 batch ID、sequence、direct backend 和有效 durable timestamp。
空库重建不能接受旧 boot 的 ACK；这不恢复被明确放弃的开发数据，也不能把未交付数据标记成已交付。
不支持通过回滚旧 meta 快照来恢复序列身份。

### 验证记录

- 第一次 `local-rust.sh check --lib --locked`：失败，发现两处生产 `get_bucket` API 不兼容，已定位并适配；不是通过记录。
- 新增 runtime namespace reopen / aborted transaction / offline checker 测试。
- 新增旧数据代际 ACK 拒绝测试；现有乱序 ACK、quarantine/reopen 和容量测试已包含在下列 34 项测试中。
- `local-rust.sh test --lib --locked metrics::storage::tests:: -- --nocapture`：19 passed，含新 namespace/reopen/abort/checker 测试，1.29 秒测试时间。
- `local-rust.sh test --lib --locked telemetry::tests:: -- --nocapture`：34 passed，0 ignored，5.44 秒，含新代际 ACK、乱序 ACK、quarantine/reopen、容量和部分入队失败。
- `local-rust.sh test --test mace_storage --locked -- --nocapture`：7 passed，0 ignored，0.28秒。子进程在已提交写入后直接process exit，不执行engine析构；父进程重开后读到原值。该测试不等同断电/fsync保证；Metrics仍使用relaxed WAL。
- 新增 checksum 有效的 format-1 metadata 拒绝且不改写 manifest 回归：前两轮 8 项 integration 断言通过，但包装器随后预算超限，均保留为命令失败。最终预算内重跑为 **8 passed / 0 ignored，0.32 秒，包装器退出 0**。
- `local-rust.sh test --lib --locked metrics::mace_backend::tests:: -- --nocapture`：**2 passed / 0 ignored，1.03 秒，包装器退出 0**，覆盖历史 adapter 多 bucket 语义和并发读。
- `local-rust.sh check --bin Zelvik-Edge --locked` 已完成，3 分 59 秒；通过实际节点入口编译验证 fail-fast 调用。可选 importer 验证仍在进行。
- 存储预算监控多次阻止或中止构建（mbx 最高 12.78 GiB > 当前脚本 12 GiB 限额），没有绕过。新增 check target、RocksDB 和 incremental 使占用继续增加。已按预算回收冷 action 对象，保留 target 和 incremental；不是每轮例行清缓存。失败日志完整保留。
- 独立只读审查完成：本次依赖/API/启动/测试差异未发现已核实正确性缺陷；审查未运行测试，不替代执行证据。

### 0.2.0 内存模型复核（配置推导，不是 RSS 实测）

生产仍有 3 个独立 Mace engine。Metrics `sync_on_write=false`，16 个物理 writer ring；Telemetry 两个 lane `sync_on_write=true`，0.2.0 每 lane 的 16 个逻辑 writer group 共享 1 个物理 ring（上游 `src/cc/context.rs:150`）。

| 参数（MiB） | Metrics 小/中/大 tier | Telemetry 每 lane |
|---|---|---|
| WAL ring 数 × 单 ring | 16×1 / 16×2 / 16×4 | 1×1 |
| WAL ring 合计 | 16 / 32 / 64 | 1 |
| shared LRU 目标 | 8 / 16 / 32 | 8 |
| bucket NodeCache 目标 | 8 / 16 / 32 | 8 |
| dirty pool + bounded checkpoint drain burst | 8+1 / 16+2 / 32+4 | 8+1 |
| checkpoint threshold（已在 pool 内，不再相加） | 2 / 4 / 8 | 2 |

三个 engine 的稳态 WAL ring 容量合计为 **18/34/66 MiB**，旧版本相同配置为48/64/96 MiB；约减少30 MiB发生在两个 durable spool 的共享 WAL，Metrics relaxed route 不变。这是当前新建库配置与实现推导；非任意既有持久化 options/route 迁移峰值保证。
WAL ring 分配后 zero 写满；LRU、NodeCache、dirty pool 是目标/门限，存在并发和淘汰延迟，不能当作总 heap 硬上限。

其他内部状态：

- btree-store metadata cache 默认8192个4KiB页，即每 engine 按需最多约32MiB页 payload，未包含全部节点开销。生产 Mace 使用 `BTree::open` 默认选项。
- 每 bucket PageMap 含三个65536项的顶层数组，名义虚拟分配1.5MiB；间接层按 PID 范围扩展，空闲PID的BTreeSet及数组高水位不随单条删除收缩。PID可复用，不能称为每次写入必然泄漏。
- 长读 view/epoch pin 可延后 retired pages 的物理释放；dirty账本扣减不代表epoch立即归还页面。缩短扫描批次比扩大pool更直接。
- data/blob handle 与 stat mask 按条数限制；单个文件的relocation表和mask随文件记录数增长，不能将handle条数当成固定字节上限。
- GC默认周期60秒、会按存活比重写及回收文件；WAL按checkpoint frontier回收。0.2修复了多处GC/checkpoint竞争，但没有取消页表高水位，也不保证删除后RSS回启动值。
- Geo container 明确使用mmap；Mace的ring与主要缓存是heap。btree文件读写/page cache和cgroup file收费需与进程anon分开观测。
- 原MetricsDb disk budget由cache/pool/checkpoint相加得到，不能证明真实磁盘上限；这一归因错误留给资源观测修复切片，不在独立依赖升级中混入容量策略。

以上源码依据为上游0.2.0的`src/cc/context.rs`、`src/cc/log.rs`、`src/map/flow.rs`、`src/map/table.rs`、`src/map/data.rs`、`src/meta/stat.rs`、`src/store/gc.rs`，以及btree-store 2.1.0的`src/lib.rs`/`src/node.rs`。最终必须用运行样本验证，当前不宣称整体高水位已经改善。

## B. 长期结构

下表记录当前源码中的生命周期边界；“待修”不表示已经通过验收。后续切片补充实际修复和测试。

| 结构 / owner | 源码入口 | 增长 / 持久化关系 | 当前释放与故障边界 | 状态 |
|---|---|---|---|---|
| Bandwidth active / pending，reporter task | `src/rpc/stats.rs:363`、`:639` | 2 秒采样、5 分钟追加；整个 active + backlog 反复 JSON 写入 core state | enqueue 失败保留全部窗口，重启一次加载；heap 与 state 均无历史总量约束 | 待修 |
| ServerMetrics live / retired，NodeMetrics | `src/metrics.rs:1093`、`:1285` | 请求创建 Arc；退役等待三消费者最终 fingerprint | retired cap 4096；满后旧实例留在 live，deferred/config backlog 无联合上界 | 待修 |
| Flusher Weak baseline | `src/metrics.rs:1713` | 每实例快照，写入成功才推进 | Weak 不持有实例；清理错误与写入结果合并会使成功增量重放 | 待修 |
| Metrics history，MetricStorage | `src/metrics/storage.rs:386`、`:448` | 每 5 分钟每站点一行；保留 1 天 | 每小时最多 1024 删除，100 站点新增已超过清理能力；cursor 从 prefix 重扫 | 待修 |
| MetricAggregator，metrics worker / reporter | `src/metrics/aggregator.rs:226`、`:400` | 动态属性与 samples 有估算字节限制 | flush 即退款但调用方仍持有对象；无 item 提前 continue；所有 item 上传同时展开 | 待修 |
| WAF Token，verifier / MetricStorage | `src/metrics/storage.rs:1110` | 挑战签发写 WAFTOK，含 exp | 验证检查有效期；持久化过期行无周期回收 | 待修 |
| Client-Agent verified IP，client_agent | `src/client_agent.rs:126` | 已验证身份落盘、启动全量载入 | 协议未证明可任意过期或淘汰；不能通过缩短豁免消除增长 | 待决策 |
| Cache tombstone，MetricStorage | `src/metrics/storage.rs:91` | CTOMB 保留删除版本屏障，hot index 有限 | 无远端安全重放水位前不能按 TTL 删除永久屏障 | 待决策 |
| Telemetry spool / loss journal，Spool | `src/telemetry.rs:501`、`:865` | queue 有编码及磁盘容量；loss marker 例外 | 数据只在匹配 durable receipt 后回收；loss 例外需单独闭环 | 待修 |
| Config materialization，config_apply | `src/config_apply.rs:277`、`:389` | ConfigPublish 在建图前 commit | 在实际分配前可能被采样结算；返回 maps 后仍有 Arc owner | 待受控时序测试及修复 |
| L1 / L2，HybridStorage | `src/cache_hybrid.rs:3156`、`:3194` | L1 预算变更重建；L2 streaming Zstd | 小预算变化有 hysteresis；旧 Arc 与 decoder workspace 要实测持有 | 待修及验收 |

上表行号以本轮切片开始时源码为准；最终报告需按最终源码更新。Mace 页表高水位、allocator retained pages、容器 capacity 和仍存活对象须分别报告。

## C. 计费交付约束

初步契约调查：接收端以 `(node,batchId)` 和完整 envelope hash 去重；计费行按 server/node/window/source boot/assignment 取最高 revision。
因此可按不同 server 的完整窗口行分块，不可将同一 server 的增量拆成多批并假设接收端求和。
分块持久化交接与 pending 游标必须原子；窗口 complete 标记必须等待全部数据块真实 ACK，不能只以最后一块入队成功代替。
有限磁盘、无限断网和无限计费输入不能同时保证无损和无限服务；安全策略须明确保留在途工作记录容量以及后续新工作背压。仅拒绝新连接并不能封住已有无限长连接持续产生未来窗口的增长：如果 Mace 完全不可写，必须在记录空间耗尽前暂停相关转发，或取得明确的计费覆盖降级策略；不能任意断开已有连接，也不能静默丢弃。

Client-Agent 契约复核：`control-plane/internal/gcloud/services_contract.go:294` 按递增 id 分页追加，`:322` 创建新验证记录；Edge `src/client_agent.rs:126` 启动全量恢复，`:155` 增量追加，没有撤销、expiry 或全量代际替换字段。因此当前不能证明删除有效身份安全。后续可选策略是定义撤销/有效期协议，或采用异步存储查询与有界热索引并明确未命中验证行为；二者均需确定业务契约，当前保留有效身份。

## D. 历史缺陷闭环

实施中，逐项补充代码和负向对照；不把此前静态审计当作当前修复证据。

## E. 测试与性能

逻辑 30 天、最终 cgroup 六组、实际 soak、配对性能和最终全量门禁尚未执行。
既有 d6e2422 验收不能代表升级后结果。

## F. 生产就绪

目前不能证明 7/30 天稳定；不构成生产升级依据。以下是实施中状态，不是最终验收结论。

1. **是否仍有随时间无限增长的 Rust heap？** 有：Bandwidth 历史 pending，以及 consumer 失败叠加配置 churn 时整个 ServerMetrics family；Client-Agent 历史有效身份也没有已证明的容量边界。
2. **是否仍有无清理边界的数据库历史？** 有：WAFTOK 过期行、loss journal 容量例外、未具备安全回收水位的 CTOMB 和有效身份。Metrics retention 目前在 100 站点规模已可能落后。
3. **哪些有界结构可能超过低内存节点预算？** Mace 的三个 engine、metadata cache、epoch retired pages，叠加 aggregator、L1/L2 和解码/配置代际峰值；表格中的局部目标不能相加当作严格总上限。
4. **0.2.0 是否改善长期高水位？** 源码推导显示两个 durable lane 的物理 WAL ring 名义减少 30 MiB，上游有 GC 修复；尚无本轮持续运行证据证明总高水位改善。PageMap 高水位仍在。
5. **能否证明 7/30 天后稳定？** 不能。逻辑时间模型、实际 soak、最终 cgroup 和最终性能对照均未完成。
6. **哪些故障仍可能 OOM？** 无界历史积压、永久身份增长、解码/配置/多 item 构造峰值，以及快于治理采样的不可回收物理注入。既有压力报告已经保留快速注入失败证据。
7. **生产前必须修复什么？** 计费 pending 持久化交接与容量故障策略、联合退役边界、retention 吞吐和其 baseline 正确性、其他已证实无界路径及预留时序；完成相应故障测试和最终协议验收。
8. **哪些可以后续优化？** 已有明确容量且通过生命周期验收后的 allocator 回收调优、Mace 可复用高水位与缓存命中率；不能把尚无边界的数据生命周期归为单纯性能优化。

在阶段修复和最终真实验证完成前，所有未完成测试、未决业务语义和已知增长路径仍属明确限制。
