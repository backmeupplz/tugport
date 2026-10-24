-- Databases have no source to deploy from; their webhooks (from old migrations) stop working.
UPDATE services SET deploy_token_hash = NULL WHERE kind = 'database';
DELETE FROM state WHERE key IN (SELECT 'hook:' || id FROM services WHERE kind = 'database');
-- Database health checks that still have the old 5-second schedule move to Health::database's,
-- from their next deploy.
UPDATE services SET spec = json_set(spec, '$.health.interval_s', 30, '$.health.timeout_s', 15)
WHERE kind = 'database' AND json_extract(spec, '$.health.kind') = 'command'
  AND json_extract(spec, '$.health.interval_s') = 5 AND json_extract(spec, '$.health.timeout_s') = 5;
