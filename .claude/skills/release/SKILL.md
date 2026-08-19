---
name: release
description: SPG 发版全链 skill — suite prerelease 闸起步,九步幂等可重入,到客户信件定稿为止(发送永远留给用户)。
---

# SPG release — 全链九步(7.38 S5.4 固化)

**两条路**:默认全链(下文九步),以及 `--fast`(见文末「快速路」)。
`--fast` 是为**线上重大缺陷的 hotfix** 和**全新东西要尽快到真实使用面前**
准备的;它换来的是速度,付出的是证据。不要因为赶就用它 —— 要因为
「等待的代价高于窄闸的代价」这个**针对本次发布**的判断才用它。

每步幂等可重入:重入时先探测「这步是否已完成」,完成则跳过,不撞。
任何一步红:停,报「停在第几步 + 日志路径 + 修好后从哪步重入」。
不使用 SKIP_* 逃生门,除非用户明示(r1041 裁定)。

## 第 0 步 — 闸(红即止)

> `--fast` 走的是 precommit 档,见文末快速路;以下是默认全链。

```sh
scripts/suite.sh prerelease --on-mini     # 或 mini 上直接 suite-run prerelease
scripts/suite.sh --result                 # 哨兵读结果;RUNNING 则等
```
全绿才继续。7.38 起这是所有版本的闸(CP5 立法)。

## 1 版本定夺

- CHANGELOG.md 未发布节 → 定 X.Y.Z(未发布节是唯一「下一版内容」台账)。
- crates.io 限流窗:当日已发版本数 ≥5 慎发(r1023:78 次/日限,
  12 crates/版;撞 429 = 等窗**补发同一版本**,不升号)。

## 2 bump

```sh
# workspace.package.version 一处改;lock 同步;CHANGELOG 定稿(未发布节 → 版本节 + 日期)
sed -i '' 's/^version      = "OLD"/version      = "NEW"/' Cargo.toml
cargo metadata --format-version 1 >/dev/null   # 同步 Cargo.lock
```
S2.6 chore:本版 Fixed 条目的 SQL 逐条进语料(15_regressions 或对应目录),同 commit。

## 3 git-flow(驻留 develop;严格 start/finish;不用 PR)

```sh
git flow release start X.Y.Z
git commit -am "chore(release): vX.Y.Z"        # bump+CHANGELOG+语料 chore
git flow release finish -m "vX.Y.Z" X.Y.Z
# tag 前缀纠正(git-flow 掉 v 的既往坑):
git tag | grep -qx "vX.Y.Z" || { git tag "vX.Y.Z" "$(git rev-parse X.Y.Z)" && git tag -d X.Y.Z; }
git rev-parse "vX.Y.Z" >/dev/null              # 校验 tag == master HEAD
```
重入探测:tag vX.Y.Z 已存在且 == master HEAD → 跳过本步。
已 push 的 tag 永不 retag;发现回归 → cut 新版本(v7.37.25 先例)。

## 4 push(flow 内视为已授权)

```sh
git push origin master develop --follow-tags
```

## 5 perf 腿自起(不依赖操作者环境)

```sh
# pgwire 7001 / 原生 7002 + 本机 PG 容器;两腿 SELECT 1 自检后才继续
mkdir -p /tmp/spg-release-leg && rm -rf /tmp/spg-release-leg/*
SPG_PG_ADDR=127.0.0.1:7001 ./target/release/spg-server 127.0.0.1:7002 \
  /tmp/spg-release-leg/db /tmp/spg-release-leg/audit /tmp/spg-release-leg/wal &
export PG_URI='postgres://bench:bench@127.0.0.1:25432/bench'
export SPG_URI='postgres://bench:bench@127.0.0.1:7001/bench'
psql "$SPG_URI" -c 'SELECT 1' && psql "$PG_URI" -c 'SELECT 1'
```

## 6 release.sh(幂等列车)

```sh
PERF_REQUIRED=1 scripts/release.sh X.Y.Z
```
preflight 拒脏树(gate 刷新的 report 先按 TOOLCHAIN §2.3 处置)→
dogfood → gate all → crates×12 拓扑 → buildx 三 tag → drop-in 59 面板 → checklist。
半失败直接重跑同命令(已上 crates 的自动跳过)。

## 7 收尾

```sh
pkill -f "spg-server 127.0.0.1:7002"; scripts/janitor.sh   # 清点上报,永不静默
# 报告 + 刷新生成物(dropin report / sqllogictest report)chore commit 入 develop 并 push
```

## 8 台账

- autorun ledger(memory project-autorun-resume)追记本版一段;
- 数据目录 fixture 刷新:用**新 tag 的二进制**捕获 xtests/compat-datadirs/vX.Y.Z/
  (S3.2 协议),旧版目录保留。

## 9 信件

客户 reply/ack 定稿(版本号 + image digest 填入),写 docs/,
**fullpath 报给用户;发送永远留给用户**。

## 教训表(全部踩过,见 RELEASE-FLOW §3)

tag 丢 v 前缀 / preflight 脏树 / PERF URI 缺失硬红 / crates 429 /
半失败列车重入 / 已 push tag 不动 / 发布后僵尸腿 —— 各步已内建对策。

## 附:本机邻载红的证据通道(7.38.0 实战成文,7.38.1 S1.4 固化)

症状:preflight `gate.sh all` 在 server e2e 报成批
`server didn't publish native listen addr within Ns` —— 且同测试
**单跑绿**、mini 上绿。这是 spawn 风暴 × 本机邻载(并行 cargo /
其他会话)挤爆启动窗,不是代码红。

处置顺序(不许直接 SKIP):
1. 先复核:单跑失败测试 + `uptime`(load>10 即嫌疑成立);
   已内建缓解:`gate.sh e2e` 缺省 `RUST_TEST_THREADS=6` +
   `SPG_TEST_SPAWN_DEADLINE_SECS=30`,可再调大。
2. 仍红 → 在 mini 对**同一 tag** 跑全类:
   `git checkout v<X.Y.Z>` + `PERF_REQUIRED=1 gate.sh all`
   (perf 段需 `PSQL=$HOME/spgbench/bin/psql` docker 包装器 +
   host.docker.internal 路由 + 0.0.0.0 腿)。
3. mini 全绿(逐类推进到 perf 即前六类绿;perf PASS 单独确认)
   → 本地 `SKIP_FULL=1 scripts/release.sh <ver>` 续列车,
   commit/日志里**引用 mini 报告路径**。
4. mini 也红 → 是真红,停列车修。

SKIP_FULL 只能走这条带证据的路;裸 SKIP = r1041 裁定禁止。


---

## 快速路 —— `--fast`

```sh
# 第 0 步照跑 precommit(不是 prerelease),其余九步同样走
scripts/suite.sh precommit                 # 150 s 硬顶
# …第 1-5 步不变(定版本 / bump / git-flow / push / perf 腿)…
PERF_REQUIRED=1 scripts/release.sh X.Y.Z --fast
```

### 它跑什么

precommit 档,150 秒硬顶:`fmt` · `clippy-affected` ·
`unit-affected`(受影响 crate 的单元测试)· `pins-current`(**本版新钉**)·
`slt-smoke`(语料子集)· `ironrule-smoke`(wire 应答 / WAL 真落 / 零列结果集)。
preflight(拒脏树、校 tag == master)、crates×12、buildx 三 tag **一律照跑** ——
产物本身和全链完全一样。

### 它不跑什么(这才是重点)

- **dogfood 生产形态重放** —— 历史上 mailrs 那次 P0 第四次复发就是跳这个跳出来的
- **`gate.sh all`** —— lint / unit / e2e / gates / biz 五类的完整格
- **perf 闸** —— 本版没有与 live PG18 比过
- **drop-in 59 格验收面板** —— 没有对着刚推的镜像验过

一句话:**这个构建做过冒烟,没做过发布电池**。

### 它欠下的账(不是可选项)

1. 赶的理由一消失,立刻对 tag `vX.Y.Z` 跑
   `scripts/suite.sh prerelease` + `scripts/dropin-acceptance.sh`。
2. **红了就发下一版,永不 retag** —— tag 和 crates 都不可撤回。
3. 发布说明里**明说本版走的是快速路**,免得别人把这个版本号读成
   带着通常那份证据。

release.sh 在 `--fast` 下会把上面三条打进收尾 checklist,并在 preflight
之后就把「没跑什么」原样喊出来 —— 不让一个窄闸的构建看起来跟宽闸的
一样。

### 与 `SKIP_*` 的区别

`SKIP_FULL=1` 等逃生门仍受 r1041 裁定约束(只能走带证据的邻载红通道)。
`--fast` 是**被明确授权的、有名字的、会自我声明的**那条路 —— 它把
「我知道我在少跑什么」写进日志和 checklist,而不是让一次跳闸悄悄发生。
