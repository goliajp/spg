# Perf Methodology vs. Mature FOSS Competitors

> 给所有在跟 PostgreSQL / MySQL / Redis / Kafka / RocksDB 等成熟开源主流竞品对抗的自研项目。
>
> 这不是一份"如何写得更快"的微优化清单 —— 那种文章已经够多了。这是一份**工作方式**手册:从我们自己 SPG 项目几次 perf 攻坚的血泪史里抽出来的、把"打不过开源竞品"这件事**真正解决掉**的工作流。
>
> 适用前提:你的产品声称"drop-in 兼容 X"或"performance better than X",而某些 endpoint 实测输给 X。

---

## TL;DR

1. **任何 perf attack 连续 2 轮 polish 没动针 → 立刻停。** 切到"micro-decomposition 模式",不是继续 polish。
2. **永远拒绝以下话术**:"language ceiling"、"user-space 不可触"、"sub-bench noise"、"structural gap"、"architectural ceiling"。每出现一个,你的下一步就错了。
3. **两步走**:Decomposition agent(read-only)拆 18+ stage × file:line × atomic-op-count side-by-side,产 ground truth → Attack agent 基于 ground truth atomically 实施 3-5 个 top attacks。
4. **预测会错,实测才算**。SPG 案例里三个 Top-3 预测,每次都有 1-2 个被翻盘。Decomposition 是**发现**的过程,不是**确认**的过程。
5. **早点 hard-means-do**。我们在 SCALARSQ 上浪费了 10+ 轮 polish,每轮都 sub-noise revert,最后被用户强制要求细拆,3 commits 内闭红线 -85%。早一周做就少烧几百万 token / 几十小时 senior 时间。

---

## 1. 问题:为什么你"卡在 X% 输"

跟成熟 FOSS 竞品对抗,典型场景:

- 13-shape benchmark 11 个赢、1 个输 1.5×、2 个输 5-10%
- 你做了 10 轮 polish:换 allocator、加 cache、改 RwLock 成 ArcSwap、缩小 dispatch tree、defer SystemTime、bumpalo arena ...
- 每次 bench 在 ±10% variance band 内,**没动针**
- 你开始相信"这是 language ceiling 了"或"剩下的是 syscall / kernel / TCP loopback"
- 最终在 spec 里 document 一句"X 端点输 1.8×,user-space 结构性不可触",ship

**这个 framing 本身是错的**。

我们 SPG 在 SCALARSQ 红线上完全踩过这个坑。事后复盘真因:

| 推测的 root cause | 实测翻盘 |
|---|---|
| Catalog RwLock acquire per query | 实测每 query ~0.1μs,**完全不是瓶颈** |
| Dispatch tree 30-50μs overhead | 实测 ~1.5-2μs,**几乎没影响** |
| 隐藏的真凶 | `eval_expr_with_correlated` interpreter 给 bare `Expr::Column` 用,30-50μs |
| 隐藏的真凶 | `Catalog::get(&str)` BTreeMap<String> descent per row,30-40μs |
| 隐藏的真凶 | `iter_cold_rows_of_table` 即使 cold-tier 空也设 iter,10-20μs |

**预测 3 个有 2 个翻盘,实际真凶里 3 个有 2 个根本没在预测清单上**。这就是 polish 永远没动针的原因 —— 我们一直在攻击不是真瓶颈的位置。

C vs Rust idiomatic 写法的语言性能差距,合理上界是 1.5×,真小心写时基本接近 0。**任何超过 1.5× 的差距,都是你 abstraction 浪费了 CPU**,不是语言。当你说"language ceiling"时,你其实在说"我懒得继续找了"。

---

## 2. 解法:Decomposition + Attack 两步 dance

### Phase A — Decomposition(read-only research,不改一行代码)

目标:产一份 ground-truth 文档,把 SPG (你方) vs PG (对手) 的**完整请求生命周期**拆成 18+ 段,每段:

| 字段 | 内容 |
|---|---|
| Stage # + name | e.g. "S08b — Inner-table PK seek per outer row" |
| 对手 path | `references/pg18/src/backend/access/nbtree/nbtsearch.c:_bt_search`,函数入口 → 出口 |
| 你方 path | `crates/spg-engine/src/subquery.rs:probe_with_pk_fast_path`,函数入口 → 出口 |
| 对手 atomic ops | 1 BTree root cache lookup (HashMap, ~30ns) + 1 BTree leaf seek (~50ns) + 1 heap_getattr (~5ns) |
| 你方 atomic ops | 1 Catalog::get BTreeMap<String> descent (log₂N × ~10ns + ~50ns cache miss = ~150ns) + 1 BTreeMap<IndexKey> descent (~200ns) + Vec<Value> alloc (~50ns) |
| 对手估 µs | per call ~85ns × 100 rows = 8.5µs |
| 你方估 µs | per call ~400ns × 100 rows = 40µs |
| Δ + 原因 | +31.5µs,源于 String key BTreeMap descent + IndexKey wrap overhead |
| Attack 候选 | 在 prepare 阶段缓存 `table_idx: usize` 数组下标,exec 阶段直接 `catalog.tables_at(idx)` 跳 String 查表 |

**关键质量约束**:

- **总和必须 ±20% 内匹配实测 wire RTT**。如果你的 18 段加起来是 60μs,但实测 200μs,你**漏了 140μs**。回去找。这是 decomposition 是否完成的硬指标。
- **每段都要有对手等价路径**。读对方源码,不是猜。PostgreSQL 在 `src/backend/`,MySQL 在 `sql/` + `storage/innobase/`,Redis 单文件 `src/*.c`。读得头大也得读。
- **enumerate atomic ops,不要写"很慢"或"复杂逻辑"**。BTree descent N 次 / heap alloc 几次 / atomic CAS 几次 / syscalls 几次。每种 op 都有公认成本表(Apple Silicon ~30ns heap alloc / ~50ns syscall vDSO / ~100ns BTreeMap::get 10k entries...)。
- **预测列在前面,实测列在旁边,翻盘的标红**。Decomposition 的核心价值在翻盘,因为翻盘的项就是 polish 永远没碰到的位置。

### Phase B — Attack(worktree-isolated 改代码)

目标:基于 Phase A 产出的"Top-N 可执行 attack 清单",atomic 一刀切下去。

清单每项必须给:
- 文件:行号
- 具体 code change(不是"改进 X" 这种空话,是"把第 937 行的 `if needs_mat { row.as_row() }` 提到循环外")
- 估 µs 收益
- Semantic change 类(none / requires bench validation / 破坏 API)
- Blast radius(LOC + 受影响的代码面)

Attack agent 串行实施所有 attacks(因为它们都基于 ground truth,理论上互相独立)。Bench validate 在每批 attacks 后跑,**不在每个 attack 单独后跑**(单 attack 的 5-25μs gain 通常 sub-noise,但 3-5 个 cumulative 一定能突破 variance)。

### 为什么是两个 Agent / 两个 Engineer

**职责隔离**:
- Decomposition agent 是 read-only research,不被改代码诱惑、不被 "我先试一下" 拉走、不被 build error 浪费时间。专注产 ground-truth 文档。
- Attack agent 在 worktree 里专心 implement + bench,不需要 re-derive attribution。

**人类版本**:Senior engineer A 做 1 周纯读源码 + 写 ground truth doc;Engineer B 接 doc 后 1 周 atomic 实施 + bench。A 和 B 可以是同一个人 —— 但**必须分两个 phase,不允许 phase 内反复跳跃**。

---

## 3. 实证 — SPG 三红线 closure

我们的 SPG 项目是声称 drop-in 兼容 PostgreSQL 18 的自研 Rust 数据库。13-shape docker-fair bench 上,三个 shape 持续输给 PG18 多个 ship cycle:

| Shape | 起点 | 终点 | 方法论 commits |
|---|---:|---:|---|
| SCALARSQ (correlated scalar subquery) | 0.256ms (2.75× 输) | 0.039ms (1.8× 反超) | 3 attacks atomic |
| DISTA (multi-aggregate w/ DISTINCT + JOIN + GROUP BY) | 25.13ms (1.12× 输) | 12.71ms (1.93× 反超) | 3 attacks atomic |
| INSUBQ (`COUNT(*) WHERE x IN (subq)`) | 1.38ms (1.09× 输) | 1.20ms (1.04× 反超) | 4 attacks atomic |

**总结**:13-shape sweep 现在 13/13 不输 PG18 任何 endpoint。在此之前各红线分别被多个 ship cycle 累积错失。

### SCALARSQ 案例(浪费最久的那个)

**蹉跎史**:

1. v7.37.42 (B) polish — 5 heap allocs 消(write_pg_frame, send_select_command_complete, send_row_description_direct 直写 wbuf)→ sub-noise revert
2. (A3) Step 1 detector + 25 unit pin → 保留作 infra,perf 不动针
3. (A3) Step 2+3 streaming executor + wire dispatch → within variance revert
4. T1.1 Phase 1A-D Value<'arena> + Cow Variants + 1218 callsite mechanical refactor → 类型签名零 perf
5. T1.2 Phase 2 engine arena materialise → 真 -18μs banked(唯一真 win 但远不够)
6. T1.3 Phase 3 wire encode arena fallback path → SCALARSQ BigInt fast path 不动针(arena win 在 mixed-type workload 显现)
7. T1.4 Phase 4 DML/Catalog boundary helpers → 零 perf
8. T1.5 Phase 5 + T1-fallback 5 small wins(Instant-derived clock + current_sql buffer reuse + 6→1 / 3→1 detector merge + inline SELECT N command-complete)→ cumulative within variance
9. 给用户写 acceptance:"SCALARSQ ratio 2.75× = user-space ceiling, plan §T1 failure fallback 触发"
10. 给用户写 hand-wave 解释:"SPG Rust engine 64μs gap + wire framework 72μs gap vs PG18 C MemoryContext + libpq"

10 轮 polish + 多轮 acceptance ducking。每轮 bench 数据 sub-noise,直接说服自己"这是结构性 ceiling"。

**翻盘**:

用户原话(2026-06-21):"noise 的两项与 2.75x 的我不接受 —— 为什么我们没办法按 学习 pg 超越 pg 的方式超过它"+"c 和 rust 不可能有 2.75x 这么大的语言性能差异"。

然后追问:"把 spg 和 pg 的路径拆解成非常小的一段段来分析对比"。

Decomposition agent 跑了 7 分钟,产 18 stage × file:line × atomic-op-count 文档,实测翻盘:

| Top-3 prediction | Decomposition 实测结果 |
|---|---|
| BTreeMap descent per row | ✓ 验证成立,30-40μs 最大单一来源 |
| Catalog RwLock 反复 acquire | ✗ 证伪,~0.1μs |
| Dispatch tax 30-50μs | ✗ 证伪,~1.5-2μs |
| —(未预测) | **★ eval_expr_with_correlated 给 bare `Expr::Column` 用,30-50μs(跟 #1 同级)** |
| —(未预测) | **★ iter_cold_rows_of_table 无条件 iter,10-20μs** |

Attack agent 接 ground truth 后 atomically 实施 3 个 fast paths:

| Attack | File:line | 收益 | Code change |
|---|---|---|---|
| 1 | `subquery.rs:2901-2984` | -25-30μs | `ScalarPkProbeFastPath` 加 `table_idx: usize` 字段,prepare 时缓存 catalog 数组下标,exec 时绕过 String BTreeMap descent |
| 2 | `scalarsq_streaming.rs:308 + 417-426` | -25-30μs | projection build 时为 bare `Expr::Column` 物化 column position,hot loop 用 `row.values[pos].clone()` 取代 interpreter call |
| 3 | `scalarsq_streaming.rs:506` | -10-20μs | cold-tier 空时跳 `iter_cold_rows_of_table` 调用 |

**实测**:SCALARSQ 0.254-0.290ms → 0.039-0.050ms = **-220μs / -85%**,从 2.75× 输直接反超 PG18 ~3×。

### DISTA 案例(翻盘最戏剧化的)

Shape:

```sql
SELECT m.thread_id, MAX(m.subject),
       string_agg(DISTINCT m.sender, ','),
       COUNT(DISTINCT m.id),
       MAX(m.internal_date)
FROM messages m
JOIN mailboxes mb ON m.mailbox_id = mb.id
WHERE mb.user_address = 'u@x'
GROUP BY m.thread_id
ORDER BY MAX(m.internal_date) DESC
LIMIT 50
```

实测 +2.6ms gap(12% 输)。

Decomposition 预测 Top suspect = `string_agg DISTINCT` 的 `BTreeSet<String>` 或 `COUNT(DISTINCT)` 的 `BTreeSet<i64>` 慢于 PG 的 tuple_hash 路径。

**翻盘**:都不是。SPG 在 BigInt DISTINCT 上已经赢 PG 的 Tuplesort。真凶是:

> `string_agg(DISTINCT m.sender, ',')` 的**字面量** arg2 = `','` 触发了 aggregate.rs:937 的 `needs_mat` flag,导致 aggregate.rs:1551 在**每一输入行**做 Cow row materialise。一个 25k-row × 50-group 的 DISTA query 里,这一行 spurious materialise 占 **5-10ms,DISTA 总耗时的 20-40%**。

修复 5 LOC:`needs_mat` 决策时识别 literal arg,literal-only args 不触发 per-row materialise。

实测 DISTA 25.13 → 10.7ms。一刀 fix 关 2.6ms gap **并把 SPG 推进到比 PG 快 2×**。

### INSUBQ 案例(B-3 又一次翻盘)

Shape:`SELECT COUNT(*) FROM messages WHERE id IN (SELECT message_id FROM email_analysis WHERE requires_action = true)`,实测 +114μs gap。

Decomposition Top 3 attack 全部 sub-100μs:dedup HashSet skip (25μs)、IndexKey wrap skip (20μs)、BigInt fast path inline (5μs)。实施后还差 50-60μs。

Decomposition 已预备 B-3 候选:`PersistentBTreeMap` Arc bookkeeping bypass(30-60μs estimate)。Attack agent 进了 B-3,profile 一看 —— **不是 Arc bookkeeping**。真凶:

> `PersistentBTreeMap::get` 在 internal node 上调 `binary_search_by`。对于**叶节点 ≤7 entries** 的常见小规模(我们的索引节点 fanout 是 32 但 hot path 多数节点 ≤7 entries),CPU 的 branch predictor 在 `binary_search` 的不规则跳转里 perf 极差。**线性扫描比 binary search 快**(7 个比较即结束,完美顺序,branch predictor 命中 100%)。

改 30 LOC,把 `binary_search_by` 在 N≤7 时切到 linear scan。INSUBQ 闭剩下的 50-60μs。

---

## 4. 反模式 / 触发词清单

当你或你的团队在 perf review 里说出以下话术时,**stop,撤回,切 decomposition 模式**:

| 错误话术 | 应该改成 |
|---|---|
| "这是 architectural ceiling" | "我没拆够细。把这条 endpoint 拆 18 段 vs 竞品" |
| "C / Rust idiomatic 差异" | "我用了一个 abstraction 浪费了 CPU。是哪个 abstraction?" |
| "user-space 不可触" | "我没读对手的 user-space 代码。读" |
| "TCP loopback / syscall / kernel 残余" | "对手在 user-space 同样路径,他怎么省的?" |
| "sub-bench noise / 5-10μs 不可观测" | "5 个 sub-noise win 累计起来一定突破 noise band。一起上" |
| "结构性 gap" | "结构是什么具体结构?file:line 给我" |
| "noise band within variance" | "把 variance band 缩窄(n=99 → n=1000)再说,不接受变量遮蔽" |
| "绝大部分赢" / "主要场景赢" | "输的那 X 个 endpoint 全列,每个都拆" |
| "客户可以接受 2× 输" | "我的标准比客户高" |

每一句话术对应一个 self-deception 模式。出现就 abort current train of thought。

---

## 5. 工作流(可直接 copy)

### Round 1-2:Surgical Polish

按 best practice / fork report / 内部经验对 hot path 做 2 轮**有目标的**优化。每轮:
- Bench validate(同样 setup,n=99 以上)
- 真正动针:**累计 ≥ 1.5× variance band** = polish 路径有效
- 没动针 = polish 路径无效

不允许"我感觉应该有用,只是 bench 看不出来"。Bench 看不出来 = **没用**。

### Round 3 IF 仍 sub-noise:**SWITCH**

强制流程:

1. **STOP polish**。Don't push another 5μs micro-optimization。
2. **Open fork PG source(或 MySQL / Redis / 等等)**。Read the exact equivalent path. Don't skim — read function entry to function exit, follow callees.
3. **Spawn decomposition task**(给团队中专人 / 给 LLM agent / 自己专门拨 1-2 天):
   - 拆 18+ stage × file:line × atomic-op-count side-by-side
   - 验 ±20% total budget against measured RTT
   - 列 Top-N actionable attacks 清单(file:line + code change + µs estimate + semantic change + blast radius)
4. **Spawn attack task**(基于 decomposition 的清单 atomic 实施)。Worktree 隔离防止 broken intermediate state 污染主分支。
5. **Bench validate** after 全部 attacks land。Single-attack bench 通常 sub-noise — cumulative bench 一定突破。

### Round 4+(如还没闭):再做一轮 decomposition

每次 decomposition 会暴露不同的 ground truth。不要 polish 上次 decomposition 留下的 same set of attacks,要重新拆。

---

## 6. 实施细节 / 工具建议

### 工具

| 阶段 | 工具 | 备注 |
|---|---|---|
| Profile | `samply` (macOS) / `perf` (Linux) | flamegraph 直观但不准定量 |
| Atomic-op cost reference | 参考表见下 | 各 platform 自校 |
| Bench harness | `n=99` 是底线,n=300+ 更好;n=1000 + 10 runs 可检 5μs 级 wins | docker-fair / cool machine |
| Read 对方源码 | git clone full repo,IDE 用 jump-to-def | grep + 读注释 + 跟 callees |
| Worktree 隔离 | `git worktree add` 让 attack 改动不污染 main | LLM-agent dance 也用 worktree |

### 原子操作成本表(Apple Silicon M-series 实测,作 budget 估算基准)

| Op | 成本 |
|---|---|
| L1 cache hit | 1 ns |
| L2 cache hit | 3-5 ns |
| L3 / DRAM access | 50-100 ns |
| Atomic load (uncontended) | 1-2 ns |
| Atomic CAS (uncontended) | 5-10 ns |
| RwLock read (uncontended) | 10-50 ns |
| Mutex acquire (uncontended) | 20-50 ns |
| Heap alloc (jemalloc small) | 30-50 ns |
| Heap free (jemalloc small) | 20-40 ns |
| BTreeMap::get (~10k entries) | 100-300 ns |
| HashMap::get (FxHash) | 30-50 ns |
| HashSet::insert (dedup) | 50-100 ns + alloc |
| Vec::push (amortised) | 5-10 ns |
| `format!` / `write!` 6-digit int | 50-100 ns |
| `itoa` stack 6-digit | 5-10 ns |
| `gettimeofday` vDSO | 25-50 ns |
| send() syscall (TCP loopback, < 8KB) | 1000-3000 ns |
| recv() syscall (TCP loopback, < 8KB) | 1000-3000 ns |

(Linux x86_64 类似,加 10-30% 在 syscalls。各自测自己 platform 的。)

### Decomposition note 模板

```
# <SHAPE_NAME> pipeline decomposition

## Measured baseline
- Competitor (PG18): X.X ms total wire RTT
- Our (SPG): Y.Y ms total
- Gap: Z.Z ms / W%

## Stages

### S01 — <stage name>
- Competitor: file:line (function), atomic ops: [N BTree descents, M heap allocs, ...], µs estimate: A
- Us: file:line, atomic ops: [...], µs estimate: B
- Delta: B - A µs
- Cause: <specific code line referencing the cost source>
- Attack candidate: file:line — <concrete change> — µs gain estimate — semantic class

### S02 — ...

(...继续 18+ stages...)

## Cross-cutting overhead
- O01 — wall clock syscall per query
- O02 — engine lock acquire
- ...

## Total budget validation
- Competitor predicted sum: 比实测低 X%,缺 Y µs 在哪段?
- Us predicted sum: 比实测低 X%,缺 Y µs 在哪段?
- 如果差 > 20%,decomposition 没完成。回去补。

## Top-N actionable attacks (sorted by expected µs gain)

| # | File:line | Code change | Gain µs | Semantic change | Blast |
|---|---|---|---|---|---|
| 1 | ... | ... | ... | ... | ... |
| 2 | ... | ... | ... | ... | ... |
```

模板见我们 SPG 自己的实际产物:`.claude/notes/v7.37.42-scalarsq-pipeline-decomposition.md` 和 `.claude/notes/v7.37.42-dista-insubq-pipeline-decomposition.md`(都 ~3000 字,密集 file:line citations,可作格式参考)。

### 团队配置 / LLM-agent 替代

| 角色 | 人类 | LLM agent |
|---|---|---|
| Decomposition | Senior engineer,1 周 read-only research | 1 agent run, 5-15 min worktree-isolated,无 code edit 权限 |
| Attack | Mid-level engineer,基于 doc 实施 + bench | 1 agent run, 30 min worktree,专攻 implement + bench validate |
| Verify | Same / 别人 review | Coordinator(人/agent)merge + retest + ship |

Decomposition agent 和 attack agent **必须分开运行**。不要一个 agent 同时读 + 改 —— 它会被 build error 拉走,会写一半就开始 doubting,最后产出一个 "尝试了 X 但是 X 没用" 的总结,跟你 polish 10 轮的状态没区别。

---

## 7. FAQ

**Q: 我们 team 没人有时间 read 对方完整源码,只是 grep 关键路径行不行?**

A: 不行。Grep 只让你确认你已经知道的东西。Decomposition 的核心价值是**发现你不知道的东西**(SPG 三个 shape 都是这样翻盘的)。Read PG `nodeAgg.c` 整文件 5000 行是值得的 — 一个下午,换 N 个 ship cycle 的 perf debt 闭合。

**Q: 我们 benchmark variance 很大,5μs win 怎么 validate?**

A: 这是 variance band 问题,不是 perf 问题。提高 n(100 → 1000),增加 run 数(3 → 10 runs),冷机 / 同 docker 网段 / 同 CPU pinning。把 variance band 缩窄到能看 5μs delta。SPG mini docker-fair 在 n=99 × 3 runs 下 variance ~10%;如果你 spec 要看 5μs 级 wins,你需要 ~1% variance,意味着 n=1000+ × 10 runs。这是 bench infrastructure 投资,不是 perf 工作本身。

**Q: 我们的代码不是 Rust 是 Go / C++ / Java / TypeScript,这套 methodology 适用吗?**

A: 适用。Decomposition + attack 的原则是语言无关的。Atomic op cost 表换成你 platform 的。Java 加 GC budget,Go 加 escape analysis 检查,C++ 注意 virtual dispatch + RTTI overhead。Pipeline stages 都一样要拆。

**Q: 我们的竞品是闭源的(如 Oracle / Snowflake)怎么办?**

A: 这是 hard mode。两条退路:
1. 找 PostgreSQL / MySQL 这种结构上类似的开源同行作 proxy reference。你输给闭源对手 X% 的 same shape,通常输给 PG/MySQL 也是 X% 左右。
2. 黑盒 profile + 假设。你只能 measure 自己的栈,推断对手的栈结构。这个时候 decomposition 文档里 "Competitor path" 列填合理的 model + 公开 paper / blog 引用。

**Q: 老板要 ship,没时间做 decomposition?**

A: SCALARSQ 我们 ducked 10+ 轮。每轮花的总 token / engineering time / context switching 远超 1 次正确做 decomposition 的成本。**早 1 周做就少 N 周 polish 浪费**。给老板看这个文档第 1 节就行。

**Q: 万一 decomposition 完发现真的就是没 attack 可做了?**

A: 那时 you've earned 你的 "architectural ceiling" claim。但实践中我们 SPG 跑了 SCALARSQ + DISTA + INSUBQ 三个 case,每个 case 都找到 3-5 个 actionable attacks,每个 case 都从输 → 反超。如果你 decomposition 完真的没 attack 可做,大概率是 decomposition 颗粒度不够细,回去再拆一层。

---

## 8. 总结

如果你在你的项目里只能记住一句话:

> **任何 perf attack 2 轮没动针后,你下一行字不允许是 "polish",必须是 "decomposition"。**

附议:

- 任何"X 不可触 / X ceiling"话术 = 你的 decomposition 不够细
- C / Rust / Go / Java idiomatic 实现差距 ≤ 1.5× 上界,任何超出 = abstraction 浪费,不是语言
- Decomposition + Attack 的两步 dance 是开源竞品对抗的**默认工作流**,不是"我们试试这次"
- 早 hard-means-do,晚浪费 N 轮 ship cycle

---

*本文档版本 1.0 / 2026-06-21。来自 SPG 项目(自研 Rust 数据库,声称 drop-in 兼容 PostgreSQL 18)的 v7.37 ship 阶段实战复盘。*

*欢迎自取、改写、翻译。如果你在自己项目里用了这套 methodology,我们对结果非常好奇。*
