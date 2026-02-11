-- Migration 003: Add error tracking columns
-- Stores error messages for failed decomposition sessions and work items.

ALTER TABLE decomposition_sessions ADD COLUMN error_message TEXT;

ALTER TABLE work_items ADD COLUMN error_message TEXT;
