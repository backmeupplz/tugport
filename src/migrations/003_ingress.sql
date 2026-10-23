-- Which servers may terminate public traffic. Only ingress servers receive TLS keys, the auth
-- config and the full route table; other servers get just the addresses they need to host
-- their own replicas. The panel ('local') is always ingress, regardless of this column.
ALTER TABLE nodes ADD COLUMN ingress INTEGER NOT NULL DEFAULT 0;
