# crm

A prospecting CRM whose main interface is MCP, built for a BDR agent to
use. The indexer collects companies in the industries you sell into, along
with the signals that say they may be ready to buy: open roles, funding,
launches, leadership changes, filings and press. The agent asks it who to
work next and why. It then records what it did, so the next conversation
starts where this one left off.

It tracks **companies only**, never individual people.

| | what it is | port |
|---|---|---|
| **`mcp/`** | a remote MCP server: prospecting, dossier and pipeline tools | 8080 |
| **`bot/`** | a continuous indexer over public sources | 8081 |
| **`dashboard/`** | a read-only operator view of the pipeline and indexer | 8090 |
| **`meilisearch/`** | the database, pinned and pre-configured | 7700 |
| `core/` | the shared schema every binary depends on (not a service) | — |

Everything market-specific lives in one TOML file: the **industry verticals**
you sell into and the **customer profiles** you score against.
`market.example.toml` is a worked example. See
[Configuring the market](#configuring-the-market).

## Quick start

```bash
make env                 # writes .env, generating the two secrets for you
$EDITOR .env             # CONTACT_EMAIL must be a real address you monitor
make up                  # build and start the whole stack
```

Then:

```bash
make urls                # where everything is listening
make health              # probe every health endpoint
make logs-bot            # follow the indexer
open http://localhost:8090
```

Point any MCP client at `http://localhost:8080/mcp` with the header
`Authorization: Bearer $MCP_AUTH_TOKEN`. For Claude Code:

```bash
claude mcp add --transport http crm http://localhost:8080/mcp \
  --header "Authorization: Bearer $MCP_AUTH_TOKEN"
```

## The tools

**Read**

| tool | what it does |
|---|---|
| `describe_market` | Verticals and profiles, index counts and freshness, pipeline by status, recent signal volume. Call it first. |
| `list_verticals` | Each vertical's SIC/NAICS codes, keywords and company count. |
| `list_profiles` | Each profile's verticals, territory, size range, hiring roles, keywords and scoring weights. |
| `find_prospects` | The starting point. Ranks companies by fit to a profile (0–100) and gives the reasons for each score. Leaves out accounts already being worked. |
| `get_company` | The full dossier: firmographics, tech seen on their site, job board, account state, fit to every profile, latest signals and the activity log. |
| `search_signals` | Buying signals, newest first, filtered by kind, date, vertical, company and hiring role. |
| `search_companies` | Filter by text, vertical, profile, territory, headcount, tech, recent signals and pipeline status. |

**Write**

| tool | what it does |
|---|---|
| `log_activity` | Records an email, call, LinkedIn message, meeting or note. Moves the account forward when the touch implies it: outbound → `contacted`, an inbound reply → `engaged`, a meeting → `meeting`. |
| `update_account` | Sets status, owner, next step and date, tags and fit profile. Disqualifying requires a reason. Every status change is written to the activity log. |
| `add_company` | Queues a company by domain. The indexer stubs it within a minute or two and crawls its site on the next pass. |

Wherever a tool takes a company, it accepts an id, a domain or an exact name.
Vertical and profile arguments accept a slug, a name or an alias, with typo
tolerance. A name that does not resolve returns suggestions; the constraint
is never silently dropped.

**Pipeline statuses:** `new → researching → queued → contacted → engaged →
meeting → qualified`, plus `disqualified` (requires a reason) and `nurture`
(skipped by prospecting until its `next_touch_at`).

### How a fit score works

`find_prospects` applies a profile's **hard filters** in Meilisearch:

- The company is in one of the profile's verticals.
- Its HQ is in the territory, or unknown.
- Its headcount is in range, or unknown.

It then scores the 200 most recently active candidates. Each criterion earns
up to its weight:

| criterion | how it scores |
|---|---|
| vertical | Full weight when the company is in one of the profile's verticals. |
| size | Full weight when headcount is in range, half when unknown. |
| geo | Full weight when the HQ is in the territory, half when unknown. |
| hiring | Scales with open roles matching `hiring_roles` within `recency_days`, reaching full weight at three. |
| news | Scales with recent non-hiring signals, reaching full weight at two. |
| keywords | Scales with the profile's keywords found in site text, tech and job titles, reaching full weight at two. |

The score is points earned over points available. Every result lists the
reasons behind it, so the agent can explain its picks and use them in
outreach.

## Configuring the market

```bash
cp market.example.toml market.acme.toml && $EDITOR market.acme.toml
make up MARKET_FILE=market.acme.toml
```

```toml
name = "Acme outbound"
slug = "acme"                          # becomes the MCP server name, crm-acme

[[verticals]]
slug = "craft-beverage"                # what `verticals` arguments take
name = "Craft beverage"
aliases = ["brewery", "winery"]        # resolve to this vertical too
sic = ["2082", "2084"]                 # EDGAR: exact match
naics = ["3121"]                       # prefix match: 3121 covers 312120
wikidata = ["Q131734"]                 # Wikidata industry (P452) or class (P31)
keywords = ["taproom", "canning line"] # one hit in name/description, two in site text
feeds = ["https://trade-press.example/feed"]

[[profiles]]
slug = "mid-market-ops"
name = "Mid-market ops-heavy"
verticals = ["craft-beverage"]         # hard filter
countries = ["US"]                     # hard filter; unknown HQ passes
states = ["CO", "UT"]                  # hard filter; unknown HQ passes
employees = { min = 50, max = 1000 }   # hard filter; unknown size passes
keywords = ["netsuite", "inventory"]   # scored
signals = { hiring_roles = ["operations", "supply chain"], recency_days = 90 }
weights = { vertical = 3, size = 2, geo = 1, hiring = 3, news = 1, keywords = 2 }
```

Validation at startup rejects:

- duplicate slugs
- a profile naming a vertical that does not exist
- an inverted headcount range
- a feed that is not an http(s) URL

As with the region file before it, the market file is baked into the images
at build time, and `MARKET_CONFIG` can point elsewhere at runtime.

Three things worth knowing:

- **Wikidata QIDs are not guessed.** Only put QIDs in `wikidata` that you
  have checked on wikidata.org. A vertical without any is simply skipped by
  the `wikidata` source.
- **Hiring roles match role families as well as titles.** Every posting is
  tagged with families such as `sales`, `operations`, `supply chain`,
  `production`, `finance` and `engineering` (the full list is in
  `bot/src/classify.rs`). A profile's `hiring_roles` can name a family or any
  phrase that appears in job titles.
- **`[vocabulary]` replaces the defaults; it does not add to them.**

## What gets indexed

Two indexes of market data, written only by the indexer:

- **`companies`** — one record per company, merged across every source that
  knows about it.
- **`signals`** — dated events, each tied to one company.

Two indexes of CRM state, written only through MCP:

- **`accounts`** — pipeline state, keyed by company id.
- **`activities`** — an append-only outreach log.

A sweep can never overwrite something the agent wrote.

**Company identity is the registrable domain** (`shop.acme.co.uk` becomes
`acme.co.uk`), so every source converges on one record. EDGAR sometimes has
no website for a company. In that case the record is keyed by CIK until
Wikidata supplies the domain for that CIK. The pipeline then folds the two
records together, and signals, accounts and activities move to the
surviving record.

| source | what it does | default interval |
|---|---|---|
| `requests` | Turns `add_company` requests into stubs and queues each homepage for crawling. | 1 min |
| `edgar` | Walks the SEC ticker list and reads each company's submissions record. Keeps companies whose SIC code is in a vertical. Recent 8-Ks, 10-Ks, S-1s and Form Ds become signals; 8-K item 5.02 is a leadership change, 2.01 an acquisition. | 30 min |
| `wikidata` | Runs one SPARQL query per vertical QID for companies with an official website. Adds headcount, HQ, founding year and CIK. | 20 min |
| `crawl` | Crawls known companies' homepages and follows only about, careers, news and product links. Reads schema.org `Organization` data, finds the company's Greenhouse, Lever or Ashby job board, notes tech keywords, and turns dated press releases into signals. | 10 min |
| `jobs` | Polls the public job-board API of every company with a known board. Each open posting becomes a `hiring` signal tagged with role families. | 15 min |
| `news` | Reads RSS/Atom feeds and attributes each item to a company. An item counts when it links to the company's domain, or when the company's distinctive name appears in it as whole words. Items that match nothing are dropped. | 30 min |

Everything goes through one pipeline:

1. Merge each company with its stored record.
2. Re-classify it into verticals.
3. Hash the result, and write only what changed.

Write volume therefore tracks real change rather than crawl volume.

To add a source, implement `Source` in `bot/src/sources/` and register it in
`sources::all()`.

## The dashboard

The dashboard is read-only and has **no authentication of its own**. The load
balancer in front of it owns access. It shows:

- index counts and freshness
- the pipeline by status, with each account's timeline of activity and
  signals
- companies by vertical
- the add-company request queue
- per-source indexer health (runs, written, unchanged, merged, dropped, last
  error)

## Configuration

| variable | services | notes |
|---|---|---|
| `PORT`, `BIND_HOST` | mcp, bot | defaults 8080 / 8081 |
| `MARKET_CONFIG` | all | default `/etc/crm/market.toml` |
| `MEILI_URL` | all | e.g. `http://meilisearch:7700` |
| `MEILI_MASTER_KEY` | bot, dashboard | admin |
| `MEILI_READ_KEY`, `MEILI_WRITE_KEY` | mcp | scoped; see [Keys](#keys) |
| `MCP_AUTH_TOKEN` | mcp | one token, recorded as actor `agent` |
| `MCP_TOKENS` | mcp | `name:secret,...`; the name is recorded on everything that token writes |
| `CONTACT_EMAIL` | bot | **required.** Goes in the User-Agent |
| `CRAWL_SEEDS` | bot | extra homepages to crawl |
| `NEWS_FEEDS` | bot | extra feeds on top of the per-vertical ones |
| `SOURCE_INTERVALS` | bot | `edgar=1h,crawl=10m,...` |
| `DISABLED_SOURCES` | bot | `edgar,wikidata` |
| `CRAWL_MAX_DEPTH`, `CRAWL_PAGES_PER_RUN`, `CRAWL_COMPANIES_PER_RUN` | bot | defaults 2 / 40 / 100 |
| `EDGAR_PER_RUN`, `EDGAR_FILING_DAYS` | bot | defaults 200 / 180 |
| `WIKIDATA_PAGE_SIZE`, `JOBS_PER_RUN` | bot | defaults 200 / 25 |
| `MARKET_FILE` | build arg | which market config to bake in |
| `ADMIN_PORT`, `BOT_HEALTH_URL` | dashboard | default 8090 |
| `RUST_LOG` | all | default `info` |

A few rules the services enforce:

- **`CONTACT_EMAIL` is required.** The SEC and Wikimedia both require a real
  contact in the User-Agent, so the indexer will not start without one.
- **The MCP server fails closed.** It will not start without a token. Opt
  out deliberately with `MCP_ALLOW_ANONYMOUS=1`.
- **Every write records who made it.** Give each agent or person its own
  entry in `MCP_TOKENS`, and `updated_by` and `actor` will say who did what.

### Keys

A Meilisearch key grants every action it lists on every index it lists. The
MCP server needs to read everything but write only CRM state, so it runs on
two keys. `make mcp-keys` (`deploy/create-mcp-keys.sh`) mints both:

| key | actions | indexes |
|---|---|---|
| `crm-mcp-read` | `search`, `documents.get` | all five |
| `crm-mcp-write` | `documents.add`, `tasks.get` | `accounts`, `activities`, `company_requests` |

Neither key can delete documents, change settings or manage keys. A leaked
agent token cannot touch the market data. In local development both keys fall
back to the master key.

## Deploying to heyo

```bash
REGISTRY=ghcr.io/your-org MARKET_FILE=market.acme.toml \
MEILI_MASTER_KEY=... MCP_AUTH_TOKEN=... CONTACT_EMAIL=you@example.com \
make deploy
```

This builds and pushes all four images, then deploys them in order:

1. meilisearch (private)
2. bot (private)
3. mcp (public)
4. dashboard (private)

It mints the MCP server's scoped keys along the way.

**Durability matters more than it did.** `firecracker_containerd` takes no
`--mount`, so the database VM has no attached storage. The market data can be
rebuilt, because the indexer re-derives it. **Accounts and activities cannot:
they exist only in Meilisearch.** Schedule dumps or snapshots off that VM
before the agent does real work in it.

## Development

```bash
make check          # fmt + clippy + tests — run before committing
make bot-once       # one cycle of every source, then exit
make meili          # just Meilisearch, in the background
make run-mcp        # or run-bot, run-bot-once, run-dashboard
```

`make bot-once` is the fastest way to see whether a source change actually
produces records. Watch the `source cycle complete` log line, which reports
`received / written / unchanged / merged / orphaned / invalid`. On a second
run over unchanged upstream data, `written` should be 0.
