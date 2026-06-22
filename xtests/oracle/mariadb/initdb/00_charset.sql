-- Pin utf8mb4_bin to mirror PG's C-locale text comparisons.
ALTER DATABASE testdb CHARACTER SET = utf8mb4 COLLATE = utf8mb4_bin;
