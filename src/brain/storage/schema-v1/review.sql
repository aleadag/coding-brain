CREATE TABLE review_meta (
    surface TEXT PRIMARY KEY CHECK (surface IN ('attention', 'review', 'diagnostics', 'recent')),
    revision INTEGER NOT NULL CHECK (revision BETWEEN 0 AND 0x7fffffffffffffff),
    source_high_water INTEGER NOT NULL CHECK (source_high_water BETWEEN 0 AND 0x7fffffffffffffff),
    last_archive_revision INTEGER CHECK (
        last_archive_revision IS NULL
        OR last_archive_revision BETWEEN 1 AND revision
    ),
    CHECK (surface <> 'recent' OR last_archive_revision IS NULL)
) STRICT;

INSERT INTO review_meta (surface, revision, source_high_water, last_archive_revision) VALUES
    ('attention', 0, 0, NULL),
    ('review', 0, 0, NULL),
    ('diagnostics', 0, 0, NULL),
    ('recent', 0, 0, NULL);

CREATE TABLE review_marks (
    surface TEXT NOT NULL,
    group_id TEXT NOT NULL CHECK (length(group_id) BETWEEN 1 AND 512),
    source_cursor INTEGER NOT NULL CHECK (source_cursor BETWEEN 1 AND 0x7fffffffffffffff),
    disposition TEXT NOT NULL CHECK (disposition IN ('reviewed', 'archived')),
    revision INTEGER NOT NULL CHECK (revision BETWEEN 1 AND 0x7fffffffffffffff),
    PRIMARY KEY (surface, group_id, source_cursor),
    FOREIGN KEY (surface) REFERENCES review_meta (surface) ON DELETE CASCADE
) STRICT;

CREATE INDEX review_marks_surface_cursor
ON review_marks (surface, source_cursor DESC, group_id);
