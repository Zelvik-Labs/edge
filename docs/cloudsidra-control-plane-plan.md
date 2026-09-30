# CloudSidra-Edge 控制面集成完整计划

## 目标

把现有 CloudSidra-Edge 发展为 CloudSidra 全局流量管理系统的稳定数据面：API 负责事实配置、发布、计费和全局策略；Edge 负责高性能请求处理、缓存/WAF、节点本地安全和执行 API 签发的配置/配额。

## 不变能力

HTTP/1.1、HTTP/2、HTTP/3、gRPC、WebSocket、TCP、TCP-TLS、UDP、共享端口、XDP/AF_XDP、缓存、WAF/UAM/CC、L4 防护、内容处理、热更新、日志统计、资源治理和 RKE2 多副本能力不得因控制面改造回归。

## 目标状态

### 1. 配置状态

Edge 保存三类本地状态：

- active snapshot：当前原子生效配置；
- base metadata：release ID、config version、digest、API protocol；
- patch journal：已应用的资源 patch 序列和确认窗口。

API 产生的 patch 必须包含 operation、resource kind、resource ID、field path/value、base version、patch sequence、digest、signature、capability requirements 和 expiration。Edge 对每条 patch 先验证签名/版本/能力，再在临时状态中校验，最后原子切换。

### 2. 同步状态机

状态为 disconnected、registered、catching_up、ready、applying、degraded。

- registered：注册能力和当前版本。
- catching_up：按序申请 patch 或完整快照。
- ready：正常接受资源级 patch。
- applying：在临时状态校验并切换。
- degraded：保留旧配置服务，报告失败原因和缺口。

完整快照触发条件：首次注册、节点离线恢复、patch 缺口、digest 不一致、协议不兼容、API 明确要求。正常状态禁止把完整快照作为每次变更传输方式。

### 3. 发布和回滚

Edge 只报告 prepare、validate、switch、ack 四类阶段结果。API 回滚时发送一个新的目标 release；Edge 不根据“反向 patch”猜测回滚。旧 active snapshot 在确认窗口内保留，切换失败自动恢复旧指针。

### 4. 日志和计量

日志使用有界内存队列和有界磁盘队列，批量上传并携带 node ID、site ID、user ID（可空）、release ID、sequence、时间、字节、状态码、缓存命中、WAF 动作和连接信息。API 去重后写 ClickHouse。

自建 Edge 的计费只基于 Edge 事件。管理员创建且未绑定用户的站点仍完整记录，但不生成用户账单。ClickHouse 不在请求热路径同步调用。

### 5. 本地配额租约

API 为用户/站点/节点签发带版本和过期时间的配额租约。Edge 本地按字节计数，在租约耗尽时执行 API 指定动作（继续、限制或停止），并把使用量和租约序列上报。租约失联时遵循最后有效策略和最大宽限时间，不自行创造账务规则。

### 6. 通信和信任

保留 node ID + secret 兼容注册；管理员可配置 plaintext、TLS 或 mTLS。新增证书信任时支持 bootstrap、指纹、轮换、吊销和旧 secret 过期指标。release snapshot、patch、配额租约都校验 API 签名。

## 交付顺序和验收

### E-01 基线记录

- 记录 upstream project/version/commit、当前产品版本、构建来源和迁移说明：见 [`ORIGIN.md`](../ORIGIN.md)、[`VERSION-MANIFEST.toml`](../VERSION-MANIFEST.toml)、[`docs/versioning.md`](versioning.md)。
- 规划 NodeControl 与现有 proto 对照：[`docs/node-control-mapping.md`](node-control-mapping.md)（交叉链接 [CloudSidra-Docs `03-api-contracts.md`](../../CloudSidra-Docs/docs/03-api-contracts.md)）。
- 历史 `configs/api_node.yaml`、gRPC、日志格式的 fixture **路径说明**（脱敏示例与单测内联 YAML，不提交真实 secret）：见对照文档「Fixture 路径」；完整 patch 状态机测试目录留待 E-03。
- 验收：旧配置可 `cargo check`；版本来源可追溯。本阶段不实现完整 patch 状态机。

### E-02 配置模型隔离

- 把规范化配置、active snapshot、base metadata 和 patch journal 分离。
- 增加 digest、schema version、capability requirements。
- 验收：相同配置产生相同 digest；旧快照可恢复；非法 schema 不触碰 active。

### E-03 增量 patch 接收器

- 支持资源 create/update/delete、按序应用、幂等、缺口检测和完整快照请求。
- 验收：重复/乱序/缺口/版本冲突/签名错误均有稳定结果；旧配置继续服务。

### E-04 原子应用和回滚

- 实现 prepare/validate/switch/report，切换只移动原子指针。
- 验收：应用失败 100% 保留旧配置；重启恢复最后成功快照；回滚不执行反向推断。

### E-05 API 通信模式

- 兼容 node ID + secret；增加 TLS/mTLS 配置、证书轮换和撤销状态。
- 验收：三种模式分别可配置、可观测、可测试；secret 不出日志。

### E-06 日志背压和计量

- 保留有界队列、批量压缩、sequence、缺口和重试；增加计费字段。
- 验收：API/ClickHouse 断开不阻塞数据面；队列满时有明确丢弃/降级计数；重复事件不重复入账。

### E-07 本地配额租约

- 设计租约签名、版本、过期、节点/站点范围和动作；实现本地计数与上报。
- 验收：延迟日志不导致无界超额；租约过期行为明确；重启恢复计数策略可验证。

### E-08 部署和升级

- systemd、Docker、RKE2/K3s 统一配置/探针/目录；保留上一版本和 active snapshot。
- 验收：滚动升级、节点断联、磁盘满、证书轮换、回滚和恢复演练通过。

### E-09 性能和安全

- 运行 HTTP/3、TCP/UDP、缓存、WAF、XDP、连接、patch、日志和配额压力测试。
- 验收：控制面改造不增加请求热路径同步依赖；内存、FD、NIC、丢包、p99 延迟和队列上限均有指标。

