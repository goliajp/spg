# Testing — v2 Constitution (v7.38 起立法,skeleton)

> **Status**: skeleton authored 2026-06-21 alongside v7.37.4 ship. v7.38 train 期间 fill each section with concrete content; v7.38 release gate 之一 = 本文件 each placeholder 替换为实际链接 / acceptance criteria / 运行命令。
>
> **Relationship to existing `docs/TESTING.md` (v1)**: v1 documents 5 categories of gate.sh as of v7.37. v2 (this file when complete) becomes the canonical replacement and v1 stays as a stub redirecting here.
>
> **关联文档**(读这里前先看):
> - `.claude/notes/v7.38-plan.md` — 完整 v7.38 工程计划
> - `docs/PERF_METHODOLOGY_VS_FOSS.md` — perf 工作流(横向 doctrine,本立法引用)
> - `memory/feedback-perf-hard-means-do-micro-decompose.md` — perf 工作铁律

---

## 0. 范围

本文档 = SPG test suite 立法,适用所有 v7.38+ 版本。

立法层级:
1. **元机制**(P0)— 4 个,任何测试都建在它们之上
2. **8 轴**— 测试覆盖的 8 个维度
3. **工艺纪律**— 10 条横向规则,每个测试遵循
4. **速度预算**— 横向硬约束,任何测试 / gate / 工具链都不许违反
5. **Perf 工作流**— `docs/PERF_METHODOLOGY_VS_FOSS.md` 是横向 doctrine,轴 8 acceptance 引用

不在本文档范围:
- 单条 test case 怎么写(详见各轴 README)
- bench 数据怎么读(详见 `docs/PERF_METHODOLOGY_VS_FOSS.md`)
- ship gate prerelease checklist(详见 `scripts/release.sh` + plan §七)

---

## 1. 元机制(4 条,v7.38 P0 必须先建)

### A. Injection points

**文件**:`crates/spg-engine/src/testkit/injection.rs`(v7.38 P0 day 2 创建)

**用法**:
```rust
// 引擎侧:编译期 no-op when feature `injection-points` off
injection_point!("aggregate_pre_topk", &group_count);
```

```sql
-- 测试侧:
SELECT spg_injection_attach('aggregate_pre_topk', 'wait');
-- ... 触发引擎到此点 ...
SELECT spg_injection_wakeup('aggregate_pre_topk');
```

**接进位置首批**(v7.38 P0 day 2 列):
- [ ] `wal_group_commit_leader_chosen`
- [ ] `checkpoint_cow_swap_pre` / `_post`
- [ ] `spg_sqlx_inline_budget_cancel`
- [ ] `aggregate_spill_trigger`
- [ ] `cold_tier_wakeup_resume`
- [ ] `planner_first_row_fetch`
- [ ] `index_build_post_seal`
- [ ] `tx_commit_walgroup_leader_switch`
- [ ] `prefetch_sequential_scan_threshold`(钉 e2e_prefetch flake)
- [ ] `segment_forward_disconnect_resume`(钉 e2e_segment_forward flake)

**Acceptance**:
- 编译 release 模式时所有 injection_point!() 展开为 `()`,**汇编可验证 zero overhead**
- 测试构建 feature `injection-points` on,attach / wakeup / error / notice 4 种 action 全支持
- ≥ 8 个点接进引擎 hot path
- 每个接进的点有 1 个 testcase 验证 attach + wakeup 可见

**借鉴**:PG `INJECTION_POINT()` + MySQL `DBUG_EXECUTE_IF` + SQLite `testcase!()`

---

### B. Permutation matrix runner

**文件**:`xtests/perm-runner/` cargo bin(v7.38 P0 day 5-7 创建)+ `tests/permutations.toml`

**Schema**:
```toml
[[permutation]]
name = "embedded"
env = { SPG_PERMUTATION = "embedded" }
# 默认 dev cycle 跑这一个

[[permutation]]
name = "server_simple"
env = { SPG_PERMUTATION = "server_simple" }

[[permutation]]
name = "server_extended"  # = mailrs prod path
env = { SPG_PERMUTATION = "server_extended" }

[[permutation]]
name = "joinfold_off"
env = { SPG_DISABLE_JOINFOLD = "1" }

[[permutation]]
name = "topk_off"
env = { SPG_DISABLE_TOPK_LIMIT = "1" }

[[permutation]]
name = "hot_only"
env = { SPG_FORCE_HOT_TIER = "1" }

# + sanitizer / debug-asserts / release 三模式
```

**接进 `gate.sh`**:
- fast tier: 跑 3 个核心 permutation(embedded / server_simple / server_extended)
- full tier: 跑全部

**Acceptance**:
- 一份 sqllogictest corpus N× 跑(N = permutation 数),全绿
- TOML 配置接进 CI,新增 permutation 无 code change

**借鉴**:SQLite `permutations.test` + CockroachDB `# LogicTest:` directive + MySQL `--ps-protocol` / `--hypergraph` mutation

---

### C. 三主差分 oracle

**文件**:`xtests/oracle/{pg18,mysql,mariadb}/` 各起 docker(v7.38 P1 期间起步,与轴 1+2 共建)

**架构**:每条 `.slt` 同步对 PG18 / MySQL / MariaDB docker 跑,sort 后 byte-equal。差分 expected 入 `expected/`。

命名约定(借 YugabyteDB):
- `port.<name>` — 从 PG `src/test/regress/sql/` port 过来的
- `orig.<name>` — SPG 自家新增的
- `depd.<name>` — 仅 setup,被别的 test depend

**Acceptance**:
- PG18 oracle 跑通(docker-compose),已 port `port.subquery_*` / `port.aggregate_*` / `port.join_*` 等子集 ≥ 80%
- MySQL oracle 跑通,covers MySQL specifics
- MariaDB oracle 跑通,covers MariaDB specifics
- R32 历史 regression 直接进 `port.subquery_correlated_agg.spg.out` 标 EXPECTED FAILURE,修后 expected 移走 → 退步立刻被捕

**借鉴**:YugabyteDB(vendors PG regress)+ RisingWave(`pg_regress` Rust port)+ TiDB(`go-randgen` vs MySQL)+ ClickHouse `casa_del_dolor`

---

### D. 测试模式 GUC

**文件**:engine 内每个非确定性 surface 加 env-gate 旋钮 + `xtests/sigil/test-mode-gucs.md` 索引(v7.38 P0 day 4-5 起)

**Index**:
| 旋钮 | 关掉的非确定性 | engine acceptor (file:line) |
|---|---|---|
| `SPG_TEST_COMPUTE_QUERY_ID=regress` | query id 不进 EXPLAIN | TODO |
| `SPG_TEST_EXPLAIN_NO_COSTS=1` | EXPLAIN 不输出 cost | TODO |
| `SPG_TEST_STATS_FROZEN=1` | stats 冻结快照 | TODO |
| `SPG_TEST_PLAN_DETERMINISTIC=1` | 关掉所有 cost-based 决策走 lexical | TODO |
| `SPG_TEST_DISABLE_TOPK=1` | 关 aggregate top-K LIMIT 路径 | TODO |
| `SPG_TEST_DISABLE_JOINFOLD=1` | 关 join folding | TODO |
| `SPG_TEST_RANDOM_SEED=N` | nondeterministic 跑用同种子 | TODO |
| `SPG_TEST_PREFETCH_DETERMINISTIC=1` | 钉 e2e_prefetch race | TODO (新加) |

**铁律**(v7.38 后所有 test):
- flaky test 默认先看是不是该加一个 GUC,**不该靠 retry 或 `#[ignore]`**
- `#[ignore]` 不解决任何问题,只藏 bug
- retry-on-failure(任何形式)等价于 `#[ignore]`

**Acceptance**:
- ≥ 7 旋钮接进 engine,每个旋钮接 acceptor 行号写本索引
- 每个旋钮有 1 testcase 验证关掉对应 surface

**借鉴**:PG `compute_query_id=regress` / `EXPLAIN (COSTS OFF)` / `stats_fetch_consistency=snapshot`

---

## 2. 八轴(每轴 acceptance 见 v7.38 plan §二)

### 轴 1 — SQL 标准 conformance

**Owner**:`xtests/sqllogictest/corpus/{pg_regress,duckdb,mysql,pgvector}/`

**Acceptance**(plan §二 轴 1):
- [ ] 三 oracle 全绿
- [ ] ≥ 80% PG `src/test/regress/sql/` port
- [ ] R32/R33 历史 regression 都有归属 `.slt`

### 轴 2 — 三方言 specifics

**Owner**:`xtests/sqllogictest/dialect/{pg,mysql,mariadb}/`

**Acceptance**:
- [ ] 三方言 oracle 全绿
- [ ] mailrs R12-R34 全部测有归属(`round_NN.slt`)
- [ ] 三方言独有特性(PG RETURNING/ARRAY/JSONB ops/LATERAL,MySQL `ON DUPLICATE KEY UPDATE`/`STRAIGHT_JOIN`,MariaDB `INSERT IGNORE`/`SEQUENCE`)各一组

### 轴 3 — 连接池

**Owner**:`xtests/sqlx-pgwire/{pool_stress,pool_chaos}.rs`

**Acceptance**:
- [ ] 64 并发 60s 0 泄漏
- [ ] 服务重启 client 自动回弹
- [ ] mid-query 弃连不破坏状态

### 轴 4 — 隔离 / 并发

**Owner**:`xtests/isolation/` Rust 版 isolationtester

**Acceptance**:
- [ ] PG `src/test/isolation/specs/*.spec` 直接 vendor 跑通
- [ ] Hermitage 11 case × 3 iso level = 33 格,每格按 Berenson/Adya 标准断言
- [ ] Elle on `list-append` + `rw-register` 交叉验证
- [ ] PG postgresql-12.3 Jepsen "fresh insert G2-item" 反向测试 — SPG **不修复**(保持 PG 行为)

### 轴 5 — 事务一致性 + 崩溃原子性

**Owner**:`xtests/recovery/` Rust 版 TAP 框架

**Acceptance**:
- [ ] Rust 版 `SpgCluster::{new,init,start,stop,kill9,promote,wait_for_catchup}`
- [ ] PG `013_crash_restart.pl` 抄过来跑通(SIGQUIT + SIGKILL 双路径)
- [ ] PG `027_stream_regress.pl` 抄过来跑通(sqllogictest corpus 作负载 + crash + replay + diff)
- [ ] Jepsen lite invariants(bank / counter / register),1k kill 实验 0 invariant violation

### 轴 6 — Dump / import

**Owner**:`xtests/dump_compat` 升级 + `xtests/data_compat` GB 级

**Acceptance**:
- [ ] `%pgdump_runs × %tests` 矩阵
- [ ] `adjust_*()` 归一化框架
- [ ] GB 级 round-trip 字节相等
- [ ] mysqldump / mariadb-dump 全 schema 绿

### 轴 7 — 灾难恢复

**Owner**:`xtests/recovery/`(与轴 5 共)

**Acceptance**:
- [ ] crash recovery 系统化
- [ ] PITR(mailrs ask #2 历史坑闭)
- [ ] 半写页检测(SQLite journal-test VFS 思路)
- [ ] basebackup / restore round-trip
- [ ] injection_points 模拟 "recovery 中再次 OOM / IO error"

### 轴 8 — Perf 四层

**Owner**:`xtests/perf_gate/` × 各 crate + `xbench/competitor/`

**Acceptance**:
- [ ] 8.1 原子(Criterion microbench):hash/merge/sort/scan/index-lookup/agg/wire-encode 每算子 ns/row
- [ ] 8.2 简单 e2e:SELECT 1 / 主键 / 单 INSERT / 简单 JOIN p50/p95/p99
- [ ] 8.3 高压(stress):pgbench tpcb-like + sysbench oltp_read_write + 自家 stress runner
- [ ] 8.4 大表(scale):TPC-C(BenchBase)+ 自家 inbox 1M / 10M
- [ ] **复现性栈强制**:isolcpus + nohz_full + numactl + performance governor + ASLR off + THP off + mitigations off
- [ ] **planner-cost regression 类**:加 DuckDB tpch_plan_cost 思路,SPG 同 query plan cost 数值变化 > 5% trigger fail
- [ ] **三栏并列报表**(PG18/SPGS/SPGE)沿用 [[feedback-spgs-spge-perf-bar]] 红线
- [ ] **`docs/PERF_METHODOLOGY_VS_FOSS.md` 工作流强制**(详 §6)

### 13-shape baseline 锁定(v7.37.4 ship 时确定)

| Shape | PG18 (ms) | SPGS (ms) | 状态 v7.37.4 | v7.38 期间允许退化上限 |
|---|---:|---:|---|---|
| PLUCK | 0.018 | 0.020 | tied (noise) | +20% |
| COUNT | 0.706 | 0.318 | SPGS WIN 55% | +10% |
| NOTEX | 1.387 | 0.582 | SPGS WIN 58% | +10% |
| TOPN | 15.872 | 4.642 | SPGS WIN 71% | +10% |
| **DISTA** | 24.413 | 12.713 | **SPGS WIN 48%** | +5% |
| INBOX | 38.882 | 15.381 | SPGS WIN 60% | +10% |
| PROJ | 9.910 | 7.820 | SPGS WIN 21% | +10% |
| INLIST60 | 0.109 | 0.048 | SPGS WIN 56% | +10% |
| **INSUBQ** | 1.250 | 1.201 | **SPGS WIN 4%** | +3% |
| LEFTJOIN | 0.802 | 0.334 | SPGS WIN 58% | +10% |
| **SCALARSQ** | 0.088 | 0.039 | **SPGS WIN 56%** | +5% |
| GBINT | 2.670 | 2.115 | SPGS WIN 21% | +10% |
| HAVING | 2.826 | 2.129 | SPGS WIN 25% | +10% |

**红线**:任何 v7.38 期间的 commit 触发上方退化上限 = perf gate 失败 = **触发 perf decomposition 工作流**(详 §6)。

---

## 3. 工艺纪律(10 条,每个测试遵循)

| # | 纪律 | 借鉴 | 落点 |
|---|---|---|---|
| 1 | **check-testcase between every test** | MySQL `check-testcase.test` | 每 test 前后跑环境不变量断言 |
| 2 | **三模式同输出**(release / debug-asserts / sanitizer 字节相等) | SQLite §7.5 | CI 加 sanitizer 模式 job |
| 3 | **`testcase!()` / `ALWAYS!()` / `NEVER!()` 宏** | SQLite | Rust 版 MC/DC,coverage build 用 |
| 4 | **每历史 regression 一条归属测试**(`100_bugs.pl` 约定) | PG | round-NN.slt + 复现器入 corpus |
| 5 | **adjust_*() 归一化 + 全等 diff** | PG `pg_upgrade` | 跨主对照、dump round-trip 都用 |
| 6 | **iter-OOM / iter-IO 故障注入循环** | SQLite §3.1 §3.2 | 每分配点 / IO 点逐个失败,assert clean error + zero leak + integrity_check |
| 7 | **100× rerun flaky-gate**(新增/touched 测试) | ClickHouse | merge 前 100× 稳定,quarantine 跟 TiDB |
| 8 | **dbsqlfuzz 形态 fuzz 入树语料库** | SQLite | findings minimize 后 check-in `tests/fuzz/corpus/`,fuzzer 持续 oss-fuzz 跑 |
| 9 | **PG_TEST_EXTRA opt-in 桶** | PG | 外部 daemon / 改证书 / 占端口的测试 default skip,加 EXTRA 才跑 |
| 10 | **`require <ext>` / `require-env`** | DuckDB | 缺 docker / 缺 mini / 缺 psql 时 self-skip 不 fail |

---

## 4. 速度预算(硬约束,任何工具链 / gate / test 不许违反)

### `gate.sh` 双档

| 档位 | 触发 | 时间上限(mini) | 时间上限(dev) | 内容 |
|---|---|---|---|---|
| **fast(默认)** | 每 PR / 本地反复跑 | **≤ 5 min** | ≤ 10 min | 各类 fast tier |
| **full(`--full`)** | nightly / pre-release | 无上限 | 不在 dev loop | 全部 + 长跑 |

### fast tier per-category 子预算

| Category | fast 上限 | 速度策略 |
|---|---|---|
| `lint` | ≤ 30 s | 现状 OK |
| `unit` | ≤ 60 s | release in-crate `#[test]` |
| `e2e` | ≤ 90 s | 仅 single permutation `embedded`,**跳 docker** |
| `gates` | ≤ 120 s | release perf microbench fast,**不跑 1M / TPC-H** |
| `biz` | ≤ 60 s | sqllogictest corpus 抽样跑(全跑进 full),**跳 docker** |

### `bench --fast` 模式

| 子档 | 时间 | 用途 |
|---|---|---|
| **`bench --fast`** | ≤ 30 s | 单 shape × 3 algos microbench |
| **`bench --diff`** | ≤ 60 s | 只跑触碰过的源文件相关的 perf gate |
| **`bench`**(默认) | ≤ 5 min | 四层全跑但 cardinality 缩到 fast |
| **`bench --full`** | 不限 | 全 cardinality + TPC-H/TPC-DS |

### Fast tier 跳 docker 的补偿

fast tier 跳 oracle docker → 用 **SPG-self differential** 顶:同 SQL 跑 embedded vs server_simple vs server_extended,**三栏自洽**作为 near-oracle。

### 元机制 / 纪律的速度兜底

详 v7.38 plan §六 C/D.

### 预算闸自查

`gate.sh bench-budget-check` CI 任务 — 跑 `gate.sh all` 并断言 wall-clock < 5 min on mini。**预算闸自己有闸**。

---

## 5. Cargo dev cycle 不退化(横向)

- 长跑测试 `#[ignore]`,默认 `cargo test` 不进
- `cargo check` 路径不会被 sqlx 重 macro / fuzzer 牵进重 build
- permutation runner 是 binary,不进 `cargo test --workspace` 默认
- IDE 反应不变(无新 build.rs / 无 proc-macro 爆炸)

---

## 6. Perf 工作流(横向 doctrine)

**所有 perf gate failure / perf 调查 / perf 收尾 都遵循 `docs/PERF_METHODOLOGY_VS_FOSS.md`。**

### 强制流程

```
N = 0 (round counter)

Round N+1: surgical polish
  - 改 1-3 处 hot path
  - bench validate
  - 真动针(>1.5× variance)= continue
  - 没动针 = N+=1

IF N >= 2:
  STOP polish
  SWITCH to decomposition:
    1. Read FOSS competitor source (PG18 / MySQL / etc)
    2. Spawn decomposition (read-only):
       - 18+ stage × file:line × atomic-op-count side-by-side
       - validated ±20% against measured RTT
       - Top-N actionable attacks (file:line + change + µs + semantic + blast)
    3. Spawn attack (worktree-isolated):
       - atomic implement all attacks
       - bench validate cumulative
    4. Re-bench full 13-shape sweep
    5. If still gap > 5% AND N+1 round done: GOTO N=0 with new attacks
```

### CI 自动产 decomposition skeleton

(v7.38 P5 期间实施)— perf gate failure 触发时,CI fail 报告同时挂一份 decomposition skeleton(空 stage 表 + 提示 file:line 候选),便于开发者 5 min 内开始填。

### 触发词 lint(可选,v7.38 P6 期间评估)

`xtests/lint-perf-acceptance/` — scan PR / commit / ack note,匹配 `docs/PERF_METHODOLOGY_VS_FOSS.md` §4 触发词清单即 fail。

---

## 7. Ship gate(per v7.38 plan §七)

v7.38 release 前必须全绿:

1. `scripts/gate.sh all`(fast tier,< 5 min on mini)
2. `scripts/gate.sh all --full`(full tier,无上限)
3. **4 元机制 acceptance**(本文档 §1 每条 acceptance)
4. **8 轴 acceptance**(本文档 §2 每条 acceptance)
5. **本文档 v2 替代 v1**(`docs/TESTING.md` 立法 ✓ + v1 redirect stub)
6. mailrs zero-change 验收(同 7.37.x)
7. dropin 57+/57+(无退化)
8. CHANGELOG + crates publish + docker multi-arch + dropin report

---

## 8. 维护

- **新增测试 → 必选 1 元机制 + 1 轴 + 1 工艺纪律 + 1 速度预算 category**;无法归类的 → 设计缺陷,先补设计
- **历史 regression → 必有 round-NN test + 工艺 #4 归属**
- **本文档每次 v7.38+ 期间被 amend** → CHANGELOG 同步 + plan §七 acceptance row 更新

---

End of TESTING v2 skeleton. v7.38 P0-P6 期间 fill / verify each placeholder.
