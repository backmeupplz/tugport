#!/bin/sh
# Inside the dev box after run-server.sh: exercise setup, agents, projects and a deploy.
set -e
B=http://localhost:8484
J='-H Content-Type:application/json'
TOKEN=$(grep -o 'setup#[A-Za-z0-9]*' /var/log/tugport.log | tail -1 | cut -d'#' -f2)
curl -sf -c /tmp/c $J -d "{\"token\":\"$TOKEN\",\"name\":\"nikita\",\"password\":\"correct horse battery\"}" $B/api/setup >/dev/null || curl -sf -c /tmp/c $J -d '{"name":"nikita","password":"correct horse battery"}' $B/api/auth/login >/dev/null
echo "me: $(curl -sf -b /tmp/c $B/api/me | jq -r .principal.name)"
AGENT=$(curl -sf -b /tmp/c $J -d '{"name":"codex-'$$'","all_projects_role":"deployer"}' $B/api/agents)
KEY=$(printf "%s" "$AGENT" | jq -r .key.token)
echo "agent: $(printf "%s" "$AGENT" | jq -r .user.name) key ${KEY%${KEY#????????????}}..."
curl -s -b /tmp/c $J -d '{"name":"shop"}' $B/api/projects >/dev/null
curl -s -b /tmp/c $J -d '{"name":"web","spec":{"source":{"type":"image","image":"nginx:1.29-alpine"}},"env":{"SECRET_TOKEN":"supersecret123"}}' $B/api/projects/shop/services | jq -c '{id, kind}'
echo "agent cannot read env values: $(curl -s -H "Authorization: Bearer $KEY" $B/api/services/shop.web/env | jq -c .env)"
echo "agent cannot change env: $(curl -s -o /dev/null -w '%{http_code}' -X PUT -H "Authorization: Bearer $KEY" $J -d '{"X":"1"}' $B/api/services/shop.web/env)"
echo "agent deploys and waits:"
curl -s -H "Authorization: Bearer $KEY" $J -d '{"wait":true}' $B/api/services/shop.web/deploy | jq -c '{status: .deploy.status, trigger: .deploy.trigger, actor: .deploy.actor, error: .deploy.error}'
C=$(docker ps --filter label=tugport.service --format '{{.Names}}' | head -1)
echo "container: $C"
echo "docker inspect env: $(docker inspect $C --format '{{json .Config.Env}}')"
echo "inside the app: $(docker exec $C /run/tugport/init exec -- sh -c 'echo SECRET_TOKEN=$SECRET_TOKEN')"
echo "pid 1: $(docker exec $C sh -c 'tr "\0" " " </proc/1/cmdline')"
echo "status: $(curl -s -b /tmp/c $B/api/services/shop.web | jq -c '.status | {state, running, desired}')"
