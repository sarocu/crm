#!/usr/bin/env bash
# Create (or reuse) the two scoped Meilisearch keys the MCP service runs on.
#
# The MCP server is the one component exposed to the internet. It reads the
# market data the indexer builds and writes only CRM state. A Meilisearch
# key grants every action it lists on every index it lists, so "read all,
# write some" takes two keys:
#
#   crm-mcp-read   search, documents.get     companies, signals, accounts,
#                                            activities, company_requests,
#                                            portfolios, bot_state
#   crm-mcp-write  documents.add, tasks.get  accounts, activities,
#                                            company_requests, portfolios
#
# Neither can delete documents, change settings or manage keys, so a leaked
# agent token cannot touch the market data or the index configuration.
# (`tasks.get` lets the server wait for its own writes to land, so a read
# straight after a write sees it. `bot_state` is read for each portfolio's
# last sweep result.)
#
# Meilisearch cannot change a key's actions or indexes after creation, so an
# existing key whose scope differs from the above is deleted and minted
# again — redeploy the MCP service with the new values afterwards.
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
  local name="$1" desc="$2" actions="$3" indexes="$4" found
  found=$(curl -fsS "${AUTH[@]}" "${MEILI_URL}/keys?limit=100" \
    | python3 -c "
import json,sys
want_a = set(json.loads('''${actions}'''))
want_i = set(json.loads('''${indexes}'''))
keys = json.load(sys.stdin).get('results', [])
k = next((k for k in keys if k.get('name') == '${name}'), None)
if k is None:
    print('')
elif set(k['actions']) == want_a and set(k['indexes']) == want_i:
    print('ok ' + k['key'])
else:
    print('stale ' + k['uid'])
")
  case "${found}" in
    ok\ *)
      echo >&2 "reusing the existing ${name} key"
      echo "${found#ok }"
      return ;;
    stale\ *)
      echo >&2 "the ${name} key has an outdated scope; replacing it"
      curl -fsS -X DELETE "${AUTH[@]}" "${MEILI_URL}/keys/${found#stale }" >/dev/null ;;
  esac
  curl -fsS -X POST "${AUTH[@]}" "${MEILI_URL}/keys" \
    --data-binary "{\"name\":\"${name}\",\"description\":\"${desc}\",\"actions\":${actions},\"indexes\":${indexes},\"expiresAt\":null}" \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["key"])'
}

read_key=$(mint crm-mcp-read "MCP server: read everything" \
  '["search","documents.get"]' \
  '["companies","signals","accounts","activities","company_requests","portfolios","bot_state"]')
write_key=$(mint crm-mcp-write "MCP server: write CRM state only" \
  '["documents.add","tasks.get"]' \
  '["accounts","activities","company_requests","portfolios"]')

echo "MEILI_READ_KEY=${read_key}"
echo "MEILI_WRITE_KEY=${write_key}"
