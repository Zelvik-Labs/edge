# futex 操作与调用栈归因

同批四目标计数中，固定 ba474 Edge 的 H1/H2 OFF 约有 1.93/1.81 次 futex/有效响应，而裸 Pingora 接近零。这只能说明存在差异，尚不能归因到统计队列或某个锁。

本轮增加独立观察器，选择两种实际固定 ELF，H1/H2 OFF、连接复用、两轮反向顺序共八格。保留同样的真实 HTTP 状态/正文/协议控制及线程附加、前后 ELF 身份、主动脱离检查。只跟踪 futex，记录操作名（含 WAIT/WAKE/PRIVATE 等）和用户态 caller chain；不改变被测程序线程、资源配额、worker 用户或功能。

原始系统调用参数与地址不上传。公开记录只包含线程 ID、操作名、去地址后的符号链，保存每条脱敏事件并重新汇总。解析器遇到未知格式失败，空线程/未返回调用无栈会单独报告。原始日志不在归档中，因此独立复核范围是已保留的脱敏记录，不是原始参数日志。

状态：实现后交 GitHub Actions 执行，尚无归因结论。插桩和 unwind 显著改变调度；不报告容量排名、等待时长或组件 CPU 份额，也不据此直接删除统计/安全功能。

仍未解决：futex 的调用来源、额外 H1 写的方向、整体性能差距。经审批的功能降级：无。


## 首次逐调用 unwind 失败，切换采样

[41804385041](https://github.com/Zelvik-Labs/edge/actions/runs/41804385041) 的 Edge H1/H2 两格均为 0 成功，共 96 个 curl 28 超时，后续未运行；不能作为正常负载的 futex 归因证据。附加/脱离和脱敏解析完成，未知符号的 Edge() 栈也不能指向某个模块。归档 13308782863（1,066,716 字节、53 文件，SHA-256 `a96b022ecc893f7d5b072e1e7f4cc741915138fcfb4d36beacca8357a777fb17`）保留失败与每条脱敏事件。
该方法逐 syscall 停顿并 unwind，扰动不可接受；没有延长客户端超时、放宽 HTTP 验证或改变服务器资源来制造成功。

改用内核 syscalls:sys_enter_futex tracepoint，每 64 次事件采样用户栈，仅运行固定 Edge 的 H1/H2 OFF 各两次。测量不再逐 syscall ptrace 停顿。先检查 runner tracefs 能力；符号化后只公开操作值和调用栈，丢弃原始参数载荷与 perf.data。专门验证参数过滤；保留所有 HTTP 失败并要求无丢失样本。
采样窗口不声明覆盖每个请求或每个线程的全部事件，不据采样数倒算每响应调用数；确切计数来自此前独立 counts-only 对照。采样步骤仍待 Actions 结果。


## 采样器的失败边界与修正

首次 tracepoint 运行 41805597028 没有有效采样率配置证明：H2/H1 写出约 2352/5073MB，发生丢样本且代理触及原 2GiB cgroup 上限，累计 93,264 成功和 103,625 失败，结束时正文/TLS 完整性也失败。没有 oom_kill，不能写成 OOM 杀进程；本轮不能作正常负载归因。归档 13309567487（44 文件、8,444,292 字节）及[冻结失败记录](evidence/futex-attribution-2026-11-04/sampled-first-failure/verification.json) 保留。

随后两次预检查拒绝了测量：41806679819 的 perf 6.12 不接受 tracepoint /period=512/ 语法，归档 13309074426；41807179476 已真实显示 512 周期，但输出为带空格的 union 且省略零值 flags，校验器错误拒绝，归档 13310132831。这两轮均未启动实际压测，不按 HTTP 性能失败统计。
现在把 -c 放在事件之后，并保留短预采样与实际 perf.data 的 evlist -v 和 -F。核查 Linux v6.12 util/evsel_fprintf.c 71–78 行，-F 依实际 attr.freq 输出 sample_period 或 sample_freq；因此要求明确 sample_period=512 且拒绝 sample_freq，不能只凭命令行或模糊 union 猜测采样模式。原始参数仍不公开，全部 HTTP、丢样本和资源门不变。正在独立运行 41807941740，尚不声明最终模块归因。

四种失败均由[公共复核器](evidence/verify_diagnostic_failure.py)按冻结 stage 检查 ZIP、原始记录及错误边界；不将校验器修正后的重放或 partial 数据标成整轮通过。逐 syscall unwind workflow 已改手动运行，以免自动重复已知扰动方案。
