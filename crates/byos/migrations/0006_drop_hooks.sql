-- Hooks are no longer stored on proposals: the driver appends the order's
-- own hooks itself, so the service no longer needs to persist them.
ALTER TABLE proposals DROP COLUMN IF EXISTS hooks;
