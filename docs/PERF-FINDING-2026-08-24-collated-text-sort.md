# A collated text sort is 4x behind PostgreSQL, and its top-N is 120x

**Status: CLOSED for the alphabet that covers the common cases, on
2026-08-24. Three of the five sort shapes now beat PostgreSQL 18. The
sort panel's worst ratio went 78.14x to 2.39x and `sort_over_ceiling`
3 to 0.**

|  | before | after | PostgreSQL 18 |
|---|---:|---:|---:|
| `ORDER BY s_long LIMIT 10` | 1357.5 ms | **43.2** | 11.4 |
| `ORDER BY s_long` (full) | 1706.5 | **395.6** | 415.9 |
| `ORDER BY s_short LIMIT 10` | 114.0 | **14.7** | 21.9 |
| `ORDER BY s_short` (full) | 446.1 | **211.0** | 502.4 |

What closed it is attack 1 below, taken with the guard it asked for.
The rest of this document is the reasoning as it stood before, kept
because two of its three readings were wrong and the record of that is
worth more than a tidy page. See "What it turned out to be" at the end.


Found on 2026-08-24 by the locale-collation panel added in v7.38.19 —
which could not have found it a day earlier, because the panel was
comparing one server against itself. See "How it stayed hidden".

## The measurement

400,000 rows. `s_long` is six concatenated md5 hex strings, 192
characters, all distinct. Every leg verified to hold 400,000 rows before
anything was timed. Idle machine, min of five.

| | SPG `en_US.utf8` | SPG `C` | PostgreSQL 18 `en_US.utf8` |
|---|---:|---:|---:|
| no sort (`ORDER BY id LIMIT 10`) | 0.185 ms | 0.113 | 0.214 |
| `ORDER BY s_long LIMIT 10` | **1329.5** | 17.8 | **11.0** |
| `ORDER BY s_long` (full) | **1687.8** | 353.9 | 418.1 |
| `ORDER BY s_long COLLATE "C"` | 350.7 | 351.8 | 127.7 |
| `ORDER BY s_short` (9 chars) | 289.0 | 212.1 | **493.9** |
| `ORDER BY left(s_long,16)` | 468.6 | 254.3 | 356.4 |

Read the last two rows before the first. On SHORT text we are nearly
twice as fast as PostgreSQL, and sorting on a 16-character prefix of the
long column costs a quarter of what sorting the whole column costs. The
cost is proportional to the length of the string, and PostgreSQL's is
not.

The top-N row is the headline: **120x**. Theirs barely sorts (11 ms);
ours costs nearly what its own full sort costs (1329 against 1687).

## What it is not

- **Not the key building.** v7.38.19 made a collated sort precompute ICU
  sort keys — n transforms instead of n·log n collator calls. The
  obvious next step was "a top-N makes only n comparisons, so keys buy
  nothing there; leave the text and let the comparator collate."

  Implemented, measured: **1334.3 → 1320.3**. Nothing. Reverted.

  The branch was proven to be taken (a `panic!` in it killed the server
  on exactly the LIMIT query and not on the un-LIMITed one), so this is
  a refuted hypothesis and not a missed edit. What it establishes is
  that **one ICU comparison costs about what one ICU sort key costs** on
  these strings — roughly 1.6–3.3 µs either way.

- **Not the top-N machinery.** The same query with `COLLATE "C"` runs
  through the same path in **16.5 ms**. Disabling the top-k fast path
  (`SPG_TEST_DISABLE_TOPK=1`) moves the collated case 1323 → 1676, so
  the path is engaged and is saving something; it is just saving it off
  a number that should not be there.

## What it is

ICU, on long strings, under the options PostgreSQL's collations need.

PostgreSQL uses glibc's `strcoll`/`strxfrm` for `en_US.utf8`. We use
ICU, because it is the only way to get PostgreSQL's ORDERING right
without a libc dependency — and v7.39 round 684 established the options
by sweeping all five against PG, landing on
`AlternateHandling::Shifted`. Shifted handling is what makes `_under`
sort between `cherry` and `Zebra` the way PostgreSQL does. It also means
a comparison cannot always stop at the first differing character, since
variable weights only settle at the quaternary level.

So the per-character cost is real work, correctly chosen, and about ten
times what glibc charges.

## Attacks not yet tried

1. **An ASCII fast path.** For strings drawn from `[0-9a-z]` only, the
   `en_US` collation order and byte order agree — digits before letters
   in both, and byte order within each class. Such a comparison could
   skip ICU entirely. That covers hashes, hex ids, slugs, and lowercase
   identifiers, which is a large share of what people actually sort.

   This is the kind of shortcut that is silently wrong if the subset is
   drawn one character too wide, so it needs a generated differential
   against PostgreSQL over the whole subset before it is believed —
   the same treatment the collation options themselves got.

2. **An abbreviated key.** PostgreSQL compares an 8-byte prefix of the
   full `strxfrm` output and falls back only on ties. Ours would have to
   compute the full ICU key to take its prefix, so this saves the
   comparison but not the transform — worth measuring, not assuming.

3. **Bound the transform by string length.** `ORDER BY left(s_long,16)`
   costs 468.6 against the full column's 1687.8, so a key built from a
   bounded prefix plus a full-collator tiebreak would cost about a
   quarter. Correctness needs care: a collation prefix does not
   determine the order of the whole string in the presence of
   contractions, so the tiebreak has to be the real comparison and the
   prefix only a filter.

## How it stayed hidden

The main sweep's SPG leg runs under `C` and its PostgreSQL leg under
`en_US.utf8`. For a text sort those are not the same work: one orders
bytes, the other collates. Sixty-four cells have been reporting no
losses on a comparison that was, for the text shapes, in our favour by
construction.

The locale panel added in v7.38.19 was supposed to close that. It could
not: the suite's free-port probe handed its second leg the port the
first one was already serving, so the panel compared one server against
itself and reported `losses=0` — the same defect it was built to catch,
one version earlier, in the other direction. It surfaced only because
the same version also made the panel STATE which collation it expects.

Two instruments, each blind in the way the other was built to see.


## What it turned out to be

Attack 1, with the differential it demanded.

**The claim**: under many collations — not all — `[0-9a-z]` orders by
bytes. Measured against PostgreSQL 18's ICU collations over all 839,160
ordered pairs of two-character strings from that alphabet:

```text
  en  0     cs    198     `ch` is a letter, after `h`
  sv  0     et  7,992     `z` sorts between `s` and `t`
  de  0     lt 20,609     `y` sorts after `i`
  fr  0     da    925     `aa` is `å`, after `z`
  tr  0     hu     18     `cs`, `gy`, `sz` … are letters
```

So: an allowlist keyed on the language, and a test that re-derives the
whole thing in process — every allowed language must agree over the
alphabet at one, two, three and some four characters, and every language
in the second column must NOT. Put `cs` in the allowlist and the second
test goes red; that is how the allowlist was confirmed to be a check
rather than a claim.

The corpus is checked by SORTING it twice rather than comparing every
pair: two total orders agree on every pair exactly when they produce the
same sequence, so one sort answers what 1.2 billion comparisons would,
in 0.13 seconds.

**Three measurements, each refuting the reading before it:**

1. *Skip the keys for a top-N.* 1334.3 → 1320.3. Nothing. Established
   that one ICU comparison costs about what one ICU sort key costs.
2. *Check the alphabet in the comparator.* 1701.9 → 1467.5. The check
   walks the string and a full sort asked it 7.4 million times — 2.8 GB
   of scanning, itself the cost.
3. *Ask once per value; let the KEY carry the answer.* 395.6.

**And an invariant that was not one.** The first version of (3) read the
promise off the key's VARIANT — a plain `Text` key under such a
collation had to be alnum, because the builder gave everything else
ICU's key. That holds only where the builder and the comparator are
handed the same collations, and the join path is not such a place:
`round688` came back `Banana,Zebra,_under,apple,cherry,Ápple` where
PostgreSQL gives `apple,Ápple,Banana,cherry,_under,Zebra`. An invariant
two call sites have to agree on is not an invariant. The promise is
carried in the value now.

## Still open

Attack 2 and attack 3 are untried and no longer urgent. What remains
behind PostgreSQL is the top-N at 2.39x, and a value OUTSIDE the
alphabet — anything with a space, a capital or an accent — still pays
full ICU, which is correct and unimproved.
