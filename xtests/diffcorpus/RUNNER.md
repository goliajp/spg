# Differential corpus — SPG vs live PG18

Moved here in round 666, from `.claude/state/diffcorpus/`.

It is one of the six release gates and it was the only one not under version
control: `.gitignore` excludes `.claude/` wholesale, so 21 SQL files and the
runner lived outside the repo while the other five gates all sit under
`xtests/`. The cost was not hypothetical — a single session lost this
directory twice to `rsync --delete` when syncing to the test machine, and
each time it came back only because a copy happened to survive in a
scratchpad. The five gates beside it were never at that risk.

`out/` stays ignored; it is regenerated on every run.

---

# 差分 runner 设计

## 判定面
SPG(pg-wire :26000)对 PG18(:25432),同一份 .sql,逐份 diff。

## 三条必须内建的归一化(每条都源于实际踩过的坑)
1. **NULL 与空串不可混淆** —— psql `-tA` 把 NULL 打成空行,已导致三次 pin 期望写错。
   runner 统一 `\pset null '<NULL>'`。
2. **标记必须走同一条缓冲流** —— `\echo` 无缓冲、结果块缓冲,两流交错会把时间/输出贴错查询(r589)。
   语料内用 `SELECT 'Tnn';` 打标记。
3. **错误比对归一化** —— PG 带 `LINE`/`^`/`HINT`/`DETAIL` 装饰而 SPG 不带。
   runner 只保留 `ERROR:` 那一行的消息体,装饰行丢弃;SQLSTATE 另行比对。

## 差异三分类(每条差异必须落进其一)
- **NEW-DEFECT** — 静默错答案 / 多余报错 / 缺失报错 ⇒ **无条件开正确性条目**(不受收敛闸约束)。
- **KNOWN** — 已在 checklist §9 已记账分歧 ⇒ 忽略。
- **NEW-DIVERGENCE** — 新发现但判定为「SPG 更早/更严格/环境差异」⇒ 追加进 §9,**必须附实测依据**。

## 运行
`bash diffcorpus/run.sh [file.sql ...]` —— 不给参数则全跑。产出 `out/<name>.{spg,pg,diff}`。

## `15-catalog` went 4 → 2 on 2026-08-23, and the 2 are classified

Two of the four were `pg_settings` rows PostgreSQL had and SPG did not;
v7.38.18 gave `pg_settings` and `SHOW ALL` PostgreSQL 18.4's full list,
so they stopped differing.

The remaining two are ONE line moving: T12 asks for three parameter
names `ORDER BY 1`, and the oracle answers `client_min_messages,
DateStyle, search_path` while SPG answers `DateStyle,
client_min_messages, search_path`. That is not a sort defect. The
oracle's database collates as `en_US.utf8` and SPG's collates as `C`,
and byte order puts `D` before `c`. SPG performs `en_US.utf8` correctly
when a column or an ORDER BY key asks for it — measured, matching PG on
all four values — but its database-level collation is fixed at `C` and
nothing can change it.

That is a real divergence and a wide one, written up with its options in
`docs/FINDING-2026-08-23-database-collation.md`. It stays in the
baseline as a NUMBER only because the file has no room for a reason;
this paragraph is the reason, and the number should go to 0 when the
finding is closed rather than being re-baselined again.
