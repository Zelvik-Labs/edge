# 主机名规范化组件

```sh
python3 scripts/perf/host-normalize/run.py build
python3 scripts/perf/host-normalize/run.py measure
python3 scripts/perf/host-normalize/run.py verify --directory DOWNLOAD/extracted
```

直接从不可变4ed5200提取两个函数，原样保留解析和规范化顺序；候选只借用去端口的切片。
65567组差分、12场景×8轮×20块，独立分配/计时二进制，编译完成后绑核；归档全部源码和实际可执行文件。
本地不编译Rust；Actions负责严格Clippy和测量。System分配器、纯字符串组件，不代表节点吞吐。
