-- depd.setup_users — depended-on table fixture.
--
-- Creates a small `users` table referenced by port.* / orig.*
-- fixtures that need a fixed-shape relation without re-creating it
-- per test. Not run standalone.
--
-- Naming: `depd.` per YugabyteDB convention. Referenced via the
-- runner's `# oracle: depends depd.setup_users` directive (parser
-- lands during v7.38 P1).

CREATE TABLE users (
    id INT NOT NULL PRIMARY KEY,
    name TEXT NOT NULL,
    org_id INT NOT NULL
);

INSERT INTO users (id, name, org_id) VALUES
    (1, 'alice', 10),
    (2, 'bob',   10),
    (3, 'carol', 20),
    (4, 'dave',  20),
    (5, 'eve',   30);
