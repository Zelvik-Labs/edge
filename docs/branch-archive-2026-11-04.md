# 分支归档与恢复（2026-11-04）

发布后的 35 个远端临时分支已清理，远端活动分支只保留 `main`。
原分支名称、完整提交号和历史集中保存在一个归档标签：
[`archive/branches-2026-11-04`](https://github.com/Zelvik-Labs/edge/tree/archive/branches-2026-11-04)。

归档提交为 `708bde0a1c37280404abbef4e3b0f9ade262a211`，其
[branches.json](https://github.com/Zelvik-Labs/edge/blob/archive/branches-2026-11-04/branches.json)
记录全部 35 个原分支头。五个父历史保留全部原始提交；已逐一核对每个分支头都能从归档到达。
归档提交的文件树包含索引和恢复说明。

| 清理类别 | 数量 | 历史处理 |
|---|---:|---|
| `perf/*` | 32 | 保留全部实验源码、基线和证据历史；性能整合已由 PR #42 发布 |
| `release/*` | 2 | 保留发布指针及附件补传工具；补传工具以独立历史保存 |
| `fix/issue34-memory` | 1 | 按旧的分叉历史保留，不将其原始提交标作已合并，也不重新覆盖主线后续实现 |

此次清理没有变更产品运行时代码或已发布的 `v26.11.1.1` 标签。
该版本仍指向 `21e01923ea5daf31152dfee18d72bf5697ed6029`。
本地已有工作区、未提交修改和构建缓存保留。

## 获取复算所需历史

```bash
git fetch origin tag archive/branches-2026-11-04
git show archive/branches-2026-11-04:branches.json
```

取得这个标签即可取得旧实验和被测源码的完整提交历史，按固定提交号读取源码的复算器可继续使用。
无需恢复原远端分支名称。GitHub Actions 的完整历史检出会一并取得此标签。

## 恢复某个分支

从索引中找到原分支名称和 `head`。例如，在没有同名本地分支的克隆中，恢复性能汇总分支：

```bash
git fetch origin tag archive/branches-2026-11-04
git switch -c perf/program-wide-20261104 ebd34770a817b68bd833f876918fc660a0089f49
```

其他原分支使用索引中对应的完整提交号恢复。归档索引中的“已保存”表示历史可恢复；各分支是否合入主线由其说明及实际内容判断。
