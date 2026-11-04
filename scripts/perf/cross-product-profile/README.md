# 三产品 CPU 路径

Actions下载已有实际Edge/控制器/客户端二进制，按固定SHA-256复核；不编译Rust。
三产品H1/H2、HIT/OFF、两轮反向顺序，24格，全新实例和独立1/2/1 CPU。
NG/OpenResty镜像只增加perf/procps，记录实际进程ELF哈希并保留ELF。

```sh
python3 scripts/perf/cross-product-profile/evidence.py prepare
bash scripts/perf/cross-product-profile/run.sh
python3 scripts/perf/cross-product-profile/evidence.py verify --directory DOWNLOAD/extracted
python3 scripts/perf/cross-product-profile/evidence.py collect
```

公开allow-list只保留响应记录、符号化栈、报告、镜像/进程元数据和竞品ELF；不发布raw perf.data、TLS私钥和运行时配置。
结果用于路径归因，全部采样RPS/CPU归一化排除出容量排名。未知符号和不完整样本仍须报告。
