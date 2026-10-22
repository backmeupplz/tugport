-- Cloud accounts, the servers Tugport creates in them, and pools that grow and shrink.

CREATE TABLE cloud_accounts (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  name TEXT NOT NULL UNIQUE,
  -- Sealed JSON with the API token or access keys.
  config BLOB NOT NULL,
  created_at INTEGER NOT NULL
);

CREATE TABLE server_pools (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  account_id TEXT NOT NULL,
  region TEXT NOT NULL,
  size TEXT NOT NULL,
  labels TEXT NOT NULL DEFAULT '{}',
  min_servers INTEGER NOT NULL DEFAULT 0,
  max_servers INTEGER NOT NULL DEFAULT 3,
  -- Average CPU (percent) over every server that adds one, or lets the pool shrink.
  scale_up_cpu REAL NOT NULL DEFAULT 75,
  scale_down_cpu REAL NOT NULL DEFAULT 25,
  created_at INTEGER NOT NULL
);

-- Every machine Tugport created, from creation until it is deleted, so none is forgotten.
CREATE TABLE cloud_servers (
  id TEXT PRIMARY KEY,
  account_id TEXT NOT NULL,
  cloud_id TEXT,
  pool_id TEXT,
  name TEXT NOT NULL,
  region TEXT NOT NULL,
  size TEXT NOT NULL,
  node_id TEXT,
  status TEXT NOT NULL,
  error TEXT,
  created_at INTEGER NOT NULL
);
