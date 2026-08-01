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
