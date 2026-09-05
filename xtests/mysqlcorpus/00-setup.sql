-- MySQL-dialect corpus, file 00 — the objects the rest of the files use.
-- Written in MySQL 9.7's own spelling, not in the subset SPG happens to
-- accept: a corpus that can only say what SPG can say cannot find what
-- SPG is missing.
DROP TABLE IF EXISTS mc_t;
CREATE TABLE mc_t (
  id INT UNSIGNED NOT NULL AUTO_INCREMENT,
  name VARCHAR(32) NOT NULL DEFAULT '',
  qty  TINYINT NOT NULL DEFAULT 0,
  amt  DECIMAL(10,2) DEFAULT NULL,
  note TEXT,
  made DATETIME DEFAULT NULL,
  PRIMARY KEY (id),
  KEY k_name (name)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
INSERT INTO mc_t (name, qty, amt, note, made) VALUES
  ('alpha', 1, 10.50, 'first',  '2020-01-02 03:04:05'),
  ('beta',  2, NULL,  NULL,     '2021-06-07 08:09:10'),
  ('Gamma', 3, -0.25, 'third',  '2022-11-12 13:14:15'),
  ('',      0, 0.00,  '',       NULL);
SELECT 'S01', COUNT(*), SUM(qty) FROM mc_t;
