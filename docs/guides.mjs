// The guides on tugport.com/docs, shared by build.mjs (the pages) and og.mjs (their share cards).
export const GUIDES = [
  { file: "API.md", slug: "api", title: "API reference", blurb: "Every endpoint of the panel, its roles and grants, the service spec, the CLI and the MCP endpoint for agents." },
  { file: "MIGRATION.md", slug: "migration", title: "Moving from Easypanel or Coolify", blurb: "How tugport migrate reads the old panel, what maps to what, and how the cutover and the rollback work." },
  { file: "COMPARE.md", slug: "compare", title: "Tugport compared with Coolify, Dokploy, Easypanel and CapRover", blurb: "License, price, servers, deploys, backups, AI agents and moving over, side by side, with sources, checked on October 27, 2026." },
  { file: "UPTIME-KUMA.md", slug: "uptime-kuma", title: "Deploy Uptime Kuma on a Hetzner VPS with Tugport", blurb: "Run Uptime Kuma 2 as a Docker image on Tugport with a data volume, a domain and HTTPS, with every step and timing tested on October 28, 2026.", tested: "2026-10-28" },
];
