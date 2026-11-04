# 实际回调构造诊断

```sh
python3 scripts/perf/http-callback-layout/run.py build
python3 scripts/perf/http-callback-layout/run.py verify --directory DOWNLOAD/extracted
```

链接实际EdgeProxy和mimalloc，真实回调只构造/销毁，不poll。
每回调20×100000次，固定CPU，保留120块、steal、源码和实际可执行文件。
构造成本不等于首次poll、业务执行、分配次数或节点吞吐；尺寸本身不是性能归因。
此工作流只做诊断构建/Clippy与测量，完整功能CI由独立工作流覆盖，不能混称完整CI通过。
