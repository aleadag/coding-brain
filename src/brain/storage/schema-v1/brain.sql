CREATE TABLE schema_meta (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    application_id INTEGER NOT NULL CHECK (application_id = 0x4342524e),
    schema_version INTEGER NOT NULL CHECK (schema_version = 1),
    schema_generation INTEGER NOT NULL CHECK (schema_generation = 1),
    migration_state TEXT NOT NULL CHECK (migration_state IN ('complete', 'in_progress')),
    migration_generation INTEGER NOT NULL CHECK (migration_generation BETWEEN 0 AND 0x7fffffffffffffff),
    erasure_state TEXT NOT NULL CHECK (erasure_state IN ('complete', 'in_progress')),
    erasure_generation INTEGER NOT NULL CHECK (erasure_generation BETWEEN 0 AND 0x7fffffffffffffff),
    activity_high_water INTEGER NOT NULL CHECK (activity_high_water BETWEEN 0 AND 0x7fffffffffffffff),
    maintenance_retention_boundary INTEGER CHECK (
        maintenance_retention_boundary IS NULL
        OR maintenance_retention_boundary BETWEEN 1 AND 0x7fffffffffffffff
    ),
    maintenance_scan_before INTEGER CHECK (
        maintenance_scan_before IS NULL
        OR maintenance_scan_before BETWEEN 1 AND 0x7fffffffffffffff
    ),
    maintenance_recent_remaining INTEGER NOT NULL DEFAULT 32 CHECK (
        maintenance_recent_remaining BETWEEN 0 AND 32
    ),
    maintenance_overlap_activity_id TEXT CHECK (
        maintenance_overlap_activity_id IS NULL
        OR length(maintenance_overlap_activity_id) BETWEEN 1 AND 512
    ),
    maintenance_overlap_keep INTEGER NOT NULL DEFAULT 0 CHECK (
        maintenance_overlap_keep IN (0, 1)
    ),
    CHECK (
        (maintenance_retention_boundary IS NULL
         AND maintenance_scan_before IS NULL
         AND maintenance_overlap_activity_id IS NULL
         AND maintenance_overlap_keep = 0)
        OR
        (maintenance_retention_boundary IS NOT NULL
         AND maintenance_scan_before IS NOT NULL
         AND maintenance_scan_before <= maintenance_retention_boundary)
    ),
    CHECK (maintenance_overlap_activity_id IS NOT NULL OR maintenance_overlap_keep = 0)
) STRICT;

INSERT INTO schema_meta (
    singleton,
    application_id,
    schema_version,
    schema_generation,
    migration_state,
    migration_generation,
    erasure_state,
    erasure_generation,
    activity_high_water,
    maintenance_retention_boundary,
    maintenance_scan_before,
    maintenance_recent_remaining,
    maintenance_overlap_activity_id,
    maintenance_overlap_keep
) VALUES (1, 0x4342524e, 1, 1, 'complete', 0, 'complete', 0, 0, NULL, NULL, 32, NULL, 0);

CREATE TABLE permission_attempts (
    attempt_id TEXT PRIMARY KEY CHECK (length(attempt_id) BETWEEN 1 AND 512),
    request_identity_key TEXT NOT NULL CHECK (
        length(request_identity_key) = 64
        AND request_identity_key NOT GLOB '*[^0-9a-f]*'
    ),
    provider TEXT NOT NULL CHECK (provider IN ('codex', 'claude', 'antigravity')),
    session_id TEXT NOT NULL CHECK (length(session_id) BETWEEN 1 AND 512),
    provider_session_id TEXT CHECK (
        provider_session_id IS NULL
        OR (length(provider_session_id) BETWEEN 1 AND 512 AND provider_session_id != session_id)
    ),
    turn_id TEXT NOT NULL CHECK (length(turn_id) BETWEEN 1 AND 512),
    tool_use_id TEXT CHECK (tool_use_id IS NULL OR length(tool_use_id) BETWEEN 1 AND 512),
    request_key TEXT NOT NULL CHECK (
        length(request_key) = 64 AND request_key NOT GLOB '*[^0-9a-f]*'
    ),
    cwd BLOB NOT NULL CHECK (length(cwd) BETWEEN 1 AND 4096),
    project_id BLOB NOT NULL CHECK (length(project_id) BETWEEN 1 AND 4096),
    tool_name TEXT NOT NULL CHECK (length(tool_name) BETWEEN 1 AND 512),
    activity_id TEXT NOT NULL CHECK (length(activity_id) BETWEEN 1 AND 512),
    authority_action TEXT CHECK (authority_action IS NULL OR authority_action IN ('allow', 'deny')),
    attempt_state TEXT NOT NULL CHECK (attempt_state IN ('evaluating', 'needs_input', 'decided', 'abandoned')),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms BETWEEN 0 AND 0x7fffffffffffffff),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms BETWEEN created_at_ms AND 0x7fffffffffffffff),
    UNIQUE (attempt_id, authority_action),
    CHECK (
        (attempt_state = 'decided' AND authority_action IS NOT NULL)
        OR (attempt_state != 'decided' AND authority_action IS NULL)
    )
) STRICT;

CREATE INDEX permission_attempts_request_active
ON permission_attempts (request_identity_key, updated_at_ms DESC)
WHERE attempt_state IN ('evaluating', 'needs_input', 'decided');

CREATE TABLE decision_identities (
    decision_id TEXT PRIMARY KEY CHECK (length(decision_id) BETWEEN 1 AND 512),
    identity_kind TEXT NOT NULL CHECK (identity_kind IN ('permission', 'observation')),
    permission_attempt_id TEXT,
    provider TEXT NOT NULL CHECK (provider IN ('codex', 'claude', 'antigravity')),
    session_id TEXT CHECK (session_id IS NULL OR length(session_id) BETWEEN 1 AND 512),
    turn_id TEXT CHECK (turn_id IS NULL OR length(turn_id) BETWEEN 1 AND 512),
    tool_use_id TEXT CHECK (tool_use_id IS NULL OR length(tool_use_id) BETWEEN 1 AND 512),
    authority_action TEXT CHECK (authority_action IS NULL OR authority_action IN ('allow', 'deny')),
    decision_source TEXT CHECK (decision_source IS NULL OR decision_source IN ('model', 'deterministic_safety', 'native_provider')),
    decided_at_ms INTEGER NOT NULL CHECK (decided_at_ms BETWEEN 0 AND 0x7fffffffffffffff),
    UNIQUE (decision_id, identity_kind),
    UNIQUE (decision_id, identity_kind, authority_action),
    UNIQUE (decision_id, provider, session_id, turn_id, tool_use_id, authority_action),
    UNIQUE (decision_id, permission_attempt_id, authority_action),
    FOREIGN KEY (permission_attempt_id) REFERENCES permission_attempts (attempt_id),
    FOREIGN KEY (permission_attempt_id, authority_action)
        REFERENCES permission_attempts (attempt_id, authority_action),
    CHECK (
        (identity_kind = 'permission'
         AND session_id IS NOT NULL AND turn_id IS NOT NULL
         AND authority_action IS NOT NULL AND decision_source IS NOT NULL)
        OR
        (identity_kind = 'observation'
         AND permission_attempt_id IS NULL
         AND session_id IS NULL AND turn_id IS NULL AND tool_use_id IS NULL
         AND authority_action IS NULL AND decision_source IS NULL)
    )
) STRICT;

CREATE INDEX decision_identities_authority
ON decision_identities (provider, session_id, turn_id, tool_use_id, authority_action, decided_at_ms DESC);

CREATE TABLE decision_payloads (
    decision_id TEXT PRIMARY KEY,
    payload_kind TEXT NOT NULL CHECK (payload_kind IN ('permission', 'observation')),
    source_cursor INTEGER NOT NULL CHECK (source_cursor BETWEEN 1 AND 0x7fffffffffffffff),
    normalized_command TEXT CHECK (normalized_command IS NULL OR length(normalized_command) <= 4096),
    reasoning TEXT CHECK (reasoning IS NULL OR length(reasoning) <= 4096),
    note TEXT CHECK (note IS NULL OR length(note) <= 4096),
    decision_record BLOB NOT NULL CHECK (length(decision_record) <= 1048576),
    FOREIGN KEY (decision_id, payload_kind)
        REFERENCES decision_identities (decision_id, identity_kind) ON DELETE CASCADE,
    FOREIGN KEY (source_cursor) REFERENCES activity_events (source_cursor) ON DELETE RESTRICT
) STRICT;

CREATE UNIQUE INDEX decision_payloads_source_cursor
ON decision_payloads (source_cursor)
WHERE source_cursor IS NOT NULL;

CREATE TABLE activity_events (
    source_cursor INTEGER PRIMARY KEY CHECK (source_cursor BETWEEN 1 AND 0x7fffffffffffffff),
    activity_id TEXT NOT NULL CHECK (length(activity_id) BETWEEN 1 AND 512),
    event_kind TEXT NOT NULL CHECK (event_kind IN ('decision', 'lifecycle', 'diagnostic')),
    event_state TEXT NOT NULL CHECK (event_state IN ('observed', 'evaluating', 'allowed', 'denied', 'abstained', 'error', 'delivered', 'delivery_failed', 'outcome', 'correction', 'interrupted', 'incomplete')),
    recorded_at_ms INTEGER NOT NULL CHECK (recorded_at_ms BETWEEN 0 AND 0x7fffffffffffffff),
    permission_attempt_id TEXT,
    terminal_provider TEXT CHECK (terminal_provider IS NULL OR terminal_provider IN ('codex', 'claude', 'antigravity')),
    terminal_session_id TEXT CHECK (terminal_session_id IS NULL OR length(terminal_session_id) BETWEEN 1 AND 512),
    terminal_turn_id TEXT CHECK (terminal_turn_id IS NULL OR length(terminal_turn_id) BETWEEN 1 AND 512),
    terminal_tool_use_id TEXT CHECK (terminal_tool_use_id IS NULL OR length(terminal_tool_use_id) BETWEEN 1 AND 512),
    terminal_action TEXT CHECK (terminal_action IS NULL OR terminal_action IN ('allow', 'deny')),
    outcome TEXT CHECK (outcome IS NULL OR outcome IN ('succeeded', 'failed', 'cancelled', 'completed')),
    correction TEXT CHECK (correction IS NULL OR correction IN ('brain_right', 'brain_wrong', 'exception')),
    distilled_at_ms INTEGER CHECK (distilled_at_ms IS NULL OR distilled_at_ms BETWEEN 0 AND 0x7fffffffffffffff),
    event_payload BLOB NOT NULL CHECK (length(event_payload) <= 65536),
    FOREIGN KEY (permission_attempt_id) REFERENCES permission_attempts (attempt_id),
    FOREIGN KEY (permission_attempt_id, terminal_action)
        REFERENCES permission_attempts (attempt_id, authority_action),
    CHECK (
        (terminal_provider IS NULL AND terminal_session_id IS NULL AND terminal_turn_id IS NULL AND terminal_tool_use_id IS NULL AND terminal_action IS NULL)
        OR
        (terminal_provider IS NOT NULL AND terminal_session_id IS NOT NULL AND terminal_turn_id IS NOT NULL AND terminal_action IS NOT NULL)
    ),
    UNIQUE (activity_id, terminal_provider, terminal_session_id, terminal_turn_id, terminal_tool_use_id, terminal_action),
    UNIQUE (activity_id, permission_attempt_id, terminal_action),
    UNIQUE (source_cursor, event_kind, event_state, terminal_action)
) STRICT;

CREATE INDEX activity_events_cursor
ON activity_events (source_cursor DESC);

CREATE INDEX activity_events_activity_id
ON activity_events (activity_id);

CREATE INDEX activity_events_permission_identity
ON activity_events (terminal_provider, terminal_session_id, terminal_turn_id, terminal_tool_use_id, terminal_action, source_cursor DESC)
WHERE terminal_provider IS NOT NULL;

CREATE INDEX activity_events_outcome
ON activity_events (outcome, source_cursor DESC)
WHERE outcome IS NOT NULL;

CREATE INDEX activity_events_correction
ON activity_events (correction, source_cursor DESC)
WHERE correction IS NOT NULL;

CREATE INDEX activity_events_distillation
ON activity_events (distilled_at_ms, source_cursor)
WHERE distilled_at_ms IS NULL;

CREATE TABLE permission_commits (
    attempt_id TEXT PRIMARY KEY,
    transaction_id TEXT NOT NULL UNIQUE CHECK (length(transaction_id) BETWEEN 1 AND 512),
    decision_id TEXT NOT NULL UNIQUE,
    terminal_activity_id TEXT NOT NULL UNIQUE,
    authority_action TEXT NOT NULL CHECK (authority_action IN ('allow', 'deny')),
    evidence_kind TEXT NOT NULL CHECK (evidence_kind IN ('provider_authority', 'deterministic_safety')),
    delivery_state TEXT NOT NULL CHECK (delivery_state IN ('not_required', 'pending', 'delivered', 'failed', 'unknown')),
    response_eligible INTEGER NOT NULL CHECK (response_eligible IN (0, 1)),
    committed_at_ms INTEGER NOT NULL CHECK (committed_at_ms BETWEEN 0 AND 0x7fffffffffffffff),
    FOREIGN KEY (attempt_id, authority_action)
        REFERENCES permission_attempts (attempt_id, authority_action),
    FOREIGN KEY (decision_id, attempt_id, authority_action)
        REFERENCES decision_identities (decision_id, permission_attempt_id, authority_action),
    FOREIGN KEY (terminal_activity_id, attempt_id, authority_action)
        REFERENCES activity_events (activity_id, permission_attempt_id, terminal_action),
    CHECK (
        evidence_kind != 'deterministic_safety'
        OR
        (authority_action = 'deny' AND response_eligible = 0 AND delivery_state = 'not_required')
    ),
    CHECK (
        (response_eligible = 0 AND delivery_state = 'not_required')
        OR
        (response_eligible = 1 AND delivery_state IN ('pending', 'delivered', 'failed', 'unknown'))
    )
) STRICT;

CREATE INDEX permission_commits_request_authority
ON permission_commits (attempt_id, authority_action, committed_at_ms DESC);

CREATE INDEX permission_commits_undelivered_audit
ON permission_commits (delivery_state, committed_at_ms)
WHERE delivery_state IN ('pending', 'unknown');

CREATE TABLE historical_permission_authority (
    decision_id TEXT PRIMARY KEY,
    terminal_source_cursor INTEGER NOT NULL UNIQUE,
    decision_kind TEXT NOT NULL CHECK (decision_kind = 'permission'),
    authority_action TEXT NOT NULL CHECK (authority_action IN ('allow', 'deny')),
    terminal_event_kind TEXT NOT NULL CHECK (terminal_event_kind = 'decision'),
    terminal_event_state TEXT NOT NULL CHECK (terminal_event_state IN ('allowed', 'denied')),
    terminal_action TEXT NOT NULL CHECK (terminal_action IN ('allow', 'deny')),
    provenance_kind TEXT NOT NULL CHECK (
        provenance_kind IN ('proposal_terminal', 'journal_correlated', 'lifecycle_correlated')
    ),
    transaction_id TEXT CHECK (
        transaction_id IS NULL OR length(transaction_id) BETWEEN 1 AND 512
    ),
    request_key TEXT CHECK (
        request_key IS NULL
        OR (length(request_key) = 64 AND request_key NOT GLOB '*[^0-9a-f]*')
    ),
    response_eligible INTEGER NOT NULL CHECK (response_eligible = 0),
    delivery_state TEXT NOT NULL CHECK (delivery_state = 'unknown'),
    FOREIGN KEY (decision_id, decision_kind, authority_action)
        REFERENCES decision_identities (decision_id, identity_kind, authority_action),
    FOREIGN KEY (
        terminal_source_cursor, terminal_event_kind, terminal_event_state, terminal_action
    ) REFERENCES activity_events (source_cursor, event_kind, event_state, terminal_action),
    CHECK (
        (authority_action = 'allow' AND terminal_event_state = 'allowed')
        OR (authority_action = 'deny' AND terminal_event_state = 'denied')
    ),
    CHECK (terminal_action = authority_action),
    CHECK (
        (provenance_kind = 'proposal_terminal'
         AND transaction_id IS NULL AND request_key IS NULL)
        OR
        (provenance_kind IN ('journal_correlated', 'lifecycle_correlated')
         AND transaction_id IS NOT NULL AND request_key IS NOT NULL)
    )
) STRICT;

CREATE INDEX historical_permission_authority_cursor
ON historical_permission_authority (terminal_source_cursor ASC, decision_id ASC);

CREATE TABLE lifecycle_meta (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    next_sequence INTEGER NOT NULL CHECK (next_sequence BETWEEN 1 AND 0x7fffffffffffffff)
) STRICT;

INSERT INTO lifecycle_meta (singleton, next_sequence) VALUES (1, 1);

CREATE TABLE lifecycle_sessions (
    provider TEXT NOT NULL CHECK (provider IN ('codex', 'claude', 'antigravity')),
    session_id TEXT NOT NULL CHECK (length(CAST(session_id AS BLOB)) BETWEEN 1 AND 512),
    cwd BLOB NOT NULL CHECK (length(cwd) BETWEEN 1 AND 4096),
    transcript_path BLOB CHECK (transcript_path IS NULL OR length(transcript_path) BETWEEN 1 AND 4096),
    provider_session_id TEXT CHECK (
        provider_session_id IS NULL
        OR (
            length(CAST(provider_session_id AS BLOB)) BETWEEN 1 AND 512
            AND provider_session_id != session_id
        )
    ),
    latest_event TEXT NOT NULL CHECK (
        latest_event IN (
            'session_start', 'user_prompt_submit', 'pre_tool_use', 'post_tool_use',
            'permission_request', 'subagent_start', 'subagent_stop', 'stop'
        )
    ),
    latest_sequence INTEGER NOT NULL CHECK (latest_sequence BETWEEN 1 AND 0x7fffffffffffffff),
    latest_received_at_ms INTEGER NOT NULL CHECK (latest_received_at_ms BETWEEN 0 AND 0x7fffffffffffffff),
    session_start_source TEXT CHECK (
        session_start_source IS NULL
        OR session_start_source IN ('startup', 'resume', 'clear', 'compact')
    ),
    ignored_reason TEXT CHECK (
        ignored_reason IS NULL
        OR ignored_reason IN (
            'duplicate', 'recent_turn', 'ambiguous_turn', 'active_subagent_capacity',
            'sequence_exhausted', 'unproven_subagent', 'provider_session_mismatch',
            'subagent_turn_mismatch'
        )
    ),
    signature_event TEXT CHECK (
        signature_event IS NULL
        OR signature_event IN (
            'session_start', 'user_prompt_submit', 'pre_tool_use', 'post_tool_use',
            'subagent_start', 'subagent_stop', 'stop'
        )
    ),
    signature_turn_id TEXT CHECK (
        signature_turn_id IS NULL
        OR length(CAST(signature_turn_id AS BLOB)) BETWEEN 1 AND 512
    ),
    signature_detail_id TEXT CHECK (
        signature_detail_id IS NULL
        OR length(CAST(signature_detail_id AS BLOB)) BETWEEN 1 AND 512
    ),
    signature_session_start_source TEXT CHECK (
        signature_session_start_source IS NULL
        OR signature_session_start_source IN ('startup', 'resume', 'clear', 'compact')
    ),
    PRIMARY KEY (provider, session_id),
    FOREIGN KEY (provider, provider_session_id)
        REFERENCES lifecycle_sessions (provider, session_id) ON DELETE CASCADE,
    CHECK (
        (latest_event = 'session_start' AND session_start_source IS NOT NULL)
        OR
        (latest_event != 'session_start' AND session_start_source IS NULL)
    ),
    CHECK (
        (
            signature_event IS NULL
            AND signature_turn_id IS NULL
            AND signature_detail_id IS NULL
            AND signature_session_start_source IS NULL
        )
        OR
        (
            signature_event = 'session_start'
            AND signature_turn_id IS NULL
            AND signature_detail_id IS NULL
            AND signature_session_start_source IS NOT NULL
        )
        OR
        (
            signature_event IN ('subagent_start', 'subagent_stop')
            AND signature_turn_id IS NOT NULL
            AND signature_detail_id IS NOT NULL
            AND signature_session_start_source IS NULL
        )
        OR
        (
            signature_event IN ('user_prompt_submit', 'pre_tool_use', 'post_tool_use', 'stop')
            AND signature_turn_id IS NOT NULL
            AND signature_detail_id IS NULL
            AND signature_session_start_source IS NULL
        )
    ),
    CHECK (
        (
            latest_event = 'permission_request'
            AND signature_event IS NULL
            AND signature_turn_id IS NULL
            AND signature_detail_id IS NULL
            AND signature_session_start_source IS NULL
        )
        OR
        (
            latest_event != 'permission_request'
            AND signature_event IS NOT NULL
            AND signature_event = latest_event
            AND (
                latest_event != 'session_start'
                OR signature_session_start_source = session_start_source
            )
        )
    )
) STRICT;

CREATE INDEX lifecycle_sessions_provider_parent
ON lifecycle_sessions (provider, provider_session_id, session_id)
WHERE provider_session_id IS NOT NULL;

CREATE INDEX lifecycle_sessions_latest
ON lifecycle_sessions (provider, latest_sequence DESC, session_id);

CREATE TABLE lifecycle_leases (
    provider TEXT NOT NULL,
    session_id TEXT NOT NULL,
    status_event TEXT NOT NULL CHECK (
        status_event IN (
            'user_prompt_submit', 'pre_tool_use', 'post_tool_use',
            'permission_request', 'subagent_start', 'stop'
        )
    ),
    status_sequence INTEGER NOT NULL CHECK (status_sequence BETWEEN 1 AND 0x7fffffffffffffff),
    status_received_at_ms INTEGER NOT NULL CHECK (status_received_at_ms BETWEEN 0 AND 0x7fffffffffffffff),
    projected_status TEXT NOT NULL CHECK (projected_status IN ('processing', 'needs_input', 'idle')),
    PRIMARY KEY (provider, session_id),
    FOREIGN KEY (provider, session_id)
        REFERENCES lifecycle_sessions (provider, session_id) ON DELETE CASCADE,
    CHECK (
        (status_event = 'stop' AND projected_status = 'idle')
        OR
        (
            status_event = 'permission_request'
            AND projected_status IN ('processing', 'needs_input')
        )
        OR
        (
            status_event IN (
                'user_prompt_submit', 'pre_tool_use', 'post_tool_use', 'subagent_start'
            )
            AND projected_status = 'processing'
        )
    )
) STRICT;

CREATE INDEX lifecycle_leases_status
ON lifecycle_leases (projected_status, status_received_at_ms DESC, provider, session_id);

CREATE TABLE lifecycle_turns (
    provider TEXT NOT NULL,
    session_id TEXT NOT NULL,
    continuity_state TEXT NOT NULL CHECK (continuity_state IN ('current', 'recent')),
    turn_id TEXT NOT NULL CHECK (length(CAST(turn_id AS BLOB)) BETWEEN 1 AND 512),
    turn_open INTEGER NOT NULL CHECK (turn_open IN (0, 1)),
    recent_position INTEGER CHECK (recent_position IS NULL OR recent_position BETWEEN 0 AND 31),
    PRIMARY KEY (provider, session_id, continuity_state, turn_id),
    FOREIGN KEY (provider, session_id)
        REFERENCES lifecycle_sessions (provider, session_id) ON DELETE CASCADE,
    CHECK (
        (continuity_state = 'current' AND recent_position IS NULL)
        OR
        (continuity_state = 'recent' AND turn_open = 0 AND recent_position IS NOT NULL)
    )
) STRICT;

CREATE UNIQUE INDEX lifecycle_turns_current
ON lifecycle_turns (provider, session_id)
WHERE continuity_state = 'current';

CREATE UNIQUE INDEX lifecycle_turns_recent_position
ON lifecycle_turns (provider, session_id, recent_position)
WHERE continuity_state = 'recent';

CREATE INDEX lifecycle_turns_exact
ON lifecycle_turns (provider, session_id, turn_id, continuity_state);

CREATE TABLE lifecycle_subagents (
    provider TEXT NOT NULL,
    parent_session_id TEXT NOT NULL,
    agent_id TEXT NOT NULL CHECK (
        length(CAST(agent_id AS BLOB)) BETWEEN 1 AND 512
        AND (provider != 'codex' OR agent_id != parent_session_id)
    ),
    turn_id TEXT NOT NULL CHECK (length(CAST(turn_id AS BLOB)) BETWEEN 1 AND 512),
    subagent_state TEXT NOT NULL CHECK (subagent_state IN ('active', 'stopped')),
    topology_slot INTEGER NOT NULL CHECK (topology_slot BETWEEN 0 AND 63),
    state_sequence INTEGER NOT NULL CHECK (state_sequence BETWEEN 1 AND 0x7fffffffffffffff),
    received_at_ms INTEGER NOT NULL CHECK (received_at_ms BETWEEN 0 AND 0x7fffffffffffffff),
    PRIMARY KEY (provider, parent_session_id, agent_id),
    UNIQUE (provider, agent_id),
    UNIQUE (provider, parent_session_id, subagent_state, topology_slot),
    FOREIGN KEY (provider, parent_session_id)
        REFERENCES lifecycle_sessions (provider, session_id) ON DELETE CASCADE,
    CHECK (subagent_state != 'stopped' OR provider = 'codex')
) STRICT;

CREATE INDEX lifecycle_subagents_parent_state
ON lifecycle_subagents (provider, parent_session_id, subagent_state, state_sequence DESC);

CREATE INDEX lifecycle_subagents_child
ON lifecycle_subagents (provider, agent_id, subagent_state);

CREATE TABLE lifecycle_invocations (
    provider TEXT NOT NULL CHECK (provider = 'antigravity'),
    session_id TEXT NOT NULL,
    invocation_id TEXT NOT NULL CHECK (length(CAST(invocation_id AS BLOB)) BETWEEN 1 AND 512),
    invocation_state TEXT NOT NULL CHECK (invocation_state IN ('active', 'stopped')),
    initial_step INTEGER CHECK (initial_step IS NULL OR initial_step BETWEEN 0 AND 0x7fffffffffffffff),
    state_sequence INTEGER NOT NULL CHECK (state_sequence BETWEEN 1 AND 0x7fffffffffffffff),
    received_at_ms INTEGER NOT NULL CHECK (received_at_ms BETWEEN 0 AND 0x7fffffffffffffff),
    PRIMARY KEY (provider, session_id, invocation_id),
    FOREIGN KEY (provider, session_id)
        REFERENCES lifecycle_sessions (provider, session_id) ON DELETE CASCADE
) STRICT;

CREATE UNIQUE INDEX lifecycle_invocations_active
ON lifecycle_invocations (provider, session_id)
WHERE invocation_state = 'active';

CREATE INDEX lifecycle_invocations_state
ON lifecycle_invocations (provider, invocation_state, state_sequence DESC, session_id);

CREATE TABLE lifecycle_invocation_steps (
    provider TEXT NOT NULL CHECK (provider = 'antigravity'),
    session_id TEXT NOT NULL,
    invocation_id TEXT NOT NULL,
    step INTEGER NOT NULL CHECK (step BETWEEN 0 AND 0x7fffffffffffffff),
    step_slot INTEGER NOT NULL CHECK (step_slot BETWEEN 0 AND 255),
    pre_tool_seen INTEGER NOT NULL CHECK (pre_tool_seen IN (0, 1)),
    post_tool_seen INTEGER NOT NULL CHECK (post_tool_seen IN (0, 1)),
    PRIMARY KEY (provider, session_id, invocation_id, step),
    UNIQUE (provider, session_id, invocation_id, step_slot),
    FOREIGN KEY (provider, session_id, invocation_id)
        REFERENCES lifecycle_invocations (provider, session_id, invocation_id) ON DELETE CASCADE,
    CHECK (pre_tool_seen = 1 OR post_tool_seen = 1)
) STRICT;

CREATE INDEX lifecycle_invocation_steps_exact
ON lifecycle_invocation_steps (provider, session_id, invocation_id, step);

CREATE TABLE recovery_reservations (
    session_key TEXT PRIMARY KEY CHECK (length(CAST(session_key AS BLOB)) BETWEEN 1 AND 4096),
    attempt_key TEXT NOT NULL UNIQUE CHECK (length(CAST(attempt_key AS BLOB)) BETWEEN 1 AND 4096),
    provider TEXT NOT NULL CHECK (provider IN ('codex', 'claude', 'antigravity')),
    session_id TEXT NOT NULL CHECK (length(CAST(session_id AS BLOB)) BETWEEN 1 AND 512),
    ephemeral INTEGER NOT NULL CHECK (ephemeral IN (0, 1)),
    epoch_order INTEGER NOT NULL CHECK (epoch_order BETWEEN 1 AND 0x7fffffffffffffff),
    reserved_at_ms INTEGER NOT NULL CHECK (reserved_at_ms BETWEEN 0 AND 0x7fffffffffffffff)
) STRICT;

CREATE INDEX recovery_reservations_retention
ON recovery_reservations (ephemeral, reserved_at_ms DESC);

CREATE TABLE recovery_once (
    activity_id TEXT PRIMARY KEY CHECK (length(CAST(activity_id AS BLOB)) BETWEEN 1 AND 512),
    source_cursor INTEGER NOT NULL UNIQUE,
    FOREIGN KEY (source_cursor) REFERENCES activity_events (source_cursor) ON DELETE CASCADE
) STRICT;
