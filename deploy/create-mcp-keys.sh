#!/usr/bin/env bash
# Create (or reuse) the two scoped Meilisearch keys the MCP service runs on.
#
# The MCP server is the one component exposed to the internet. It reads the
# market data the indexer builds and writes only CRM state. A Meilisearch
# key grants every action it lists on every index it lists, so "read all,
# write some" takes two keys:
#
#   crm-mcp-read   search, documents.get     companies, signals, accounts,
#                                            activities, company_requests
#   crm-mcp-write  documents.add, tasks.get  accounts, activities,
#                                            company_requests
#
# Neither can delete documents, change settings or manage keys, so a leaked
# agent token cannot touch the market data or the index configuration.
# (`tasks.get` lets the server wait for its own writes to land, so a read
# straight after a write sees it.)
#
#   MEILI_URL=http://127.0.0.1:7700 MEILI_MASTER_KEY=... deploy/create-mcp-keys.sh
#
# Prints shell assignments and nothing else, so it can be evaluated:
#   eval "$(deploy/create-mcp-keys.sh)"
set -euo pipefail

: "${MEILI_URL:?set MEILI_URL}"
: "${MEILI_MASTER_KEY:?set MEILI_MASTER_KEY}"

AUTH=(-H "Authorization: Bearer ${MEILI_MASTER_KEY}" -H "Content-Type: application/json")

# mint NAME DESCRIPTION ACTIONS_JSON INDEXES_JSON
mint() {
  local name="$1" desc="$2" actions="$3" indexes="$4" existing
  existing=$(curl -fsS "${AUTH[@]}" "${MEILI_URL}/keys?limit=100" \
    | python3 -c "
import json,sys
keys = json.load(sys.stdin).get('results', [])
print(next((k['key'] for k in keys if k.get('name') == '${name}'), ''))
")
  if [ -n "${existing}" ]; then
    echo >&2 "reusing the existing ${name} key"
    echo "${existing}"
    return
  fi
  curl -fsS -X POST "${AUTH[@]}" "${MEILI_URL}/keys" \
    --data-binary "{\"name\":\"${name}\",\"description\":\"${desc}\",\"actions\":${actions},\"indexes\":${indexes},\"expiresAt\":null}" \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["key"])'
}

read_key=$(mint crm-mcp-read "MCP server: read everything" \
  '["search","documents.get"]' \
  '["companies","signals","accounts","activities","company_requests"]')
write_key=$(mint crm-mcp-write "MCP server: write CRM state only" \
  '["documents.add","tasks.get"]' \
  '["accounts","activities","company_requests"]')

echo "MEILI_READ_KEY=${read_key}"
echo "MEILI_WRITE_KEY=${write_key}"
