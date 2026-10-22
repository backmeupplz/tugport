CREATE TABLE settings (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

-- Runtime state that is not user configuration (last image digest checked, etc.).
CREATE TABLE state (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE users (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL CHECK (kind IN ('human', 'agent')),
  name TEXT NOT NULL UNIQUE,
  email TEXT,
  password_hash TEXT,
  totp_secret BLOB,
  is_owner INTEGER NOT NULL DEFAULT 0,
  -- Role on every project, including ones created later. NULL = only listed projects.
  all_projects_role TEXT,
  -- JSON array of extra grants: "secrets:read", "destroy".
  grants TEXT NOT NULL DEFAULT '[]',
  description TEXT,
  disabled INTEGER NOT NULL DEFAULT 0,
  created_by TEXT,
  created_at INTEGER NOT NULL
);

CREATE TABLE sessions (
  token_hash TEXT PRIMARY KEY,
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  ip TEXT,
  user_agent TEXT
);

CREATE TABLE api_keys (
  id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  prefix TEXT NOT NULL,
  hash TEXT NOT NULL UNIQUE,
  expires_at INTEGER,
  allowed_ips TEXT,
  last_used_at INTEGER,
  last_used_ip TEXT,
  created_at INTEGER NOT NULL,
  revoked_at INTEGER
);

CREATE TABLE projects (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  description TEXT,
  created_at INTEGER NOT NULL
);

CREATE TABLE project_access (
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  role TEXT NOT NULL,
  PRIMARY KEY (user_id, project_id)
);

-- Variables shared by every service in a project. Values are always encrypted.
CREATE TABLE project_vars (
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  key TEXT NOT NULL,
  value BLOB NOT NULL,
  PRIMARY KEY (project_id, key)
);

CREATE TABLE services (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('app', 'database', 'compose')),
  spec TEXT NOT NULL,
  deploy_token_hash TEXT UNIQUE,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  UNIQUE (project_id, name)
);

CREATE TABLE service_env (
  service_id TEXT NOT NULL REFERENCES services(id) ON DELETE CASCADE,
  key TEXT NOT NULL,
  value BLOB NOT NULL,
  PRIMARY KEY (service_id, key)
);

-- Files mounted into a service (contents may hold secrets, so they are encrypted).
CREATE TABLE service_files (
  service_id TEXT NOT NULL REFERENCES services(id) ON DELETE CASCADE,
  target TEXT NOT NULL,
  content BLOB NOT NULL,
  PRIMARY KEY (service_id, target)
);

CREATE TABLE domains (
  id TEXT PRIMARY KEY,
  service_id TEXT NOT NULL REFERENCES services(id) ON DELETE CASCADE,
  host TEXT NOT NULL,
  path TEXT NOT NULL DEFAULT '/',
  port INTEGER NOT NULL,
  container TEXT,
  https INTEGER NOT NULL DEFAULT 1,
  via_tunnel INTEGER NOT NULL DEFAULT 0,
  options TEXT NOT NULL DEFAULT '{}',
  created_at INTEGER NOT NULL,
  UNIQUE (host, path)
);

CREATE TABLE certificates (
  host TEXT PRIMARY KEY,
  cert_pem TEXT,
  key_pem BLOB,
  not_after INTEGER,
  source TEXT NOT NULL DEFAULT 'acme',
  last_error TEXT,
  next_attempt_at INTEGER,
  updated_at INTEGER NOT NULL
);

CREATE TABLE deploys (
  id TEXT PRIMARY KEY,
  service_id TEXT NOT NULL REFERENCES services(id) ON DELETE CASCADE,
  status TEXT NOT NULL,
  trigger TEXT NOT NULL,
  actor TEXT,
  ref TEXT,
  commit_sha TEXT,
  commit_message TEXT,
  image TEXT,
  error TEXT,
  created_at INTEGER NOT NULL,
  started_at INTEGER,
  finished_at INTEGER
);
CREATE INDEX deploys_service ON deploys (service_id, created_at);

CREATE TABLE git_providers (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  name TEXT NOT NULL,
  base_url TEXT NOT NULL,
  config BLOB NOT NULL,
  created_at INTEGER NOT NULL
);

CREATE TABLE registries (
  id TEXT PRIMARY KEY,
  host TEXT NOT NULL UNIQUE,
  username TEXT NOT NULL,
  password BLOB NOT NULL,
  created_at INTEGER NOT NULL
);

CREATE TABLE notification_channels (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL CHECK (kind IN ('telegram', 'email', 'webhook')),
  name TEXT NOT NULL,
  config BLOB NOT NULL,
  events TEXT NOT NULL DEFAULT '["*"]',
  enabled INTEGER NOT NULL DEFAULT 1,
  created_at INTEGER NOT NULL
);

CREATE TABLE backup_targets (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('s3', 'local')),
  config BLOB NOT NULL,
  created_at INTEGER NOT NULL
);

CREATE TABLE backup_jobs (
  id TEXT PRIMARY KEY,
  service_id TEXT REFERENCES services(id) ON DELETE CASCADE,
  target_id TEXT NOT NULL REFERENCES backup_targets(id) ON DELETE CASCADE,
  kind TEXT NOT NULL CHECK (kind IN ('database', 'volume', 'state')),
  volume TEXT,
  schedule TEXT NOT NULL,
  keep_last INTEGER NOT NULL DEFAULT 7,
  enabled INTEGER NOT NULL DEFAULT 1,
  created_at INTEGER NOT NULL
);

CREATE TABLE backup_runs (
  id TEXT PRIMARY KEY,
  job_id TEXT NOT NULL REFERENCES backup_jobs(id) ON DELETE CASCADE,
  kind TEXT NOT NULL DEFAULT 'backup',
  status TEXT NOT NULL,
  snapshot TEXT,
  size_bytes INTEGER,
  error TEXT,
  started_at INTEGER NOT NULL,
  finished_at INTEGER
);
CREATE INDEX backup_runs_job ON backup_runs (job_id, started_at);

CREATE TABLE audit (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  at INTEGER NOT NULL,
  actor_id TEXT,
  actor_name TEXT,
  actor_kind TEXT,
  key_id TEXT,
  ip TEXT,
  action TEXT NOT NULL,
  project_id TEXT,
  service_id TEXT,
  target TEXT,
  detail TEXT
);
CREATE INDEX audit_at ON audit (at);
CREATE INDEX audit_service ON audit (service_id, at);
CREATE INDEX audit_project ON audit (project_id, at);

CREATE TABLE nodes (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  is_local INTEGER NOT NULL DEFAULT 0,
  address TEXT,
  mesh_ip TEXT,
  wg_public_key TEXT,
  region TEXT,
  labels TEXT NOT NULL DEFAULT '{}',
  status TEXT NOT NULL DEFAULT 'online',
  info TEXT NOT NULL DEFAULT '{}',
  last_seen_at INTEGER,
  created_at INTEGER NOT NULL
);

CREATE TABLE metric_series (
  id INTEGER PRIMARY KEY,
  node_id TEXT NOT NULL,
  scope TEXT NOT NULL,
  name TEXT NOT NULL,
  UNIQUE (node_id, scope, name)
);

CREATE TABLE metrics_raw (
  series INTEGER NOT NULL,
  ts INTEGER NOT NULL,
  v REAL NOT NULL,
  PRIMARY KEY (series, ts)
) WITHOUT ROWID;

CREATE TABLE metrics_5m (
  series INTEGER NOT NULL,
  ts INTEGER NOT NULL,
  v REAL NOT NULL,
  vmax REAL NOT NULL,
  PRIMARY KEY (series, ts)
) WITHOUT ROWID;

CREATE TABLE alerts (
  key TEXT PRIMARY KEY,
  firing INTEGER NOT NULL,
  since INTEGER NOT NULL,
  last_notified INTEGER,
  message TEXT
);
