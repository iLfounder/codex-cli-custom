ALTER TABLE thread_goals
ADD COLUMN revision INTEGER NOT NULL DEFAULT 1 CHECK(revision >= 1);

CREATE TABLE thread_goal_revision_tombstones (
    thread_id TEXT PRIMARY KEY NOT NULL,
    revision INTEGER NOT NULL CHECK(revision >= 1)
);
