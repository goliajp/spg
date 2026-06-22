# Testing v2 — live dashboard (v7.38 在建)

> **作用**:这是 `docs/TESTING_V2_SKELETON.md` 的活态镜像 + 进度审计。
> 每条 acceptance criterion 写当前实际状态 (`✅` LANDED / `⚠️` PARTIAL /
> `❌` NOT STARTED) + evidence (file:line 或 commit SHA)
> + 学术 TDD 视角 gap 评注。
>
> **铁律**(回应 2026-06-22 用户反馈 "v7.38 要时刻回顾整体性,要有学术精神"):
> 1. **百分比不撒谎**:`1338/1338 pg_regress PASS` ≠ `100% PG 兼容`。它只是
>    "我们移植到 corpus 的子集对 SPG 自己 pass"。学术 TDD 区分 *self-consistency*
>    vs *ground-truth conformance*。
> 2. **缺缺口必须可见**:任何未 wire 的 stub / 任何 skip 都写在表里,不许
>    隐藏在 "successfully completed" 后面。
> 3. **每行一证据**:模糊的 "已基本完成" 不算 ✅;必须给 file:line / commit SHA。
> 4. **覆盖率 ≠ 兼容率 ≠ 正确率**:三者是独立维度,dashboard 各列分别报。
>
> **更新时机**:每个 v7.38 train commit 触碰本表所引文件时,同 commit 更新。
> **本表退化(✅→⚠️ / ⚠️→❌)= release block**。

Last update: 2026-06-23 (post v7.37.8 mailrs lock-hang 4th-recurrence
closure — `SPG_WAL_ROW_REDO` default flipped ON, ending 3 releases of
sealed-fix negligence; dogfood-replay gate now blocks `release.sh`
preflight)
Branch: `feature/v7.38-test-constitution` @ commit `fcc9c3b`

**v7.37.8 incident-driven additions** (all defended in this dashboard now):
- 元机制 + 工艺纪律 add new row: **release.sh preflight runs `gate.sh
  dogfood`** — release blocked on fail. Closes mailrs ask P2 ("prod-shape
  catalog in SPG CI for any change touching open_path / WAL replay /
  ACTIVE_OPEN_PATHS"). Operator can override with `SKIP_DOGFOOD=1` after
  triage, never silently.
- New memory `feedback-no-sealed-fix-behind-env-var`: perf / correctness
  fixes targeting the dogfood customer must ship default ON; opt-in env
  vars hide the fix from `0-let-fall` consumers.

---

## 0. Honest TL;DR

| 大块 | LANDED | PARTIAL | NOT STARTED | 加权完成度 |
|---|---|---|---|---|
| 4 元机制 acceptance | 7 | 8 | 6 | 33% (acceptance bar) |
| 8 轴 acceptance | 1 | 6 | 16 | ~8% |
| 工艺纪律 10 条 | 0 | 1 | 9 | 5% |
| 速度预算 4 项 | 0 | 2 | 2 | 25% |
| Ship gate 8 项 | 1 | 2 | 5 | 19% |

**最大缺口**(按学术 TDD 杠杆排序;2026-06-23 更新):
1. ~~**元机制 C 三主差分 oracle 完全是 stub**~~ ✅ **CLOSED 本 turn**(`run_on_spg` 实装 + 3/3 oracle fixture PASS 100% vs PG18 baseline + R32 closed)。剩 MySQL/MariaDB sqlx adapter 没建,但 PG18 已经从 self-consistency 升级到 conformance。**oracle corpus 4 fixtures 真小** — 下一步 port 多 fixture 才能让 PG18 conformance 形成体量。
2. **轴 4 隔离 / 并发 整轴未起步** — Hermitage / Elle / PG isolationtester 全 0。这是数据库 TDD 的核心轴,缺它 = 没有 anomaly detection。
3. **工艺纪律 10/10 几乎全空** — 现有测试不走 100×rerun / check-testcase / iter-OOM / 三模式同输出 / adjust_*() 五大学术 TDD 基础设施。
4. **历史 regression 归属(纪律 #4)**:R32/R33/R34/round-NN.slt **全部未入 corpus**(注意:R32 在 oracle 单 fixture 里已被 anchor,sqllogictest corpus 里仍无 round-32.slt 归属)。下一次 cascade 再发,我们仍没法机械 detect。

---

## 1. 元机制(P0,先建)

### A. Injection points

| Acceptance | 状态 | Evidence | Gap |
|---|---|---|---|
| 编译 release 时 `injection_point!()` 展开 `()`,zero overhead | ⚠️ | `crates/spg-engine/src/testkit/injection.rs` macro defined; release behaviour by `#[cfg(feature)]` gate | **未做汇编验证** — 应 `cargo asm` 抽一段 release build 确认 |
| 测试构建 feature `injection-points` on,4 种 action (attach/wakeup/error/notice) 全支持 | ✅ | `testkit/injection.rs::Action enum` + lib test `tests::sql_injection_attach_notice_then_trigger_records` (PASS feature-on) + `tests::sql_injection_attach_off_feature_errors` (PASS feature-off) | — |
| ≥ 8 个接进点 | ✅ 9/8 | grep `injection_point!` (incl. cross-crate via spg-embedded re-export) → 9 sites:`tx_commit_walgroup_leader_switch` / `wal_group_commit_leader_chosen` / `index_build_post_seal` / `aggregate_spill_trigger` / `planner_first_row_fetch` / **`checkpoint_cow_swap_pre`** / **`checkpoint_cow_swap_post`** / **`cold_tier_wakeup_resume`** / **`spg_sqlx_inline_budget_cancel`** (last 4 added this turn) | Acceptance ≥ 8 met. Skeleton-listed `prefetch_sequential_scan_threshold` / `segment_forward_disconnect_resume` defer to when those flakes resurface |
| 每接进点 1 testcase 验证 attach + wakeup 可见 | ⚠️ 1/9 | 仅 `aggregate_spill_trigger` 有 lib test 验证 (`tests::sql_injection_attach_notice_then_trigger_records`) | **缺 8 接进点的 1-testcase** — 但 lib test 已 prove framework correctness;per-site test 延后 |

### B. Permutation matrix runner

| Acceptance | 状态 | Evidence | Gap |
|---|---|---|---|
| 一份 sqllogictest corpus N× 跑(N = permutation 数)全绿 | ⚠️ 1/3 | `xtests/perm-runner/src/runner.rs::run_one_fixture_embedded` 实现;`server_simple` / `server_extended` permutation 返回 `PermStatus::SkippedPending` | **缺 server 桥** — `xtests/perm-runner/src/runner.rs` `run_one_fixture_server_*` 全是 stub。需要桥到 `spg-server` 二进制 + sqlx |
| TOML 配置接进 CI,新增 permutation 无 code change | ⚠️ | TOML schema 已定义 `tests/permutations.toml`(从 plan + skeleton 引用) | **未接进 `gate.sh`**;dev 跑 corpus 默认走 `cargo run --bin sqllogictest`(不走 perm-runner) |

### C. 三主差分 oracle

| Acceptance | 状态 | Evidence | Gap |
|---|---|---|---|
| PG18 oracle 跑通,≥ 80% port | ⚠️ 3/3 currently | `cargo run --bin spg-oracle-runner -- run --oracle pg18` → **3/3 fixtures 100% PASS**(commit `[本 turn]`);`run_on_spg` 实装 in `xtests/oracle/src/runner.rs::run_on_spg` (Engine + psql-aligned format + depd directive resolver) | 80% port 目标按 PG regress 文件数计,当前 oracle corpus 只 3 fixtures;**需大规模 port 才能达 80%** |
| MySQL oracle 跑通 | ❌ | docker-compose 含 MySQL service | sqlx-mysql adapter 未建(`run_on_oracle` 仍 stub for MySQL/MariaDB) |
| MariaDB oracle 跑通 | ❌ | docker-compose 含 MariaDB service | 同上 |
| R32 历史 regression 进 `port.subquery_correlated_agg.spg.out` 标 EXPECTED FAILURE,修后退步立刻被捕 | ✅ **CLOSED** | R32 EXPECTED FAILURE lock **DELETED 本 turn** — SPG output 实测 byte-equal 对 PG18 baseline after AdjustWhitespace;`port.subquery_correlated_agg.sql` 现 enforce PG baseline 直接 | R32 closed; outer-agg correlated subq dialect 在 v7.37.x 周期(K02 fix + planner work)悄悄修好了 — dashboard 触发的 audit 第一次正式 detect 这点 |
| adjust_*() 归一化框架 | ⚠️ 2/8 working | `xtests/oracle/src/normalise.rs::AdjustWhitespace` + `AdjustOrderingViaSort` 实现并被实际差分用上;其他 6 step (`timestamps` / `seqs` / `dollar-quoted` / `explain-costs` / `float-repr` / `null-display`) 仍 stub | 6 step 等真 corpus 触碰才补 |
| self-diff(fast-tier 替代,跨 SPG 3 perm 自洽) | ❌ | `xtests/oracle/src/self_diff.rs::run` 仍返回 `Err("self-diff: stub — depends on v7.38 元机制 B...")` | 元机制 B lib 已 expose(commit `d9d1ed8`);next move = wire self-diff to call `perm-runner::run_permutation` |

### D. 测试模式 GUC

| Acceptance | 状态 | Evidence | Gap |
|---|---|---|---|
| ≥ 7 旋钮接进 engine,index 写每个 acceptor 行号 | ⚠️ 6/8 LANDED | `xtests/sigil/test-mode-gucs.md`:`EXPLAIN_NO_COSTS` `DISABLE_TOPK` `RANDOM_SEED` `DISABLE_JOINFOLD` `STATS_FROZEN` `PLAN_DETERMINISTIC` LANDED;其余 2 (`COMPUTE_QUERY_ID` `PREFETCH_DETERMINISTIC`) TBD | `COMPUTE_QUERY_ID` 等 SPG 加 query_id annotation;`PREFETCH_DETERMINISTIC` 等 prefetch path 落 surface |
| 每旋钮 1 testcase 验证关掉对应 surface | ⚠️ 6/8 | `e2e_env_cfg_explain_no_costs` / `_disable_topk` / `_random_seed` / `_disable_joinfold` / `_stats_frozen` / `_plan_deterministic` 都已 PASS | 同上 2 个 TBD |

---

## 2. 八轴

### 轴 1 — SQL 标准 conformance

| Acceptance | 状态 | Evidence | Gap |
|---|---|---|---|
| 三 oracle 全绿 | ❌ | 元机制 C 未 wire | — |
| ≥ 80% PG `src/test/regress/sql/` port | ⚠️ 38% | 88 / 233 PG 文件;1338 records / 100% PASS in sqllogictest runner | **缺 ~100 file port** — 学术诚实:`100%` 是 self-consistency,不是 PG conformance |
| R32/R33 历史 regression `.slt` 归属 | ❌ | `grep -r "R32|R33|R34" xtests/sqllogictest/corpus/` → 0 文件命中 | mailrs cascade 4 次复发 + R32 outer-agg correlated subq + R33 inlist + R34 等历史 bug **均未进 corpus**;cascade 复发只在 `xtests/dogfood_replay/fixtures/` 命名,不在 sqllogictest 标准 corpus |

### 轴 2 — 三方言 specifics

| Acceptance | 状态 | Evidence | Gap |
|---|---|---|---|
| 三方言 oracle 全绿 | ❌ | 元机制 C 未 wire | — |
| mailrs R12-R34 全部测有归属 (`round_NN.slt`) | ❌ | `ls xtests/sqllogictest/corpus/*/round_*` → 无文件 | **23 个 round 历史 0 归属**;PG `100_bugs.pl` 约定缺失 |
| 三方言独有特性各一组 | ⚠️ | duckdb 21 / mysql 15 / pgvector 9 file | 是 baseline 而非 specifics |

### 轴 3 — 连接池

| Acceptance | 状态 | Evidence | Gap |
|---|---|---|---|
| 64 并发 60s 0 泄漏 | ❌ | `xtests/sqlx-pgwire/tests/` 只 `p0_mailrs_prod.rs` + `smoke.rs`;无 `pool_stress.rs` | 未起步 |
| 服务重启 client 自动回弹 | ❌ | 同上 | — |
| mid-query 弃连不破坏状态 | ❌ | 同上;也未走 `injection_points` chaos | — |

### 轴 4 — 隔离 / 并发(基本未起步,gap-tracker 建立)

| Acceptance | 状态 | Evidence | Gap |
|---|---|---|---|
| PG `src/test/isolation/specs/*.spec` 直接 vendor 跑通 | ❌ | `xtests/isolation/` **目录不存在** | — |
| Hermitage 11 case × 3 iso level = 33 格 | ❌ | 同上 | — |
| Elle on `list-append` + `rw-register` | ❌ | 同上 | — |
| Jepsen postgresql-12.3 "fresh insert G2-item" 反向测试 — SPG **不修复** | ❌ | 同上;**重要**:SPG STABILITY.md 未明确列哪些 anomaly 故意不修(PG 兼容意味的) | 设计补 + 测试补 双坑 |
| 轴 4 SQL surface | ✅ **LANDED v7.37.8** | `xtests/sqllogictest/corpus/spg_baseline/16_isolation/set_transaction_isolation_level.test` 18/18 PASS;parser 接所有 4 standard iso levels(`READ UNCOMMITTED`/`READ COMMITTED`/`REPEATABLE READ`/`SERIALIZABLE`)+ `READ ONLY/WRITE` + `[NOT] DEFERRABLE` + comma-separated combos;`Engine::current_isolation_level()` accessor;`SHOW transaction_isolation` 返回 PG-canonical string(`read uncommitted` silently upgrades 同 PG)| MVCC / SSI 真 isolation 语义(REPEATABLE READ snapshot / SERIALIZABLE SSI)— v7.38 isolation framework 单独 train |

### 轴 5 — 事务一致性 + 崩溃原子性(整轴未起步)

| Acceptance | 状态 | Evidence | Gap |
|---|---|---|---|
| Rust 版 `SpgCluster::{new,init,start,stop,kill9,promote,wait_for_catchup}` | ❌ | `xtests/recovery/` **目录不存在** | — |
| PG `013_crash_restart.pl` 抄过来 | ❌ | 同上 | — |
| PG `027_stream_regress.pl` 抄过来(sqllogictest 全 corpus 作负载 + crash + diff)— 这条是 SQL 覆盖 → WAL 语义覆盖的杠杆 | ❌ | 同上 | **整 v7.38 计划里的最高 ROI 一条都未建** |
| Jepsen lite invariants(bank / counter / register),1k kill 0 violation | ❌ | 同上 | — |

### 轴 6 — Dump / import

| Acceptance | 状态 | Evidence | Gap |
|---|---|---|---|
| `%pgdump_runs × %tests` 矩阵 | ⚠️ | `xtests/dump_compat/` 存在,有 `PHASE_9_PLAN.md` 表明 phase 进行中;矩阵未达 | partial scaffolding |
| `adjust_*()` 归一化框架 | ❌ | `xtests/oracle/src/normalise.rs` 是 stub;`xtests/dump_compat` 内也无 adjust_*() | — |
| GB 级 round-trip byte-equal | ❌ | `xtests/data_compat` 有 setup 但 GB 级未跑 | — |
| mysqldump / mariadb-dump 全 schema | ⚠️ | `xtests/dump_compat/{mariadb,mysql}/` 目录存在 | 完整度未审计 |

### 轴 7 — 灾难恢复(整轴未起步)

| Acceptance | 状态 | Evidence | Gap |
|---|---|---|---|
| crash recovery 系统化 | ⚠️ | v7.37.x WAL replay e2e 散在 `crates/spg-embedded/tests/`;非系统 | 散点 |
| PITR | ❌ | mailrs ask #2 历史坑 (memory ref) — 未闭 | — |
| 半写页检测 | ❌ | 无 VFS-layer journal-test | — |
| basebackup / restore round-trip | ❌ | — | — |
| 坏 page detection → quarantine | ❌ | — | — |
| injection_points 模拟 recovery 中再次 OOM/IO error | ❌ | injection_points 已建 framework,但 recovery path 未打点 | 仅需要在 recovery path 加 3-5 `injection_point!` |

### 轴 8 — Perf 四层

| Acceptance | 状态 | Evidence | Gap |
|---|---|---|---|
| 8.1 原子 Criterion microbench:每算子 ns/row | ⚠️ | `crates/spg-engine/tests/perf_gate/` 30 files,但不是 Criterion 格式 (自家 timer + budget) | **范式不同**:plan 要 Criterion + bootstrap CI,SPG 现用自家 budget assertion |
| 8.2 简单 e2e p50/p95/p99 | ⚠️ | 同上 30 files 走自家 budget 体系 | **未报 p50/p95/p99 三档** |
| 8.3 高压 stress (pgbench / sysbench) | ❌ | 无 pgbench/sysbench 集成 | — |
| 8.4 大表 scale (TPC-C / 自家 1M/10M) | ⚠️ | `xtests/dogfood_replay/` 走 mailrs prod snapshot,自家 large-scale 部分 ok;TPC-C / BenchBase 未起 | — |
| 复现性栈强制 (isolcpus / nohz_full / numactl / governor / ASLR / THP / mitigations) | ❌ | mini 上未配 | — |
| planner-cost regression 类 (DuckDB tpch_plan_cost 思路) | ❌ | — | — |
| 三栏 PG18 / SPGS / SPGE 报表沿用 | ⚠️ | 13-shape baseline LOCKED at v7.37.4(skeleton 内) | 报表 generator 未自动化 |
| `docs/PERF_METHODOLOGY_VS_FOSS.md` 工作流强制 | ✅ | v7.37.7 K02 attack 严格走 decomposition → counter-first → attack (memory: `feedback-counter-first-not-samply`) | 已工作流化 |

---

## 3. 工艺纪律(10 条)

| # | 纪律 | 状态 | Evidence | Gap |
|---|---|---|---|---|
| 1 | check-testcase between every test | ❌ | 无环境不变量断言 framework | — |
| 2 | 三模式同输出(release / debug-asserts / sanitizer 字节相等) | ❌ | CI 只跑 release | — |
| 3 | `testcase!()` / `ALWAYS!()` / `NEVER!()` 宏 | ❌ | — | — |
| 4 | 每历史 regression 一条归属测试(`100_bugs.pl` 约定) | ⚠️ | `xtests/dogfood_replay/fixtures/` 有 4 + Class B/C 但是 stress 形态;非 sqllogictest 标准 corpus 归属 | **核心缺口** — 见上 轴 1+2 |
| 5 | `adjust_*()` 归一化 + 全等 diff | ❌ | 无 | — |
| 6 | iter-OOM / iter-IO 故障注入循环 | ❌ | 无 | — |
| 7 | 100× rerun flaky-gate | ❌ | 无 | — |
| 8 | dbsqlfuzz 形态 fuzz 入树语料库 | ❌ | 无 `tests/fuzz/corpus/` | — |
| 9 | `PG_TEST_EXTRA` opt-in 桶 | ❌ | 无 | — |
| 10 | `require <ext>` / `require-env` self-skip | ❌ | 测试缺 docker 时直接 fail,不是 self-skip | — |

---

## 4. 速度预算

| 档位 | acceptance | 状态 | Gap |
|---|---|---|---|
| gate.sh fast ≤ 5 min(mini) | ❌ 未测 | `scripts/gate.sh` 跑,但**无 wall-clock 自闸**(`bench-budget-check` 任务未建) |
| gate.sh full ≤ 无上限 | ✅ | 默认 |
| bench --fast ≤ 30s | ⚠️ | `scripts/test-on-mini.sh` + `cargo test` 默认 release 测一组;无显式 `--fast` flag |
| Cargo dev cycle 不退化 | ⚠️ | `cargo check` clean(7m01s on mini),但 perm-runner / oracle 二进制现在编 — 加 perm-runner lib 后**还未实测影响** |

---

## 5. Ship gate(v7.38 release 前)

| # | 条件 | 状态 |
|---|---|---|
| 1 | `gate.sh all` (fast tier) | ⚠️ 未自闸 budget |
| 2 | `gate.sh all --full` | ⚠️ 未自动化 |
| 3 | 4 元机制 acceptance 全 ✅ | ❌ A(5/8 inject sites)/ B(1/3 perm) / C(全 stub) / D(3/8 GUC) |
| 4 | 8 轴 acceptance 全 ✅ | ❌ 1/8 大头剩 |
| 5 | 本文档 v2 替代 v1 | ⚠️ skeleton 写完,dashboard(本文)在建,v1 redirect 未做 |
| 6 | mailrs zero-change 验收 | ✅ v7.37.7 ack 已 sent |
| 7 | dropin 57+/57+(无退化) | ✅ v7.37.7 = 59/59 |
| 8 | CHANGELOG + crates publish + docker + dropin report | ✅ v7.37.7 模板已工作 |

---

## 6. 学术 TDD 视角 — 真正缺什么

**TDD 的灵魂三问**:
1. *Tests as specification* — 我们的 test 表达「SPG 应该如何」还是「SPG 当前如何」?
2. *Source of truth* — 每个 test 的 ground truth 是 *PG 文档 / SQL 标准 / 实测 PG 行为* 还是 *SPG 自家上次的输出*?
3. *Diagnosable failure* — fail 时,reader 能不能从 test 名 + 失败信息一句话还原 "我违反了 spec X 的 property Y"?

按这三问审 v7.38 当前 state:

- **轴 1**:1338 records 大都 ports 自 PG regress,所以 Q1 ✅;但 Q2 = SPG 自家 PASS(没跑 oracle diff)= ❌;Q3 = 失败信息 generic `row mismatch | expected: X | actual: Y`,无 spec 引用 ❌
- **元机制 C oracle**:理论 fully solves Q2,但全 stub = 没在执行
- **轴 4 isolation**:整个轴存在的理由就是 Q1 + Q3 — 用 PG `.spec` 格式定义 anomaly,失败信息 = "G2-item detected on level X",未起步
- **历史 regression 归属(纪律 #4)**:每条修过的 bug → corpus test;现 0 归属。**这是 academic TDD 的最严重缺失**

**结论 — 单步最高 ROI 行动**(本 turn / next turn 候选):

| 候选 | ROI 评级 | 工作量 | 直接解锁 |
|---|---|---|---|
| **wire 元机制 C oracle minimal**(run_on_spg + PG18 sqlx + 4 fixture diff) | ★★★★★ | 1-2 day | Q2 转 ✅;后续所有 轴 1 / 2 records 升级为 conformance |
| **wire self-diff 走 perm-runner lib**(已 unblock) | ★★★ | ~1 day | 轴 1+2 在 fast tier 有 self-consistency 检查(不是 ground truth) |
| **将 mailrs R12-R34 + cascade 4 次 fixture 移入 sqllogictest corpus 标准归属** | ★★★★ | 半 day mechanical | 纪律 #4 转 ⚠️;Q3 改善 |
| **补 3-5 inject sites + 4-5 GUC + recovery 打点** | ★★ | 1-2 day mechanical | 元机制 A 转 ✅,D 转 ✅;为后续 轴 5/7 铺路 |

**academic TDD 视角下推荐顺序**:wire oracle minimal(★★★★★)→ 历史 regression 归属(★★★★)→ self-diff(★★★)。

---

## 7. 维护

- 本 dashboard 每次 v7.38 commit touch 任一文件路径必须更新对应行
- ✅ → ⚠️ / ⚠️ → ❌ 退化 = release block;不允许藏在 PR 描述里
- 新增 acceptance 必须同步 `docs/TESTING_V2_SKELETON.md`(spec)+ 本文(状态)
- 现 dashboard 是 v7.38 ship gate #5 的待替代物
