# IP查找与UDP错误载荷的累计完整CI

Actions41774460518，source c5ebb91，对应运行时20e56ff；1367库测试、16二进制测试、集成/vendor/check/Clippy和32组原生节点通过。
4项库探针按既有规则忽略；默认node-control功能，未在此CI重建可选h3-quiche矩阵。

下载artifact13298382608，保留raw.zip、artifact.json、manifest.json、extracted/；另保留原测试job126391458997的日志为job.log，并放入冻结test-summary.json。

```sh
python3 docs/evidence/ip-udp-full-ci-2026-11-04/verify.py DOWNLOAD --commit c5ebb91a76afe4855993f4822b4ac0657e85f18b
```

检查69个原归档文件、全部32组合的退出/完成标记/清理、原日志SHA-256与测试摘要。
归档不含实际节点ELF，不能把记录的ELF身份称为下载后重哈希；没有吞吐或长期稳定性结论。
