"""Fill the report with measured values only after the completed archive."""
import json,re,shutil
from pathlib import Path
root=Path.cwd();local=root/'.local/h3-stage2';base=root/'docs/evidence/h3-stage2-2026-11-02'
report=root/'docs/performance-h3-stage2-2026-11-02.md';text=report.read_text()
x=json.loads((base/'compare-h3/results.json').read_text());groups={(g['connection'],g['cache'],g['label']):g for g in x['groups']}
lines=['本轮 H3、1 KiB 三轮对照的观测中位数如下。CPU 为目标整个 cgroup 的 µs/成功响应，变化为本轮相对第一轮；共享云和客户端限制使这些数值不能直接代表峰值容量。','', '| 模式 / 状态 | 第一轮 CPU | 本轮 CPU | CPU 变化 | 第一轮 RPS | 本轮 RPS |', '| --- | ---: | ---: | ---: | ---: | ---: |']
for mode in ['reuse','multiplex']:
    for state in ['hit','off','miss']:
        old=groups[mode,state,'baseline'];new=groups[mode,state,'candidate']
        if old['passed']==old['samples'] and new['passed']==new['samples']:
            a=old['cpu_usec_per_200_median'];b=new['cpu_usec_per_200_median']
            lines.append(f'| {mode} / {state.upper()} | {a:.2f} | {b:.2f} | {(b/a-1)*100:+.1f}% | {old["good_rps_median"]:,.0f} | {new["good_rps_median"]:,.0f} |')
        else:lines.append(f'| {mode} / {state.upper()} | 场景未全部通过 | 场景未全部通过 | 不排名 | — | — |')
lines+=['','本轮未将 tokio-quiche 接入或实测；上述结果属于现有 Quinn / h3 栈。']
text=text.replace('<!-- RESULTS_OVERVIEW -->','\n'.join(lines))
text=text.replace('<!-- CAPACITY_TABLES -->',(local/'results-tables.md').read_text())
profiles=json.loads((base/'profiles-summary.json').read_text())['profiles'];pmap={(p['phase'],p['state']):p for p in profiles}
parts=['同一发行配置下，第一轮与本轮各采集 H3 HIT/OFF 用户态样本；每次 15 秒、99 Hz，附着 Edge PID，使用 2 MiB mmap 缓冲区，被测进程仍绑定 CPU 0–1，排除内核和控制端。初次 199 Hz 的基线 HIT 丢失 16 个样本，故初始四组保留为不合格诊断；16 MiB 缓冲区重试因超出内核锁页额度失败，该两项记录也保留。前后四组最终按一致的 99 Hz / 2 MiB 参数重采；没有将丢样计为零。采样时吞吐不进入容量表。分组按实际函数栈根识别，排除泛型参数中的 origin_h3 名称误判；包含栈组重叠，不能相加或换算成绝对节省。','', '| 分组 | 第一轮 HIT | 本轮 HIT | 第一轮 OFF | 本轮 OFF |','| --- | ---: | ---: | ---: | ---: |']
for name,title in [('governor_snapshot_in_demux','分流内完整治理快照'),('quic_demux','UDP 分流'),('memory_governor','资源治理'),('l4_defense','L4 防护'),('h3_adapter','H3 适配'),('h3_protocol','h3 协议'),('quinn_protocol','quinn-proto'),('mpsc','Tokio mpsc')]:
    cells=[f'{pmap[phase,state]["groups"][name]["percent"]:.2f}%' for phase,state in [('before','hit'),('after','hit'),('before','off'),('after','off')]]
    parts.append('| '+title+' | '+' | '.join(cells)+' |')
parts+=['',f'总样本数 {sum(p["sample_count"] for p in profiles):,}，记录丢失样本数 {sum(p["lost_samples"] for p in profiles)}。这是对当前可见用户态栈的描述，不是完整开销账单。','']
criterion=local/'criterion/h3_governor_reads'
assert criterion.is_dir(),criterion
bench=[]
for name in ['diagnostic_snapshot','quic_ingress_snapshot']:
    source=criterion/name/'new/estimates.json';data=json.loads(source.read_text());bench.append((name,data['mean']))
    destination=base/'criterion'/name;destination.mkdir(parents=True,exist_ok=True)
    for p in source.parent.glob('*.json'):shutil.copy2(p,destination/p.name)
parts+=['匹配微基准位于 benches/hotpath_bench.rs 的 h3_governor_reads，两个 API 在同一可执行文件中测量。该微基准用 dev opt-level 2、codegen-units 16、无 LTO；容量测量使用 release opt-level 3 / fat LTO，两者不混用。微基准隔离的是快照 API 工作，不是整个 UDP 或 HTTP 请求。','', '| API | 平均 ns/调用 | 95% 置信区间 ns |','| --- | ---: | ---: |']
for name,mean in bench:
    ci=mean['confidence_interval'];parts.append(f'| {name} | {mean["point_estimate"]:.2f} | {ci["lower_bound"]:.2f}–{ci["upper_bound"]:.2f} |')
text=text.replace('<!-- MICROBENCH_AND_PROFILES -->','\n'.join(parts))
tests=json.loads((base/'test-results.json').read_text());metadata=json.loads((base/'metadata.json').read_text());v=json.loads((base/'measurement-validation.json').read_text())
parts=[f'相关 Rust 测试去重后 **{tests["passed_unique"]} 项通过、{tests["ignored_unique"]} 项既有测试忽略**；新增三项治理快照及两项批量接收回归全部通过。14 项 Python 测试通过。Clippy 覆盖 lib/tests/hotpath_bench；发行构建完成。不是全仓所有二进制、集成或 RFC 合规验收。', '',f'冻结根仓源码、Cargo 和公开测试夹具共 {len(metadata["source_sha256"])} 项；冻结后无 Rust 改动。发行构建与基线相同：Rust 1.99.1、x86-64-v2、opt-level 3、fat LTO、codegen-units 1、panic abort、jobs 2。', '',f'候选二进制 SHA-256：`{metadata["binaries"]["candidate"]["sha256"]}`。容量 {v["capacity_cases"]} 场景状态为 `{v["capacity_statuses"]}`；候选 {v["candidate_cases"]} 场景状态为 `{v["candidate_statuses"]}`，已记录候选原生失败 {v["candidate_native_failures"]}。所有旧版与竞品失败保留。']
text=text.replace('<!-- VALIDATION -->','\n'.join(parts));assert '<!--' not in text
report.write_text(text)
print('Report filled; review conclusions before final validation')
