-- Deploys started with "Redeploy without cache".
ALTER TABLE deploys ADD COLUMN no_cache INTEGER NOT NULL DEFAULT 0;
