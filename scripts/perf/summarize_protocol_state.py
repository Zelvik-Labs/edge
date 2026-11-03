#!/usr/bin/env python3
"""Publish inspectable summaries of independent protocol/state measurements."""
import argparse
from collections import defaultdict
import csv
from datetime import datetime
import hashlib
import json
from pathlib import Path
import statistics
from zoneinfo import ZoneInfo


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def median(values):
    values=[v for v in values if v is not None]
    return statistics.median(values) if values else None


def run(args):
    evidence=args.evidence; evidence.mkdir(parents=True,exist_ok=True)
    records=[]; runs=[]; groups=defaultdict(lambda:defaultdict(list))
    for path in args.inputs:
        m=json.loads(path.read_text()); target=m['target']
        if m['status']!='completed' or m.get('cleanup_errors'):
            raise RuntimeError(f'incomplete run/cleanup: {path}')
        if not m.get('case_isolation') or any(not c.get('fresh_instance') for c in m['cases'] if c['status'] != 'not-supported'):
            raise RuntimeError(f'formal comparison requires independently reset cases: {path}')
        runs.append({'target':target,'manifest':str(path),'manifest_sha256':sha(path),
                     **{k:m[k] for k in ['source_revision','driver_sha256','client_sha256','client_source_sha256',
                                        'args','images','server_version','scope','cpu_assignment','parent_limits','client_versions']},
                     'binaries':m.get('binaries'),'origin_reference':m.get('origin_reference'),'case_isolation':m['case_isolation'],
                     'started_at':m['started_at'],'ended_at':m['ended_at']})
        for c in m['cases']:
            r={'target':target,'payload_bytes':m['args']['payload_size'],
               **{k:c[k] for k in ['protocol','cache','connection','repeat','status','stem']}}
            r['error']=c.get('error')
            r['cache_status_before']=c.get('integrity_before',{}).get('cache_status')
            r['fresh_instance']=c['fresh_instance']
            n=c.get('native',{})
            for name in ['good_rps','successful','failed','p99_ms','new_connections','request_limit','error_distribution']:
                r[name]=n.get(name)
            r['elapsed']=c.get('elapsed'); r['origin_requests']=c.get('origin_requests')
            r['proxy_cpu_usec_per_200']=c.get('proxy_cpu_usec_per_200')
            if r['elapsed']:
                r['client_cpu_fraction_of_allocated']=c['client_cpu_seconds']/(len(m['cpu_assignment']['client'])*r['elapsed'])
                r['proxy_cpu_fraction_of_allocated']=(c['proxy_after']['cpu.stat']['usage_usec']-c['proxy_before']['cpu.stat']['usage_usec'])/(len(m['cpu_assignment']['proxy'])*1e6*r['elapsed'])
                r['origin_cpu_fraction_of_1core']=(c['origin_after']['cpu.stat']['usage_usec']-c['origin_before']['cpu.stat']['usage_usec'])/(1e6*r['elapsed'])
                r['proxy_peak_mib']=c['proxy_after']['memory.peak']['bytes']/2**20
                mount='/node' if target=='edge' else '/run/cache'
                if c.get('filesystem_after'):
                    r['data_filesystem_used_mib']=c['filesystem_after'][mount]['used_kib']/1024
                    r['data_filesystem_growth_mib']=(c['filesystem_after'][mount]['used_kib']-c['filesystem_before'][mount]['used_kib'])/1024
                r['oom_kill_delta']=c['proxy_after']['memory.events']['oom_kill']-c['proxy_before']['memory.events']['oom_kill']
                steals={}
                for cpu in [f'cpu{cpu}' for ids in m['cpu_assignment'].values() for cpu in ids]:
                    delta=[b-a for a,b in zip(c['host_cpu_before'][cpu],c['host_cpu_after'][cpu])]
                    steals[cpu]=delta[7]/sum(delta[:8]) if sum(delta[:8]) else 0
                r['max_cpu_steal_fraction']=max(steals.values())
                for role,ids in m['cpu_assignment'].items():
                    cores=[f'cpu{cpu}' for cpu in ids]
                    available=1-statistics.mean(steals[cpu] for cpu in cores)
                    field='origin_cpu_fraction_of_1core' if role=='origin' else role+'_cpu_fraction_of_allocated'
                    r[role+'_cpu_fraction_adjusted_for_steal']=r[field]/available if available>0 else None
            records.append(r)
            groups[(r['payload_bytes'],r['protocol'],r['cache'],r['connection'])][target].append(r)
    summaries=[]
    for key,targets in sorted(groups.items()):
        entry={'payload_bytes':key[0],'protocol':key[1],'cache':key[2],'connection':key[3],'targets':{}}
        for target,rows in targets.items():
            rates=[r['good_rps'] for r in rows if r['good_rps'] is not None]
            known=sum(r['successful'] is not None for r in rows)
            s={'samples':len(rows),'passed':sum(r['status']=='passed' for r in rows),'known_native_samples':known,
               'successful':sum(r['successful'] or 0 for r in rows) if known else None,
               'failed':sum(r['failed'] or 0 for r in rows) if known else None}
            s['error_fraction_completed_batches']=s['failed']/(s['successful']+s['failed']) if known and s['successful']+s['failed'] else None
            s['error_fraction']=s['error_fraction_completed_batches'] if known==len(rows) else None
            for field in ['good_rps','p99_ms','proxy_cpu_usec_per_200','elapsed','new_connections',
                          'client_cpu_fraction_of_allocated','proxy_cpu_fraction_of_allocated','origin_cpu_fraction_of_1core',
                          'client_cpu_fraction_adjusted_for_steal','proxy_cpu_fraction_adjusted_for_steal','origin_cpu_fraction_adjusted_for_steal',
                          'max_cpu_steal_fraction','proxy_peak_mib']:
                s[field+'_median']=median([r.get(field) for r in rows])
            s['rps_min']=min(rates) if rates else None; s['rps_max']=max(rates) if rates else None
            s['rps_spread_fraction']=(max(rates)-min(rates))/statistics.median(rates) if rates and statistics.median(rates) else None
            s['errors']=[r['error'] for r in rows if r['error']]
            s['cache_status_before']=[r['cache_status_before'] for r in rows]
            s['quality_flags']=[]
            for condition,label in [
                (s['passed']!=s['samples'],'traffic_errors_or_state_failure'),
                ((s['max_cpu_steal_fraction_median'] or 0)>.1,'cpu_steal_above_10pct'),
                ((s['rps_spread_fraction'] or 0)>.15,'repeat_spread_above_15pct'),
                (s['elapsed_median'] is not None and s['elapsed_median']<1,'short_batch_under_1s'),
                ((s['client_cpu_fraction_adjusted_for_steal_median'] or 0)>.9,'client_near_effective_capacity'),
                ((s['origin_cpu_fraction_adjusted_for_steal_median'] or 0)>.9 and key[2]!='hit','origin_near_effective_capacity')]:
                if condition:s['quality_flags'].append(label)
            s['origin_requests']=[r['origin_requests'] for r in rows]
            entry['targets'][target]=s
        summaries.append(entry)
    output={'runs':runs,'cases':records,'groups':summaries,'total_cases':len(records),
            'passed_cases':sum(r['status']=='passed' for r in records),
            'all_samples_retained':True,'cold_miss_is_bounded_burst':True,'long_stability_completed':False}
    (evidence/'results.json').write_text(json.dumps(output,ensure_ascii=False,indent=2)+'\n')
    with (evidence/'cases.csv').open('w',newline='') as f:
        fields=sorted(set(k for r in records for k in r))
        writer=csv.DictWriter(f,fieldnames=fields); writer.writeheader(); writer.writerows(records)
    date=datetime.fromtimestamp(min(r['started_at'] for r in runs),ZoneInfo('Asia/Hong_Kong')).date()
    text=['# Edge / Nginx / OpenResty 协议、缓存与连接状态吞吐报告',
          '', f'日期：{date}（Asia/Hong_Kong）。当前云环境短测；使用生产 Edge 二进制与真实启动路径，和发行版 Nginx/OpenResty 独立运行。',
          '', f'共 {len(records)} 个测量样本，{output["passed_cases"]} 个通过协议、逐响应正文、连接和缓存状态检查；失败样本完整保留。',
          '', '## 条件与口径', '',
          '- 代理 CPU 0–1 / 2 GiB，原生客户端 CPU 2–3，独立 Nginx 源站 CPU 4 / 256 MiB；产品顺序独立运行，每个协议/缓存/连接/重复场景均重启目标并使用全新临时数据目录。测量不与编译重叠。',
          '- 同一 RSA 测试证书、源站 H1、上游连接复用、32 个并发请求、相同正文。没有业务 Lua 脚本、业务 WAF 规则、访问日志或自定义指标；Edge 基础统计和内置保护保留。',
          '- 比较的是上述业务配置下的现版本行为，未将所有内置保护、响应头和 TLS 密码套件/曲线强制等价。Nginx/OpenResty 的下游 keepalive_requests 为 1,000,000，Edge 的 H2 轮换规则另见下文；这不是纯 I/O 引擎极限基准，也不是本轮指标模板优化的 A/B 收益。',
          '- Edge 使用当前工作树 release：opt-level=3、fat LTO、codegen-units=1、panic=abort、x86-64-v2，经真实启动命令和认证测试控制端下发配置。测试控制端与 Edge 共用代理 cgroup，CPU/内存含它的额外成本。运行时基镜像只提供动态库，当前生产二进制只读挂载。',
          '- 所有场景使用同一 libcurl 8.14.1 原生客户端，两个进程各 16 个并发请求。reuse：32 个连接槽各 1 个在途请求；multiplex：2 个连接槽各 16 个在途请求，服务端仍可轮换连接，实际累计建连数保留；fresh：每请求新连接并核验 NUM_CONNECTS=1。新建 TLS 连接禁用会话恢复，执行完整握手。',
          '- off：该 URL 缓存规则关闭；hit：预热且测量源站增量为 0；miss：每请求唯一新键且源站增量等于成功响应数。cold miss 为限量批次，表中时长可短于 1 秒，不能视为稳态容量。fresh 为固定请求数；其他持续场景按配置时长运行并排空在途请求。',
          '- 吞吐为完整正确 HTTP 200 数 / 测量墙钟时长，展示全部重复的中位数；P99 为各重复客户端延迟 P99 的中位数，不是合并分位数。错误率按已完成传输合计；未完成整批和不成立状态保留 null，不补 0。错误样本的吞吐保留但不用于领先判定。',
          '- 正文为可压缩的合成文本；缓存文件驻留 tmpfs，测的是内存驻留路径及新对象写入，不是物理磁盘冷读。Edge 混合缓存和 Nginx/OpenResty 文件缓存的内部存储策略保留各自实现。全新进程的短批次不代表后台周期任务已进入长时稳态。',
          '- MISS 验证唯一新键回源和缓存准入状态，不逐对象复读核验填充完成，因此不是持久化缓存写入吞吐或填充完成率验收。',
          '', '## 吞吐（请求/秒）', '',
          '| 正文 | 协议 | 缓存 | 连接 | Edge | Nginx | OpenResty | Edge / Nginx（观测比值） |',
          '| ---: | --- | --- | --- | ---: | ---: | ---: | ---: |']
    findings=['## 已复现的问题与解释范围','']
    if any(r['target']=='edge' and r['protocol']=='h3' and r['cache_status_before']=='BYPASS' and r['cache']!='off' for r in records):
        findings+=['- **H3 缓存状态不成立**：实际 H3、证书和正文验证成功，但缓存请求得到 `X-Cache: BYPASS`。当前 [缓存准入分支](../src/proxy.rs#L9668) 对所有 H3 请求执行 `ProtocolBodyNotProvenEmpty`，因此不能将回源吞吐填写为 H3 缓存命中或冷写入吞吐。这是当前请求体结束证明不足导致的保守能力限制；没有删除保护来获得分数。']
    h2_failed=sum(r['failed'] or 0 for r in records if r['target']=='edge' and r['protocol']=='h2')
    if h2_failed:
        findings += [f'- **H2 持续复用有传输错误**：正式独立实例样本共记录 {h2_failed:,} 次失败。保留的 libcurl stderr 包含连接失效及重试耗尽。代码存在 [30 秒内 200 个已接受流的轮换规则](../src/http_proxy_manager.rs#L60)，正常完成的请求也会计入；它会触发 REFUSED_STREAM/GOAWAY。该机制是明确的连接轮换成本来源，但目前未做帧级边界复现，不能把所有错误都归因于这一处。']
    h3_timeouts=sum(r['target']=='edge' and r['protocol']=='h3' and r['connection']=='fresh' and 'timed out' in (r['error'] or '') for r in records)
    if h3_timeouts:
        findings += [f'- **H3 新建连接长批次超时**：当前 libcurl × Edge 组合有 {h3_timeouts} 个独立场景无法在执行上限内完成。表中写超时，不虚构 0 QPS、P99 或完整错误率。部分传输及连接超时细节仍保留；连接回收、准入和客户端互操作的具体原因尚未锁定。']
    other_errors={target:sum(r['failed'] or 0 for r in records if r['target']==target and r['protocol']=='h3')
                  for target in ['nginx','openresty']}
    other_timeouts={target:sum(r['target']==target and r['protocol']=='h3' and 'timed out' in (r['error'] or '') for r in records)
                    for target in ['nginx','openresty']}
    if any(other_errors.values()):
        findings += ['- **其他产品的 H3 错误同样保留**：'+ '、'.join(f'{t} 完整批次内 {n:,} 次传输错误、另有 {other_timeouts[t]} 个整批超时' for t,n in other_errors.items() if n or other_timeouts[t])+
                     '。客户端报告的错误尚需结合包级诊断和第二种客户端交叉验证，不将其全部归为服务端缺陷。']
    steal_values=[r.get('max_cpu_steal_fraction',0) for r in records if r.get('elapsed')]
    if steal_values:
        findings += [f'- **云调度干扰已量化**：全部有效窗口中，最大核 steal 的中位数为 {statistics.median(steal_values)*100:.1f}%，最高为 {max(steal_values)*100:.1f}%。资源表和 JSON 同时保留 CPU/请求、原始 CPU 占用、按 steal 修正的占用及重复范围；观测比值不能作为稳定领先验收。']
    findings += ['- 测试控制端实现认证配置、NodeControl 健康及持久化遥测回执，并未实现全部旧 RPC、配置推流和 Geo/资产接口；对应后台错误与重试保留并计入 Edge 资源。本报告不代表真实控制面、XDP/AF_XDP 或控制面容错验收。',
                 '- 预运行曾因共享 512 MiB 数据目录耗尽触发 Mace `ENOSPC` fatal，生产进程退出。该混合场景运行已与正式数据分开，正式每场景重置并记录数据盘余量。这个故障说明磁盘满的失效行为仍需单独验证和处理，不能用容量测试的健康样本掩盖。','']
    at=text.index('## 吞吐（请求/秒）');text[at:at]=findings
    for group in summaries:
        values=[]
        for target in ['edge','nginx','openresty']:
            s=group['targets'].get(target)
            if s and s['good_rps_median'] is None and any('timed out' in e for e in s['errors']):
                values.append(f'超时（{len(s["errors"])}/{s["samples"]}）')
            elif s and s['good_rps_median'] is None and 'BYPASS' in s['cache_status_before']:
                values.append('BYPASS，状态不成立')
            elif not s or s['good_rps_median'] is None: values.append('未完成')
            else:
                value=f'{s["good_rps_median"]:,.0f}'+(' ⚠' if s['passed']!=s['samples'] else '')
                if s['known_native_samples']<s['samples']:
                    value+=f'（完整记录 {s["known_native_samples"]}/{s["samples"]}）'
                values.append(value)
        e=group['targets'].get('edge',{}); n=group['targets'].get('nginx',{})
        ratio=f'{e["good_rps_median"]/n["good_rps_median"]:.2f}×' if e.get('passed')==e.get('samples') and n.get('passed')==n.get('samples') and n.get('good_rps_median') and e.get('good_rps_median') is not None else '—'
        text.append(f'| {group["payload_bytes"]} B | {group["protocol"]} | {group["cache"]} | {group["connection"]} | '+ ' | '.join(values)+f' | {ratio} |')
    text+=['', '⚠ 表示至少一次失败或有请求错误；完整记录不足时，该数字只来自已完成批次，不代表全部重复的有效中位数，整组错误率保留 null。未完成不是 0 QPS。比值只描述本次观测，不能替代调度波动和资源余量验收。', '',
           '## 延迟、错误与资源', '',
           '| 正文/协议/缓存/连接 | 产品 | P99 ms | 错误 % | CPU µs/成功请求 | 客户端 CPU %（两核） | 源站 CPU %（一核） | 最大核 steal 中位 % | 批次秒 | 吞吐 min–max |',
           '| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |']
    def fmt(v,multiplier=1): return '—' if v is None else f'{v*multiplier:.2f}'
    for group in summaries:
        key=f'{group["payload_bytes"]}/{group["protocol"]}/{group["cache"]}/{group["connection"]}'
        for target in ['edge','nginx','openresty']:
            if target not in group['targets']: continue
            s=group['targets'][target]
            text.append(f'| {key} | {target} | {fmt(s["p99_ms_median"])} | {fmt(s["error_fraction"],100)} | {fmt(s["proxy_cpu_usec_per_200_median"])} | {fmt(s["client_cpu_fraction_of_allocated_median"],100)} | {fmt(s["origin_cpu_fraction_of_1core_median"],100)} | {fmt(s["max_cpu_steal_fraction_median"],100)} | {fmt(s["elapsed_median"])} | {fmt(s["rps_min"])}–{fmt(s["rps_max"])} |')
    text+=['', '## 版本与证据', '']
    for target in ['edge','nginx','openresty']:
        entry=next((r for r in runs if r['target']==target),None)
        if entry: text.append(f'- {target}：`{entry["server_version"].splitlines()[0]}`；镜像 ID `{entry["images"][target]}`。')
    text+=['', f'- [完整结果及每次测量]({evidence.relative_to(args.report.parent)}/results.json)',
           f'- [可复算 CSV]({evidence.relative_to(args.report.parent)}/cases.csv)',
           '- 本地 manifest、命令、stderr、压缩逐传输记录、测试配置和测试私钥保留在 `.local/protocol-state-matrix/`；私钥不进入版本化证据。',
           '', '## 结论边界', '',
           '这是同机隔离的短时探索结果。低于 1 秒的冷批次、客户端/源站接近饱和、CPU steal 或重复波动较大的分组均不能作为稳定领先验收；这些条件可从逐次资源与完整原始样本复核。尚未完成 24–72 小时稳定性、真实公网 RTT/丢包、TLS 会话恢复、业务 WAF/Lua/自定义指标或磁盘冷读缓存的独立对照。']
    args.report.write_text('\n'.join(text)+'\n')
    print(json.dumps({'cases':len(records),'passed':output['passed_cases'],'groups':len(summaries),'report':str(args.report)}))


if __name__=='__main__':
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--inputs',type=Path,nargs='+',required=True)
    p.add_argument('--evidence',type=Path,required=True)
    p.add_argument('--report',type=Path,required=True)
    run(p.parse_args())
