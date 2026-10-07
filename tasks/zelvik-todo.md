# Zelvik-Edge 控制面 Todo

- [ ] EDGE-001 记录 upstream provenance、版本清单和历史配置 fixture。
- [ ] EDGE-002 固定 schema version、能力模型、digest 和签名验证接口。
- [ ] EDGE-003 分离 active snapshot、base metadata、patch journal 和恢复目录。
- [ ] EDGE-004 实现 resource patch create/update/delete 和 canonical serialization。
- [ ] EDGE-005 实现 patch 序列、幂等、缺口、乱序、版本冲突和快照请求。
- [ ] EDGE-006 实现 prepare/validate/switch/report 原子应用状态机。
- [ ] EDGE-007 实现失败保留旧配置、重启恢复和 release 回滚。
- [ ] EDGE-008 兼容 nodeId + secret，增加 plaintext/TLS/mTLS 配置和证书轮换。
- [ ] EDGE-009 完善日志有界队列、批量上传、sequence、压缩、重试和缺口。
- [ ] EDGE-010 增加站点/用户可空计费字段，管理员站点只记录不出账。
- [ ] EDGE-011 实现签名配额租约、本地字节计数、过期策略和动作执行。
- [ ] EDGE-012 验证 API/ClickHouse 断开、磁盘队列满、节点重启和网络恢复。
- [ ] EDGE-013 提供 systemd、Docker、RKE2/K3s 一致配置和探针。
- [ ] EDGE-014 完成 HTTP/2/3、TCP/UDP、缓存、WAF、XDP 和连接压力回归。
- [ ] EDGE-015 完成安全、secret scan、SBOM、签名和发布回滚演练。
