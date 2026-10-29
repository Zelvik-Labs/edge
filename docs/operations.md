# 部署与运维

本文件说明 Zelvik Edge 的构建、安装、运行、监控和排障建议。

## 构建

GitHub Release workflow 会在 `v*` tag 上执行正式发布构建。`main` 分支只在 `.cargo/`、`pingora-main/`、`toa-main/`、`toa-sender/` 和 `vendor/` 这些基础框架或底层依赖变化时预热 release 缓存；普通业务代码、`build.rs`、Cargo 清单、proto 和测试变更不会触发 main 预热。

普通构建：

```bash
cargo clean
cargo build --release
```

针对 x86_64 v3 优化：

```bash
cargo clean
RUSTFLAGS="-C target-cpu=x86-64-v3 -C opt-level=3 -C lto=fat" cargo build --release
```

针对 ARM64 Neoverse N1 优化：

```bash
cargo clean
RUSTFLAGS="-C target-cpu=neoverse-n1 -C opt-level=3 -C lto=fat" cargo build --release
```

不要在不支持对应指令集的机器上运行高阶 CPU 构建产物。

XDP/AF_XDP eBPF 对象在 Linux 目标构建时由 `build.rs` 自动编译并**内嵌进主二进制**（需要 nightly + rust-src + bpf-linker；缺少工具链时回退嵌入 `data/zelvik-edge-xdp-ebpf.o` 并输出告警）。二进制与 eBPF 程序因此永远同版本，部署只需一个二进制文件。

也可以用 `cargo xtask build-ebpf` 单独构建到 `data/zelvik-edge-xdp-ebpf.o`，用于调试或通过 `xdp.ebpfObject` / `ZELVIK_EDGE_XDP_EBPF_OBJECT_PATH` 显式指定外部对象做热替换。

## 安装

安装命令：

```bash
sudo ./target/release/zelvik-edge install
```

安装会执行两件事：

- 写入 `/usr/bin/zelvik-edge` wrapper。
- 写入 `/etc/systemd/system/zelvik-edge.service` 并启用服务。

安装后的常用命令：

```bash
zelvik-edge start
zelvik-edge stop
zelvik-edge restart
zelvik-edge status
zelvik-edge upgrade
zelvik-edge ntp
zelvik-edge xdp status
zelvik-edge xdp doctor
```

systemd unit 会以 `Type=simple` 托管前台节点进程，用于开机自启、日志和状态排障。已注册 systemd unit 时，`zelvik-edge start/restart` 会优先委托给 `systemctl`；未注册 systemd 时才使用内置后台进程管理。节点自身会同时把运行日志追加到 `logs/run.log`。

XDP/AF_XDP 默认启用，接口和队列由代码自动推导。部署前确认服务具备 root 或 `CAP_BPF`、`CAP_NET_ADMIN`、`CAP_NET_RAW`，建议先执行：

```bash
zelvik-edge xdp doctor
zelvik-edge xdp status
```

如需关闭，设置 `ZELVIK_EDGE_XDP=0` 或在 `configs/api_node.yaml` 中写 `xdp.enabled: false`（文件为最终裁决）。如果生产环境要求 XDP 不可用时拒绝启动，把 `xdp.fallback` 设置为 `fail-start`；默认 `pass` 会回退到原监听器并记录原因。

## 内置 NTP 命令

已安装 Rust 版后，可以用内置 NTP 命令交互式设置系统时区并校准系统时钟：

```bash
sudo zelvik-edge ntp
```

非交互设置时区并校时：

```bash
sudo zelvik-edge ntp --timezone Asia/Hong_Kong --yes
```

只校时、不修改时区：

```bash
sudo zelvik-edge ntp --no-timezone --yes
```

守护进程启动后的自动 NTP 同步只修正程序内部时间偏移，并通过节点日志上报偏差；显式执行 `zelvik-edge ntp` 才会修改系统时钟。

如果服务器或机房禁用了 UDP/123，命令会在内置 NTP 源全部超时后自动回退到 HTTPS Date 时间源，继续通过 TCP/443 获取时间参考。

## 内置升级命令

已安装 Rust 版后，可以直接使用二进制内置升级命令从授权站拉取最新版。先在授权站生成更新密钥（`zvu_` 后跟 64 位小写十六进制），用隐藏输入提供，避免密钥进入 shell 历史：

```bash
read -rs ZELVIK_UPDATE_KEY
export ZELVIK_UPDATE_KEY
sudo --preserve-env=ZELVIK_UPDATE_KEY zelvik-edge upgrade
```

CLI 同时支持 `--update-key`；推荐环境变量，避免命令行参数暴露密钥。密钥仅通过 `Authorization: Bearer` 请求头发送，不放入 URL，也不会打印或写入节点配置。下载使用 HTTPS 并拒绝重定向。

默认行为：

- 自动选择当前 CPU/架构匹配的官方 release 包。
- 默认目标版本为 `latest`。
- 显示升级摘要并交互确认。
- 替换前把旧二进制备份到 `/var/backups/zelvik/edge-upgrade/`。
- 替换成功后，如果 `zelvik-edge.service` 正在运行则重启 systemd 服务；未使用 systemd 时会重启内置后台进程。

非交互升级到最新版：

```bash
sudo --preserve-env=ZELVIK_UPDATE_KEY zelvik-edge upgrade --yes
```

升级到指定版本（规范 CalVer，不加 `v` 前缀）：

```bash
sudo --preserve-env=ZELVIK_UPDATE_KEY zelvik-edge upgrade --version 26.10.2 --yes
```

授权站使用独立域名时，可以指定 HTTPS 基础地址（只允许 origin，不含路径、查询参数或凭据）：

```bash
sudo --preserve-env=ZELVIK_UPDATE_KEY zelvik-edge upgrade --base-url https://license.example.com --yes
```

上线前查看升级计划：

```bash
sudo --preserve-env=ZELVIK_UPDATE_KEY zelvik-edge upgrade --dry-run
```

只替换二进制，不立即重启：

```bash
sudo --preserve-env=ZELVIK_UPDATE_KEY zelvik-edge upgrade --yes --no-restart
```

管理端下发的节点升级任务从 API 的 `/v1/edge/packages/meta` 和 `/v1/edge/packages/download/...` 获取安装包；两个请求各自使用现有 `instanceId`/`secret` 铸造短期 `zvc1` 凭据，发送 suite 定义的四个 `zelvik-*` 身份头。节点不接收授权站更新密钥。HTTP 客户端复用 `commMode` 与 `rpc.tls` 的 CA、客户端证书和 `serverName`，TLS/mTLS 模式要求 HTTPS；元数据只能指向同一更新 origin 的包下载路径，所有重定向均被拒绝。

使用完成后清理当前 shell 中的密钥：

```bash
unset ZELVIK_UPDATE_KEY
```

## 安装

### 一键安装命令

节点由控制面 bootstrap 分发安装。控制台在节点注册页面会给出完整的可复制命令：

```bash
curl -fsSL https://cp.example.com/install/edge.sh | sudo bash -s -- \
    --endpoint https://rpc1.example.com:29090 \
    --artifact-base https://cp.example.com \
    --node-id <节点 ID> \
    --secret <注册凭据> \
    --install-dir /opt/zelvik/edge
```

两个地址不能混用：`--endpoint` 是控制面 **gRPC 控制端点**（平台设置「节点接入端点」里的 **RPC 端点**），installer 会把它写进 `api_node.yaml` 的 `rpc.endpoints`；`--artifact-base` 是 bootstrap 取包用的 HTTP(S) 地址（同一处的 **更新地址**），`install.sh` 看不到它。

bootstrap 脚本从控制面的 `/v1/edge/packages/meta` 接口取包元数据，下载对应架构的 tar 包，校验 sha256 后解包并执行包内的 `install.sh`。

### 参数说明

`install.sh` 接受 `--flag value` 和 `--flag=value` 两种形式：

| 参数 | 说明 |
|---|---|
| `--endpoint <url>` | 控制面 **gRPC 控制端点**（平台「节点接入端点」的 RPC 端点），可逗号分隔多个；落到 `api_node.yaml` 的 `rpc.endpoints` |
| `--node-id <id>` | 节点身份（也接受 `--instance-id`） |
| `--secret <hex>` | 注册时下发的一次性凭据 |
| `--comm-mode <plaintext\|tls\|mtls>` | 通信模式，默认 `tls` |
| `--tls-ca-file <path>` | 控制面 CA（X.509）。安装器复制到 `<home>/pki/cp-ca.pem`（0644 root）并写入 `rpc.tls.caFile`，**不引用临时路径** |
| `--tls-ca-sha256 <hex>` | 上述证书 DER 编码的 SHA-256（`openssl x509 -outform DER \| sha256sum`）；提供时安装前校验，不匹配立即失败 |
| `--tls-cert-file` / `--tls-key-file` | `--comm-mode mtls` 时的客户端证书材料路径 |
| `--tls-server-name <name>` | 覆盖 TLS SNI |
| `--install-dir <path>` | **组件 home**（不是 FHS staging 前缀），默认 `/opt/zelvik/edge`；重跑时省略则沿用现有 unit 探测到的 home |
| `--allow-unregistered` | 离线安装：显式跳过“安装后必须已注册”的校验（节点运行期仍持续重试注册） |
| `--register-timeout <seconds>` | 等待首次注册成功的秒数，默认 60 |
| `--dry-run` | 只打印动作，不写盘、不启服务 |

`--dir` 已删除：它表示“文件系统 staging 前缀（伪根）”，与组件 home 语义冲突。传入会直接报错并提示使用 `--install-dir`，不保留别名。

环境变量兜底：`ZELVIK_ENDPOINT`/`API_ENDPOINTS`、`ZELVIK_NODE_ID`/`ZELVIK_INSTANCE_ID`、`ZELVIK_NODE_SECRET`/`ZELVIK_INSTANCE_SECRET`。

Edge `26.10.4.7` 起移除 Geo 公钥参数。通过控制面安装前先升级 API 至 `26.10.4.19` 或更高版本；旧 API 的 bootstrap 会传递已删除的 `--trust-keys` 并导致安装失败。

### 安装布局

```
/opt/zelvik/edge/bin/zelvik-edge                        二进制（root:root 0755）
/opt/zelvik/edge/configs/api_node.yaml                  节点配置与凭据（0640 root:zelvik-edge）
/opt/zelvik/edge/pki/cp-ca.pem                          控制面 CA（0644 root；rpc.tls.caFile 指向它）
/opt/zelvik/edge/data/                                  服务用户数据：pid、缓存、data/geo、控制面 runtime override（zelvik-edge:zelvik-edge 0750）
/opt/zelvik/edge/logs/                                  日志目录（zelvik-edge:zelvik-edge 0750）
/opt/zelvik/edge/backups/                               root-only 备份（root:root 0700）
/etc/systemd/system/zelvik-edge.service                 systemd unit（ExecStart 指向 <home>/bin/zelvik-edge）
/usr/local/bin/zelvik-edge                              wrapper：export ZELVIK_EDGE_HOME=<home> 后 exec <home>/bin/zelvik-edge
```

权限边界：`bin/`、`configs/`、`pki/`、`backups/` 归 root 控制，服务用户只读；`data/` 与 `logs/` 归服务用户，服务只写这两处。原先把 `data/` 的创建挂在“tar 里带 eBPF 对象”条件下，导致不带 `data/*.o` 的包安装后 `data/` 仍是 root 属主，节点写 pid 文件 EACCES 并反复重启；现在 `data/` 无条件创建并 chown 给服务用户，eBPF 对象本身仍是 root 0644。

节点把 `configs/`、`data/`、`logs/` 都解析在 home 之下，凭据只从 `configs/api_node.yaml` 读——这是它唯一读取的配置文件。安装脚本不再写 `secrets.env` 或 `runtime.yaml`：二进制里没有任何代码读它们，写了等于让节点看起来配好了却起不来。

安装幂等：重复执行只更新二进制与配置，不破坏数据目录。

### 安装后校验

安装器不再以 `systemctl restart` 的返回码作为成功依据。重启后会依次校验：

1. `systemctl is-active` 在连续 15 秒窗口内保持 active；
2. `systemctl show -p MainPID --value zelvik-edge.service` 非 0；
3. `<home>/bin/zelvik-edge --version` 与包内 `VERSION` 一致；
4. 控制面接受注册（journal 中出现 `registered on the control channel`），等待上限 `--register-timeout`。

任一项失败都会打印 `journalctl -u zelvik-edge -n 60` 并以非零退出。纯离线环境必须显式传 `--allow-unregistered`，否则安装按失败处理（节点运行期仍会持续重试注册）。

核对安装结果：

```bash
systemctl show -p ActiveState -p MainPID -p ExecStart -p Environment -p WorkingDirectory zelvik-edge.service
stat -c '%n %U:%G %a' /opt/zelvik/edge/{bin,configs,data,logs,backups}
cd /tmp && /usr/local/bin/zelvik-edge status
```

### dry-run

上线前验证安装步骤而不实际写盘：

```bash
curl -fsSL https://cp.example.com/install/edge.sh | sudo bash -s -- \
    --endpoint https://rpc1.example.com:29090 \
    --artifact-base https://cp.example.com \
    --node-id <节点 ID> \
    --secret <注册凭据> \
    --install-dir /opt/zelvik/edge \
    --dry-run
```

### 服务管理

```bash
systemctl status zelvik-edge
systemctl restart zelvik-edge
journalctl -u zelvik-edge -f
```

### 卸载

```bash
systemctl disable --now zelvik-edge
rm -f /usr/local/bin/zelvik-edge /etc/systemd/system/zelvik-edge.service
# 凭据与 CA（保留则会随目录一起被删除）：
rm -f /opt/zelvik/edge/configs/api_node.yaml /opt/zelvik/edge/pki/cp-ca.pem
# 如需同时清除缓存、状态、日志与升级备份（不可恢复）：
# rm -rf /opt/zelvik/edge/data /opt/zelvik/edge/logs /opt/zelvik/edge/backups
# 彻底删除 home（含二进制）：
# rm -rf /opt/zelvik/edge
# userdel zelvik-edge
systemctl daemon-reload
```

升级备份在 `<home>/backups/`（root:root 0700），卸载时不会自动删除；确认不再需要回滚后再手工清理。旧版本安装在 `/var/lib/zelvik/edge` 时，安装器重跑会沿用该 home（不自动搬数据）；要改用 `/opt/zelvik/edge` 必须按“停服务 → 移动 home → 改写 unit → 重跑安装器”的显式迁移流程执行。

## 1.1.2 说明

1.1.2 聚焦 WAF 正确性、HTTP/3 可用性和 RKE2 多副本部署：修复内置 WAF preset 漏匹配、缓存命中绕过 WAF、chunked/无 Content-Length 请求体 WAF 失效、IP 名单同步版本推进错误，并补齐 RKE2 内部服务、Leader 选举、Longhorn 缓存分片和健康探针。

本版本同时优化 Basic Auth、rewrite replacement、模板渲染、访问日志上传和防火墙网络名单匹配热路径；完整更新见 [1.1.2 发布说明](../release-notes/v1.1.2.md)。

## 1.1.1 说明

1.1.1 聚焦数据面性能和可靠性：缓存 L2 大对象命中改为文件分块读取，压缩对象支持流式 zstd 解压，小对象继续走内存命中以保持 Fast L1 晋升；WAF CC 计数器改为固定窗口滚动桶，避免攻击流量下按请求数线性扫描时间戳。

本版本同时优化访问日志、指标、UDP/QUIC 活动时间、上游 TLS 连接器和证书选择热路径；访问日志上传在中间 chunk 失败时会重新入队当前失败 chunk 与尚未发送的后续 chunk，避免批量日志丢失。

## 1.1.0 说明

1.1.0 新增 `@quic` UDP 域名透传：节点会解析 QUIC Initial 中的 TLS SNI，并在同一 UDP 端口上和 HTTP/3 共享监听；同时修复配置任务完成闭环，避免控制面任务未确认时本地跳过版本导致节点状态停留在同步中。

本版本重新整理 systemd 生命周期管理：新安装的服务使用 `Type=simple` 直接托管前台进程；`stop` 会等待真实退出并在必要时强制结束；`restart` 不再固定等待 1 秒，而是确认旧进程退出后再启动。

GeoIP 不再由安装包或节点本地文件提供。节点通过 `zelvik.asset.v1.AssetControl`
拉取控制面发布的整代 bundle，核对 manifest 身份、SHA-256 和制品大小后
原子安装到 `data/geo/`，节点无需额外的 Geo 验签公钥。City 是必需组件，ASN/Country 可选；
旧版 `data/GeoLite2-*.mmdb` 与 `geoip/` 目录不再读取。

恢复时脚本会停止当前服务，恢复 `go-original` 二进制、`/usr/bin/zelvik-edge` 和 systemd unit，并将当前 Rust 文件备份到 `restore-current-*` 目录。手动回滚也可以停止服务后恢复备份目录中的文件，再执行 `systemctl daemon-reload` 与 `systemctl start zelvik-edge`。

## 运行前检查

上线前建议确认：

- `configs/api_node.yaml` 存在且 `instanceId`、`secret` 正确（`nodeId` 不是这个文件的键）。
- API 节点 RPC endpoint 可达。
- 运行目录权限正确。
- `data` 和 `logs` 可写。
- 缓存目录可写且容量充足。
- `data/geo` 可写，Geo 资产控制通道可达。
- 监听端口没有被其它进程占用。
- systemd `LimitNOFILE` 足够大。

## 监控

启动本地监控页：

```bash
zelvik-edge --monitor-port 8888
```

生产环境建议只暴露在内网或通过本机 SSH 隧道访问，不建议直接公网暴露。

重点观察：

- 请求速率和错误率。
- 活跃连接数。
- 上游连接失败。
- 缓存命中率。
- 磁盘缓存容量。
- CPU 和内存。
- WAF 拦截量。
- 日志上传失败次数。

## 日志

节点运行日志为文本格式，使用 `RUST_LOG` 控制级别和模块过滤；systemd 的
`Environment=` 可设置该变量。采集 `journalctl` 或 `logs/run.log` 时按文本解析，
不要把运行日志当成逐行 JSON。访问日志和上传协议各自使用对应的数据格式。

常见日志来源：

- systemd 日志：`journalctl -u zelvik-edge`
- 本地运行日志：`logs/run.log`
- 控制面节点日志：由 NodeLogUploader 上报。
- 访问日志：由 LogUploader 批量上报。

节点日志防刷：

- 相同 `type`（或无 type 时的 `tag`+`serverId`）在限流窗口内只上报一次，并用 `count` 累计被合并次数。
- API/连通性类失败（如 `apiNodeListFailed`、`ocspListFailed`、`updatingServerSyncFailed`）默认 10 分钟窗口；其它节点日志默认 5 分钟。
- 上传失败时 NodeLogUploader 指数退避（上限 5 分钟），重试队列按 `type` 合并，避免 API 宕机恢复后对控制面数据库造成日志洪峰。
- 状态里的 `pipelines.nodeLogThrottled` 可观察被抑制次数。

排障时优先看：

```bash
systemctl status zelvik-edge
journalctl -u zelvik-edge -n 200 --no-pager
tail -n 200 logs/run.log
```

## 性能建议

高并发场景建议：

- 使用 release 构建。
- 选择匹配 CPU 的 release 包。
- 提高文件句柄限制。
- 调整 listen backlog 和 SYN backlog。
- 将缓存目录放到高 IOPS 磁盘。
- 避免对大 body 开启详细访问日志。
- 谨慎开启 JS/CSS/HTML 优化等 CPU 密集型能力；WebP 会写入独立缓存变体，仍建议配合缓存策略和图片大小限制使用。
- 对大流量站点启用合适的缓存策略，减少回源。

## 缓存运维

缓存相关建议：

- 独立磁盘或分区存放磁盘缓存。
- 监控磁盘剩余空间。
- 避免多个节点共享同一个本地缓存目录。
- 控制缓存对象数量，避免极端小文件造成元数据压力。
- 定期检查 Mace 指标和缓存清理日志。
- 开启 WebP 时确认站点 WebP 规则、缓存策略和响应状态码同时满足要求；否则节点会按普通响应处理，不会为了不可缓存响应重复转换。

## 常见问题

### 节点启动后没有流量

检查：

- 控制面是否已下发站点配置。
- 监听端口是否成功绑定。
- DNS 是否解析到节点。
- 防火墙或安全组是否放行。
- Host 是否命中站点域名。

### 控制端显示节点在线但没有实时命令

检查：

- 周期配置同步是否成功。
- `nodeStream` 是否能保持连接。
- 控制端是否向该节点下发 `connectedAPINode` 或其它 stream 消息。
- 节点日志中是否出现 `Node stream transport opened`。
- API endpoint 是否发生运行时切换。

节点在线状态和 `nodeStream` 是两条路径：在线状态可以通过 `updateNodeStatus` 上报成功，而实时命令需要控制端在 `nodeStream` 上实际发送消息。

### HTTPS 握手失败

检查：

- 证书是否同步。
- SNI 是否匹配。
- 共享 443 是否进入了 SNI 透传。
- TLS 噪音是否来自非 TLS 流量探测。

### 回源 403 或 404

检查：

- upstream Host 是否符合源站要求。
- SNI 是否符合源站证书。
- request header policy 是否生效。
- 源站是否限制节点 IP。

### 缓存命中率低

检查：

- cacheRefs 是否命中 URL 和 method。
- 响应状态码是否允许缓存。
- Cache-Control 是否跳过缓存。
- 是否存在 Set-Cookie。
- Range 请求策略是否允许。
- 缓存目录是否可写。

### 访问日志不完整

检查：

- 全局和站点访问日志开关。
- 日志通道是否过载。
- 控制面 RPC 是否可达。
- 是否启用了 body 记录导致消息过大。
- 上传端是否出现 retry 或 ResourceExhausted 日志。

## 升级建议

升级前：

1. 备份当前二进制和配置。
2. 确认 release 包和系统架构匹配。
3. 在测试节点验证控制面配置同步。
4. 低峰期替换二进制并重启。
5. 观察监控页、访问日志、错误率和缓存命中率。

从 `26.10.4.7`/`26.10.4.8` 升到 `26.10.4.9` 会在首次启动时把 Mace 0.1.x 的 metrics/telemetry 库迁移到 `data/mace-v2/`，旧库原样保留；迁移耗时与数据量成正比，期间节点尚未开始监听。详见 [mace-migration.md](mace-migration.md#mace-01x-到-020261049-起)。

回滚时恢复旧二进制并重启服务即可。缓存目录和统计数据库通常可以保留，但跨版本如果缓存格式发生变化，应按 release notes 处理。
