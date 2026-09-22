#!/usr/bin/env bash
# Build the four images, push them, and deploy them as heyo microVMs.
#
# heyo's firecracker_containerd backend pulls ordinary OCI references, so the
# Dockerfiles in this repo ARE the VM image build — there is no separate
# image format to produce.
#
# Required:
#   REGISTRY           where to push, e.g. ghcr.io/your-org
#   MEILI_MASTER_KEY   Meilisearch admin key
#   MCP_AUTH_TOKEN     bearer token the MCP endpoint will require
#   CONTACT_EMAIL      a real address, required by SEC EDGAR and Wikimedia
# Optional:
#   TAG (default: git short sha), REGION (US|EU — the heyo cloud region),
#   SIZE_CLASS, CRAWL_SEEDS, NEWS_FEEDS, MCP_TOKENS, PREFIX (default: crm)
#   MARKET_FILE  which market config to bake in (default market.example.toml)
#
# Secrets should come from HeyoSecret rather than your shell history:
#   MEILI_MASTER_KEY=$(heyo-secret get crm/meili-master) deploy/heyo-deploy.sh
set -euo pipefail

: "${REGISTRY:?set REGISTRY, e.g. ghcr.io/your-org}"
: "${MEILI_MASTER_KEY:?set MEILI_MASTER_KEY}"
: "${MCP_AUTH_TOKEN:?set MCP_AUTH_TOKEN}"
: "${CONTACT_EMAIL:?set CONTACT_EMAIL}"

TAG="${TAG:-$(git rev-parse --short HEAD 2>/dev/null || date +%s)}"
MARKET_FILE="${MARKET_FILE:-market.example.toml}"
REGION="${REGION:-US}"
SIZE_CLASS="${SIZE_CLASS:-small}"
PREFIX="${PREFIX:-crm}"
BACKEND=firecracker_containerd
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

command -v heyvm >/dev/null || { echo "heyvm is not installed"; exit 1; }

say() { printf '\n==> %s\n' "$*"; }

# ---------------------------------------------------------------- build

for svc in meilisearch bot mcp dashboard; do
  say "building ${PREFIX}-${svc}:${TAG} (market: ${MARKET_FILE})"
  # meilisearch takes no market config; the build arg is ignored there.
  docker build -f "${ROOT}/${svc}/Dockerfile" \
    --build-arg "MARKET_FILE=${MARKET_FILE}" \
    -t "${REGISTRY}/${PREFIX}-${svc}:${TAG}" "${ROOT}"
  docker push "${REGISTRY}/${PREFIX}-${svc}:${TAG}"
done

# ---------------------------------------------------------- meilisearch
#
# Deployed first, and privately: only the bot and the MCP server should be
# able to reach the database.
#
# Note on durability: firecracker_containerd does not take `--mount`, and
# heyo's named volumes are host directories rather than cloud disks. The
# market data (companies, signals) is reconstructible — the indexer rebuilds
# it from the upstream sources — but the CRM state is NOT: accounts and
# activities exist only here. Take Meilisearch dumps or snapshots off this VM
# on a schedule before relying on it. `--no-ttl` keeps it from being reaped.
say "deploying ${PREFIX}-meilisearch"
heyvm create --cloud \
  --name "${PREFIX}-meilisearch" \
  --backend "${BACKEND}" \
  --region "${REGION}" \
  --size-class "${SIZE_CLASS}" \
  --image "${REGISTRY}/${PREFIX}-meilisearch:${TAG}" \
  --env "MEILI_MASTER_KEY=${MEILI_MASTER_KEY}" \
  --port 7700 \
  --private \
  --health-path /health \
  --health-timeout 180s \
  --no-ttl \
  --format json

MEILI_URL=$(heyvm bind "${PREFIX}-meilisearch" 7700 --private --format json \
  | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d.get("url") or d.get("hostname") or "")')

if [ -z "${MEILI_URL}" ]; then
  echo "could not determine the meilisearch URL; check: heyvm list --format json" >&2
  exit 1
fi
say "meilisearch reachable at ${MEILI_URL}"

# Scoped keys for the one service that faces the internet: read everything,
# write only CRM state.
export MEILI_URL MEILI_MASTER_KEY
eval "$("${ROOT}/deploy/create-mcp-keys.sh")"

# ------------------------------------------------------------------ bot

say "deploying ${PREFIX}-bot"
heyvm create --cloud \
  --name "${PREFIX}-bot" \
  --backend "${BACKEND}" \
  --region "${REGION}" \
  --size-class "${SIZE_CLASS}" \
  --image "${REGISTRY}/${PREFIX}-bot:${TAG}" \
  --env "MEILI_URL=${MEILI_URL}" \
  --env "MEILI_MASTER_KEY=${MEILI_MASTER_KEY}" \
  --env "CONTACT_EMAIL=${CONTACT_EMAIL}" \
  --env "CRAWL_SEEDS=${CRAWL_SEEDS:-}" \
  --env "NEWS_FEEDS=${NEWS_FEEDS:-}" \
  --env "RUST_LOG=${RUST_LOG:-info}" \
  --port 8081 \
  --private \
  --health-path /healthz \
  --no-ttl \
  --format json

# ------------------------------------------------------------------ mcp

say "deploying ${PREFIX}-mcp"
heyvm create --cloud \
  --name "${PREFIX}-mcp" \
  --backend "${BACKEND}" \
  --region "${REGION}" \
  --size-class "${SIZE_CLASS}" \
  --image "${REGISTRY}/${PREFIX}-mcp:${TAG}" \
  --env "MEILI_URL=${MEILI_URL}" \
  --env "MEILI_READ_KEY=${MEILI_READ_KEY}" \
  --env "MEILI_WRITE_KEY=${MEILI_WRITE_KEY}" \
  --env "MCP_AUTH_TOKEN=${MCP_AUTH_TOKEN}" \
  --env "MCP_TOKENS=${MCP_TOKENS:-}" \
  --env "RUST_LOG=${RUST_LOG:-info}" \
  --port 8080 \
  --health-path /healthz \
  --no-ttl \
  --auto-bind \
  --format json

# ------------------------------------------------------------ dashboard
#
# Read-only, with no authentication of its own: bound private so only
# account members reach it. Put the load balancer's auth in front of it for
# anything stronger.
say "deploying ${PREFIX}-dashboard"
heyvm create --cloud \
  --name "${PREFIX}-dashboard" \
  --backend "${BACKEND}" \
  --region "${REGION}" \
  --size-class "${SIZE_CLASS}" \
  --image "${REGISTRY}/${PREFIX}-dashboard:${TAG}" \
  --env "MEILI_URL=${MEILI_URL}" \
  --env "MEILI_MASTER_KEY=${MEILI_MASTER_KEY}" \
  --env "BOT_HEALTH_URL=${BOT_URL:-http://${PREFIX}-bot:8081}/healthz" \
  --env "RUST_LOG=${RUST_LOG:-info}" \
  --port 8090 \
  --health-path /healthz \
  --no-ttl \
  --format json

say "binding the dashboard port"
heyvm bind "${PREFIX}-dashboard" 8090 --private --format json

say "done"
cat <<NOTE

Find every URL with:

  heyvm list --format json

  MCP endpoint      <mcp-url>/mcp        header: Authorization: Bearer \$MCP_AUTH_TOKEN
  Operator dashboard <dashboard-url:8090>  bound private — put your load
                                           balancer's auth in front of it

The dashboard has no authentication of its own and is read-only. Port 8090
is bound --private so only account members reach it.

Accounts and activities live only in Meilisearch. Back them up.

NOTE
