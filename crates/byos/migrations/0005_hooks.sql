-- Order hooks: pre- and post-interactions parsed from fullAppData.metadata.hooks,
-- stored alongside the proposal so /solve can encode them without re-fetching.
ALTER TABLE proposals ADD COLUMN hooks JSONB NOT NULL DEFAULT '{"pre":[],"post":[]}'::jsonb;
