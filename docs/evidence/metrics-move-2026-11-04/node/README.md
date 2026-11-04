# 统计所有权节点对照

原始 Actions 41769127561，harness 0fc9cbd612c0d1cdefd5389ad903c946f99fbe0c。
下载 artifact 13296585822 和 13296436290，各保留 raw.zip、API artifact.json、逐文件 manifest.json 和 extracted/。

```sh
python3 docs/evidence/metrics-move-2026-11-04/node/verify.py EVIDENCE_DOWNLOAD BINARIES_DOWNLOAD
```

复核两个原始归档、所有文件、实际可执行文件、逐请求证据与 Range 控制；冻结验证器重算192单元及全部配对比例。
源码只差 src/metrics.rs 的构建断言记录于 build-metadata.json；竞品可执行文件位于固定容器镜像，未作为四个 Edge 可执行文件归档。
192 passed、11,015,647 成功响应、128 Range 控制、零错误与 steal。多数条件无性能提升，完整原始变化保留。
