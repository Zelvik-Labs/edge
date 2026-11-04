# 七组原始性能归档的固定版本复核

[41816776890](https://github.com/Zelvik-Labs/edge/actions/runs/41816776890) 在源码 da3a979c1ca852e65c8b3af1930209995533793b 上通过三项报告边界测试与七组原始归档复核。这里的 [verification.json](verification.json) 是该运行日志 PERF_REVIEW_RESULT 的完整结构化副本，包含全部逐组结果、缓冲配对轮次、短窗口 caller 分组及实际客户端记录进度。

不是新增 183 格容量压测；是对七个已存在运行的 183 格、35,115,447 份成功响应记录和相关采样进行独立重算。调用者窗口、syscall、CPU profile 和容量对照口径不同，不把它们合并成一个性能排名。

## 输入与输出

脚本 [scripts/perf/evidence-review/review.py](../../../scripts/perf/evidence-review/review.py) 的 SPECS 固定每组原 run/source/prefix。完整 Git 历史用于提取历史客户端 C 源、独立工作树和各运行自己的验证器。先校验实际固定 Edge/controller/client 字节，再核对原 ZIP/API SHA、每个安全 ZIP 项与文件 SHA、所有保留 HTTP 记录、栈/计数及原汇总。新控制源码/锁文件还与 Git 树核对，每格 CPU 集合必须一致且不共享。

复核产物 [13315331623](https://github.com/Zelvik-Labs/edge/actions/runs/41816776890/artifacts/13315331623)（355,303,841 字节）ZIP SHA-256：
`941a0d2fab4f4a6e1b78a5ff2cb7f70a01699052a3723292ae450b048463b6bb`。

内含原始 ZIP、API 元数据、逐文件清单、每组原 run 元数据、重算结果、原始/重算结果比较及最终报告。原 ZIP 不修改；临时派生工作副本可由验证器写入。所有测量 ELF 仅哈希，不执行。不含 TLS 私钥、原始 perf.data/参数载荷或部署动作。

## 重放

使用完整仓库历史和固定 da3a979c 工作树，在一个未存在 .local/evidence-review 的目录运行。GH_TOKEN 只需当前仓库 Actions/contents 读取权限；Actions workflow 已自动注入，不需要在聊天或日志提供凭据。仅 Python 标准库与 Git，无 Rust 编译。

```sh
python3 -m unittest discover -s scripts/perf/evidence-review -p 'test_*.py' -v
python3 scripts/perf/evidence-review/review.py
```

Actions 提供 GITHUB_STEP_SUMMARY；其他运行器可设置为本地报告文件路径。某个原运行不是 success、原始记录不完整或任一校验失败，记录失败并继续检查其他独立组，最终任务仍失败，不用成功重试替换原测量。

## 复核链路自身的失败

- 41815857888：syscall、框架 CPU 两组通过后，误把 cross 的 complete_cells 读作 cells；任务失败，原归档 13314263837 保留。
- 41816107941：七组原始校验全部完成后，最终报告把 syscall medians 当作带 rps 的容量数据，格式化失败；原始结果及失败归档保留。
- 41816567948：修正格式化，三项边界测试和全部七组通过；归档 13314528836。
- 41816776890：进一步逐格检查实际 CPU 集合、对照源码/锁与 Git 树一致，三项测试及七组全部通过；作为本目录最终冻结来源。

原始被测运行中的早期 HTTP、资源、编译或 checkout 失败在各专题报告继续保留；上述复核通过不会覆盖它们。未声明生产 Edge 增益或整体超过竞品。
