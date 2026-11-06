-- Functions and their current editable draft. The draft is one JSON document written in a single
-- statement, so a save is all-or-nothing. `name` and `description` are copies for listing/search.
CREATE TABLE functions (
    id               TEXT PRIMARY KEY,
    name             TEXT NOT NULL,
    description      TEXT NOT NULL,
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL,
    deleted_at       INTEGER,
    -- Monotonic edit sequence: a write carrying an older sequence is ignored.
    draft_seq        INTEGER NOT NULL,
    draft_format     INTEGER NOT NULL,
    draft            TEXT NOT NULL,
    draft_content_fp TEXT NOT NULL,
    -- The revision the draft was last committed as or restored from.
    base_revision_id TEXT
) STRICT;

CREATE TABLE revisions (
    id          TEXT PRIMARY KEY,
    function_id TEXT NOT NULL REFERENCES functions (id) ON DELETE CASCADE,
    number      INTEGER NOT NULL,
    parent_id   TEXT,
    created_at  INTEGER NOT NULL,
    reason_format INTEGER NOT NULL,
    reason      TEXT NOT NULL,
    format      INTEGER NOT NULL,
    document    TEXT NOT NULL,
    content_fp  TEXT NOT NULL,
    spec_fp     TEXT NOT NULL,
    code_fp     TEXT NOT NULL,
    tests_fp    TEXT NOT NULL,
    UNIQUE (function_id, number)
) STRICT;

CREATE TABLE validation_reports (
    id          TEXT PRIMARY KEY,
    function_id TEXT NOT NULL REFERENCES functions (id) ON DELETE CASCADE,
    created_at  INTEGER NOT NULL,
    spec_fp     TEXT NOT NULL,
    code_fp     TEXT NOT NULL,
    tests_fp    TEXT NOT NULL,
    format      INTEGER NOT NULL,
    report      TEXT NOT NULL
) STRICT;
CREATE INDEX validation_reports_by_function ON validation_reports (function_id, created_at);

CREATE TABLE benchmark_results (
    id          TEXT PRIMARY KEY,
    function_id TEXT NOT NULL REFERENCES functions (id) ON DELETE CASCADE,
    config_id   TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    format      INTEGER NOT NULL,
    result      TEXT NOT NULL
) STRICT;
CREATE INDEX benchmark_results_by_function ON benchmark_results (function_id, created_at);

-- Index of example inputs that reference another function's output, from the draft
-- (source_revision NULL) and from every revision. Maintained on each write; used to find
-- dependents and to refuse permanent deletion of a referenced function.
CREATE TABLE output_references (
    source_function TEXT NOT NULL REFERENCES functions (id) ON DELETE CASCADE,
    source_revision TEXT,
    example_id      TEXT NOT NULL,
    target_function TEXT NOT NULL,
    target_revision TEXT,
    target_example  TEXT NOT NULL
) STRICT;
CREATE INDEX output_references_by_target ON output_references (target_function);
CREATE INDEX output_references_by_source ON output_references (source_function, source_revision);

-- History and results are append-only.
CREATE TRIGGER revisions_are_immutable BEFORE UPDATE ON revisions
BEGIN SELECT RAISE(ABORT, 'revisions are immutable'); END;
CREATE TRIGGER validation_reports_are_immutable BEFORE UPDATE ON validation_reports
BEGIN SELECT RAISE(ABORT, 'validation reports are immutable'); END;
CREATE TRIGGER benchmark_results_are_immutable BEFORE UPDATE ON benchmark_results
BEGIN SELECT RAISE(ABORT, 'benchmark results are immutable'); END;
