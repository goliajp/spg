-- Pin strict SQL mode globally. Image default already includes
-- STRICT_TRANS_TABLES; we go full TRADITIONAL-equivalent to surface
-- silent truncation / coercion bugs that PG18 would error on.
SET GLOBAL sql_mode = 'STRICT_ALL_TABLES,NO_ZERO_IN_DATE,NO_ZERO_DATE,ERROR_FOR_DIVISION_BY_ZERO,NO_ENGINE_SUBSTITUTION';
