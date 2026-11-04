# 每日域名统计候选

```sh
python3 scripts/perf/daily-domain/run.py build
python3 scripts/perf/daily-domain/run.py measure
python3 scripts/perf/daily-domain/run.py verify --directory DOWNLOAD/extracted
```

原始 map 实现取自不可变 e866d8b 的 src/metrics/daily.rs，仅移除全局实例、其未使用 imports 与测试；方法体保留。
候选用已存在的事件 Arc<str> 作为 key，现有 key 一次 get_mut 后累积，只有缺少 key 才取容量预算。
保留 record(&str) 入口、六项计数、基数上限、丢弃记录与输出 String；尚未进入生产。

边界：MemoryGovernor 替换成显式固定容量 fixture，drop sink 替换成计数 fixture；不测实际 memory_snapshot CPU 或日志成本。
两种版本均对已有 Arc clone/drop，避免只给候选预制所有权；System 分配器与生产 mimalloc 不同。
20566 条差分/检查记录含5种容量、非法值、127/128字节边界、按天 flush、四线程更新与并行 flush，核对全部计数。
8种场景、8轮平衡顺序、每轮20块，计时/分配分开构建、编译完成后绑核计时，保留 steal 和所有退步。
新 key 与 legacy-hot 场景单独报告，避免只展示已存在 key 的收益。
