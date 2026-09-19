# FIPS Service Discovery

> **Status: proposal.** Nothing in this document is implemented. It
> explores how service discovery could work in FIPS, names the existing
> machinery it would reuse, and lists the questions that are still open.
> Message type numbers, the event kind, the FSP port and all
> configuration keys are tentative.

FIPS can already turn a *known* identity into a route. The `.fips` DNS
responder maps an npub or a host alias to an address, and
`LookupRequest`/`LookupResponse` find the tree coordinates of a known
`NodeAddr`. What it cannot do is answer the question "who offers a
Nostr relay?". Hosting a service today ends with the operator telling
people an npub and a port out of band (see
[../tutorials/host-a-service.md](../tutorials/host-a-service.md)).

Service discovery closes that gap. A node announces the services it
hosts — a web server, a Blossom server, a Nostr relay — to everyone or
to a group, and other nodes find them by *service type* without
knowing an address or a port. Everything runs inside the mesh. No
public Nostr relay, no DNS root, no bootstrap server on the legacy
internet is involved at any step.

## Role

The proposal adds three capabilities:

- **Announcing.** A node publishes a *service record* for each service
  it wants found: type, port, protocol and a little metadata. The
  record is a Nostr event under the node's FIPS identity key. Each
  announcement has a scope: public, a list of npubs, a shared-secret
  group, or a combination.
- **Finding.** A node asks the mesh for providers of a service type
  and gets back the nearest ones first. The answer carries everything
  needed to connect: npub, FIPS address, port, protocol.
- **Browsing (optional).** Nostr relays that live *inside* the mesh
  can act as directories that hold many records and answer richer
  queries. They are themselves found through the second capability, so
  they need no bootstrap configuration.

Three existing properties of FIPS make this cheaper than it sounds:

1. **Address and key are the same thing.** `NodeAddr` is
   `SHA-256(pubkey)[..16]` (`src/identity/node_addr.rs`) and the IPv6
   address is `fd` plus the first 15 bytes of that
   (`src/identity/address.rs`). A record signed by a pubkey is
   therefore bound to exactly one address. Nobody can announce a
   service on another node's address, a record never needs to carry an
   address, and public records can be cached or relayed by untrusted
   parties without losing authenticity.
2. **Bloom filters already answer "which direction".** Every node
   gossips a filter of what is reachable through it
   ([fips-bloom-filters.md](fips-bloom-filters.md)). A second, small
   filter for service keys reuses the same propagation rules and the
   same code.
3. **Sessions authenticate both ends.** An FSP session is Noise XK,
   so a provider knows the npub of whoever is asking. That is all an
   npub-list group needs.

## When to use it

- **You host something for the whole mesh** — a relay, a Blossom
  server, a wiki — and want it found without publishing your npub and
  port anywhere.
- **You host something for a few people** — family photos, a lab
  dashboard — and want only them to find it, or to even learn that it
  exists.
- **You run a client** that should use "the nearest relay" or "any
  Blossom server my group runs" and keep working as nodes come and go.
- **You are bootstrapping a Nostr-based application inside FIPS** and
  need to find the first relay before any relay-based discovery can
  work.

It is not a naming system. There is no global, unique, human-readable
name for a service; see [Names](#names).

## Under the covers

### Overview

Discovery is split into three planes. Only the first two are required.

| Plane | Question answered | Mechanism |
| ----- | ----------------- | --------- |
| Locate | Which nodes offer service key *K*, and where are they in the tree? | A service bloom filter, and `ServiceQuery` / `ServiceResponse` link messages that carry cacheable signed *locators* |
| Fetch | What exactly does that node offer? | Nostr events over an FSP session to a reserved port |
| Index | What is out there? (browse, search) | In-mesh Nostr relays acting as directories |

A typical public lookup:

```text
client                         transit                      provider
  |                               |                             |
  |  pre-check: some peer's service filter contains K           |
  |-- ServiceQuery(K, ttl=1) ---->|  (no provider, empty cache) |
  |-- ServiceQuery(K, ttl=4) ---->|-- forwarded along tree ---->|
  |                               |                             |  locator
  |                               |                             |  (signed at
  |                               |                             |  most every
  |                               |                             |  30 s)
  |<-- ServiceResponse(locator) --|<-- reverse path ------------|
  |                               |  verify, cache locator
  |  verify locator, cache coords, prime identity cache         |
  |                                                             |
  |== FSP session (Noise XK) to port 257 ======================>|
  |-- LIST nostr-relay ---------------------------------------->|
  |<-- RECORD (signed Nostr event) -----------------------------|
  |  verify event signature and that event.pubkey == session peer
  |
  `-> connect to [fd..]:7777
```

The next client that asks the same transit node within the locator's
lifetime is answered from its cache.

### Service records

A service record is a parameterized replaceable Nostr event, tentative
**kind 37196**, the sibling of the overlay advert kind 37195
(`src/nostr/types.rs`,
[../reference/nostr-events.md](../reference/nostr-events.md)). It is
issued under the node's identity key; there is no separate service
key.

```json
{
  "kind": 37196,
  "pubkey": "<node pubkey>",
  "created_at": 1790000000,
  "tags": [
    ["d", "nostr-relay:tcp:7777"],
    ["s", "nostr-relay"],
    ["port", "7777", "tcp"],
    ["scheme", "ws"],
    ["name", "andre's relay"],
    ["expiration", "1790003600"]
  ],
  "content": "",
  "id": "…",
  "sig": "<signed by the node key>"
}
```

| Tag | Required | Meaning |
| --- | -------- | ------- |
| `d` | yes | Instance identifier, `<type>:<proto>:<port>`. Makes the event replaceable per service instance; the protocol is part of it so that the same type on the same port over `tcp` and `udp` stays two instances. |
| `s` | yes | Service type. Single-letter so that Nostr relays index it and `{"#s": [...]}` filters work on directories. |
| `port` | yes | Port and transport protocol (`tcp` or `udp`) on the node's FIPS address. |
| `scheme` | no | URL scheme a client should use (`http`, `ws`, …). |
| `path` | no | URL path prefix. |
| `name` | no | Self-asserted display label. Not unique, not trusted. |
| `expiration` | yes | NIP-40 expiry. Records are short-lived and refreshed. |

`content` may hold service-specific JSON (for a relay, a subset of its
NIP-11 document). A record is capped at 1024 bytes so that it always
fits in one FSP datagram; FIPS does not fragment
([fips-mtu.md](fips-mtu.md)).

Rules a consumer applies:

- The address to connect to is *derived from `pubkey`*. The record has
  no address field on purpose.
- A **public** record must carry a valid signature. A **restricted**
  record is unsigned and is accepted only on the fetch port, where the
  Noise XK session authenticates `pubkey` instead; see
  [Records by scope](#records-by-scope).
- When fetched directly, `pubkey` must equal the authenticated session
  peer. A node only serves its own records on the fetch port.
- Newer `created_at` replaces older for the same `(pubkey, d)`.
  Expired records are dropped. Withdrawal is expiry, or a NIP-09
  delete on directories — the same pattern as `PublishPlan::Delete`
  in `src/nostr/advert.rs`.
- Any URL or relay hint inside a record must point at `fd00::/8` or a
  `.fips` name. Anything else is ignored. This is the "no legacy
  internet" rule in one line.

**Service types** are short lowercase strings. Where an IANA service
name exists it is used (`http`, `https`, `ssh`); FIPS adds
`nostr-relay`, `blossom` and `fips-directory`. Unknown types are
legal; the registry is a convention, not a gate.

Prior art that was considered and not reused as-is: NIP-66 relay
discovery (kind 30166) describes relays as observed by monitors, not
self-announced endpoints; NIP-89 (kind 31990) maps event kinds to
handler apps; DNS-SD has the right vocabulary (and is reused for the
DNS view below) but no signatures and assumes multicast.

#### Records by scope

The record always says *what* is offered. The group never appears in
it: a group name is a local label and a group secret is a secret. The
scope (see [Scopes and groups](#scopes-and-groups)) decides who
receives the record, over which channel, and whether it is signed.

| Scope | Form | Delivered over |
| ----- | ---- | -------------- |
| `public` | Signed event, as above | Fetch port; optionally directories |
| `allow` (npub list) | Unsigned event | Fetch port only, to session peers on the list |
| `secret` | Unsigned event | Fetch port only, after the group proof |
| `secret` + `allow` | Unsigned event | Fetch port only, after the group proof, to session peers on the list |

The rule behind the table: **a signature exists so that third parties
can carry a record.** Public records are cached, relayed and stored on
directories, so they are signed. Restricted records are never carried
by anyone but their author, so they are never signed:

```json
{
  "kind": 37196,
  "pubkey": "<node pubkey>",
  "created_at": 1790000000,
  "tags": [
    ["d", "blossom:tcp:3000"],
    ["s", "blossom"],
    ["port", "3000", "tcp"],
    ["scheme", "http"],
    ["name", "family photos"],
    ["expiration", "1790003600"]
  ],
  "content": "",
  "id": "<sha256 of the NIP-01 serialization>"
}
```

The Noise XK session on the fetch port already authenticates the
provider, and `pubkey` must equal the session peer. A signed copy
would be transferable: a member who leaks it could prove to outsiders
that the node offers the service. Without the signature the record is
deniable, and it cannot be cached or relayed by anyone else, which is
what a restricted scope wants. The Locate plane keeps this property:
a sealed `ServiceResponse` carries no signature either (see
[Scopes and groups](#scopes-and-groups)).

An unsigned event cannot be published to a relay at all, so restricted
records need no NIP-70 `["-"]` marker.

Groups whose secrets are managed by an external key-agreement protocol
may have a further channel for their records. The Marmot case is
described in
[fips-service-discovery-marmot.md](fips-service-discovery-marmot.md).

### Locate: finding providers

#### The service filter

Each `public` or `secret` announcement contributes one 16-byte
**service key**:

```text
public key  = SHA-256("fips-svc-v1" || type)[..16]
```

Service keys do **not** go into the routing bloom filter. They go into
a second filter, the *service filter*, gossiped in its own link
message, `ServiceFilterAnnounce = 0x21`, next to `FilterAnnounce =
0x20` (`src/proto/link.rs`). The payload layout and the code are those
of `FilterAnnounce` (`BloomFilter`, `src/proto/bloom/core.rs`); the
state is a second instance of `BloomState`
(`src/proto/bloom/state.rs`) whose own entries are the node's service
keys instead of its address and leaf dependents. Propagation rules are
identical: along tree edges, split horizon, debounced, rebuilt from
scratch on every recompute, subject to its own inbound FPR cap.
All peers receive the filter; only tree-peer filters are merged.

The service filter is small, tentatively size class 0 (512 bytes,
4,096 bits, k = 5). It holds one entry per service *type* and one per
*(secret group, type)* in the whole mesh, not one per node: 200
distinct keys give an FPR of 0.05 %, 500 give 2 %.

**Capability is implicit.** A node sends one `ServiceFilterAnnounce`
(possibly empty) when a link comes up, and afterwards only to peers
from which it has received one. `ServiceQuery` is sent only to peers
whose service filter is known. An old node logs one unknown message
per link-up and is otherwise left alone.

Consequences worth stating:

- **A filter match means a query can get there.** The filter
  propagates only across nodes that also forward queries, so the
  pre-check never promises providers behind a node that would drop
  the query.
- **Routing is untouched.** Service keys consume no routing-filter
  capacity, cannot push a routing filter over
  `node.bloom.max_inbound_fpr`, and a poisoned service filter harms
  discovery only. This holds on constrained nodes too, which the
  scaling plan in [fips-bloom-filters.md](fips-bloom-filters.md)
  expects to *fold* received routing filters down to a smaller size.
- **Cost is per type, not per provider.** Every provider of
  `nostr-relay` inserts the same key, which sets the same bits. A
  thousand relays cost the mesh one filter entry.
- **Withdrawal works.** Filters are rebuilt from scratch on every
  recompute, so a removed announcement disappears with the next
  update. Nothing relies on deleting from a bloom filter.
- **A filter says "that way", not "how near" or "how many".** A
  popular key is a *true* positive on almost every tree edge: the
  upward filter holds the subtree, the downward filter the rest of the
  mesh. No filter size changes that. Bounding the search is the job of
  the mechanisms under [Bounding the search](#bounding-the-search).

Why not the routing filter, as an earlier draft had it, is recorded
under [Alternatives considered](#alternatives-considered).

#### ServiceQuery and ServiceResponse

Two new link messages take the next free slots in the Discovery block
of `LinkMessageType` (`src/proto/link.rs`, currently `0x30` and
`0x31`). Both carry a version byte, following `TreeAnnounce`.

`ServiceQuery = 0x32`:

| Field | Size | Notes |
| ----- | ---- | ----- |
| `version` | 1 | `0x01` |
| `flags` | 1 | bit 0 `A`: group auth present |
| `request_id` | 8 | Random; fresh per attempt |
| `key` | 16 | Service key (public or blinded) |
| `ttl` | 1 | Hop limit, decremented per forward; a node that receives `ttl = 1` answers but does not forward. Clamped by every transit node to its own lookup TTL |
| `max_responses` | 1 | Cap on responses relayed per request. Clamped by every transit node to its own `query.max_responses` |
| `min_mtu` | 2 | As in `LookupRequest` |
| `timestamp` | 4 | Only with `A`. Unix seconds |
| `auth` | 16 | Only with `A`; see [Scopes](#scopes-and-groups) |

Unlike `LookupRequest`, the query names **no origin**. Responses
return by reverse path through `request_id`, and if a reverse-path
entry has expired the response is dropped and the origin's retry
covers it. Every node in the search radius would otherwise learn
*who* is looking for *what*. A direct neighbour can still guess that a
query with a ring-start `ttl` originated next door; nodes further out
cannot.

`ServiceResponse = 0x33`:

| Field | Size | Notes |
| ----- | ---- | ----- |
| `version` | 1 | `0x01` |
| `flags` | 1 | bit 0 `E`: body is sealed; bit 1 `C`: answered from a transit cache |
| `request_id` | 8 | Echo |
| `key` | 16 | Echo |
| `path_mtu` | 2 | Transit annotation, outside the signature |
| `responder` | 32 | Responder x-only pubkey |
| `coords` | 2 + 16×n | Responder tree coordinates |
| `issued_at` | 4 | Unix seconds |
| `proof` | 64 | Schnorr over `"fips-svc-loc-v1" ‖ key ‖ coords ‖ issued_at` |

`responder`, `coords`, `issued_at` and `proof` together are a
**locator**: a short-lived, self-contained statement "I provide *K*
and I am at these coordinates". It deliberately does *not* cover
`request_id`:

- A provider signs a locator when its coordinates change and at most
  every `locator_refresh_secs` (30 s) per key — not once per query. A
  flood of queries costs it no signatures.
- A locator is valid for any requester until `issued_at +
  locator_max_age_secs` (120 s; 30 s of clock skew into the future is
  tolerated). That is what lets transit nodes cache it. The clock
  requirement is the one record `expiration` already imposes.
- Freshness per request is not needed. A locator is a routing hint;
  liveness is proven by the FSP session that follows. If the
  coordinates have gone stale within the two minutes, session setup
  falls back to an ordinary `LookupRequest` for the `NodeAddr`, which
  the identity cache can already supply.

The requester checks the proof against the carried pubkey, derives the
`NodeAddr` from it, caches the coordinates as `Verified`
(`src/cache/entry.rs`) and primes the identity cache the same way a
DNS resolution does today (`DnsResolvedIdentity` in
`src/upper/dns.rs`). After that the provider is routable.

With flag `E` the body has a different layout and carries no
signature; see [Scopes](#scopes-and-groups). Coordinates from a sealed
response are cached as `Hint`, never as `Verified`: any group member
can produce one, so they must not outrank or overwrite coordinates
that a signed proof established.

#### Forwarding

Forwarding borrows from the lookup machinery
([fips-mesh-operation.md](fips-mesh-operation.md), "Bloom-Guided Tree
Routing") and changes it where a shared key behaves differently from a
unique address:

- `plan_forward` sends the query to tree peers whose *service* filter
  contains the key, falling back to non-tree peers when no tree peer
  with a known service filter matches.
- Responses return by reverse path through the `recent_requests` table
  (`src/proto/lookup/state.rs`). There is no coordinate fallback.
- A node that holds a matching announcement answers **and** keeps
  forwarding, because other providers may lie further on.
- **The forward limiter is keyed by (key, inbound peer)**, not by key
  alone. A lookup target is one node, so collapsing rapid lookups for
  it is harmless. A service key is shared by every client in the mesh:
  a per-key interval would let one `nostr-relay` query per two seconds
  through a busy transit node and drop everyone else's, and anyone
  could suppress a type by asking for it every two seconds. Keyed per
  inbound peer, a client can only ever suppress queries that arrive
  over the same link as its own.
- **A suppressed query is answered, not dropped**, when the transit
  node can answer it from its locator cache (next section). The
  limiter then bounds flooding without costing liveness.
- The per-peer eviction accounting and the transit forward limiter
  apply as they do to lookups.

#### Transit locator cache

A transit node verifies every plain (not sealed) locator it relays and
keeps the nearest `max_responses` per key until they pass
`locator_max_age_secs`, for a bounded number of keys (LRU). On a plain
query:

1. It answers with its cached locators, flag `C` set.
2. If it can supply `max_responses` fresh locators, it forwards only
   one query in `cache_forward_every` (4) for that key, again subject
   to `ttl` and the forward limiter.
3. Otherwise it forwards, subject to `ttl` and the forward limiter.

Two rules keep the cache from becoming a way to end other people's
searches:

- **A full cache never stops forwarding completely.** "Nearest wins"
  plus "full means stop" would let a sybil with `max_responses`
  identities next to a busy transit node win every refill and cut
  everyone behind that node off from all other providers — without
  being on anyone's path. With one query in four still forwarded,
  honest locators keep arriving.
- **No single downstream peer may hold more than half of a key's
  cache slots**, the same share rule that applies to the
  `max_responses` counter. Identities that all sit behind one link
  compete with each other for that half; the other half goes to
  locators that arrived over other links, even if they are further
  away.

A full cache is refreshed by the forwarded quarter and otherwise
drains within two minutes. New providers therefore become visible
within that time, and a withdrawn one disappears within it.

An earlier draft left transit answering out because it "lets a node
suppress competitors' answers". A hostile *transit node* can do that
with or without a cache: it can drop any response it does not like.
What the cache adds is the hostile *provider* next to an honest
transit node, which the two rules above address. Both threats are
covered under [Security](#security-and-threat-model).

Sealed responses cannot be cached — they are encrypted per request —
so blinded keys always travel to the provider. Group queries are few
by nature.

#### Bounding the search

This is where a service query differs from a lookup, and it is the
main cost of the design. A `NodeAddr` lives in exactly one place, so a
lookup follows one branch. A query for a *rare* key is guided the same
way. A *popular* key is present behind almost every tree edge, so a
naive query reaches the whole tree.

The proposal bounds this with:

- **Expanding-ring search.** The origin tries `ttl` 1, 2, 4, 8, 16,
  then the full lookup TTL, with a fresh `request_id` each time, and
  stops as soon as it has enough verified answers. The first ring goes
  to *every* direct peer with a matching service filter, tree or not,
  so a provider (or a warm cache) one mesh hop away is always asked.
  Inner rings are re-visited, but the cost is geometric and the common
  case — a provider or a cache nearby — ends after the first rings.
  This also gives **locality for free**: nearest providers answer
  first. The ring is a courtesy of the origin, not a bound the mesh
  can rely on; the bounds that hold against a hostile origin are the
  clamps, the limiter and the cache.
- **`max_responses`.** A transit node relays at most that many
  distinct responses per `request_id`, replacing the lookup's single
  `response_forwarded` flag with a small counter and a set of
  responder digests. While a branch it forwarded to has not answered
  yet, no single downstream peer may fill more than half of the
  counter.
- **Clamps.** `ttl` and `max_responses` are chosen by the origin, so
  every transit node clamps them to its own configuration.
- **Transit cache.** One flood per key, per inbound peer, per two
  seconds refills caches along its path; everything else in that time
  is answered locally.
- **No per-query signatures.** Providers sign per refresh interval.
  The signing budget (`LookupSignRateLimiter`,
  `src/node/rate_limit.rs`) is not touched by service queries.
- **Pre-check.** If no peer's service filter contains the key the
  origin reports "no providers" without sending anything, as lookups
  do.
- **Origin caching.** Positive results are cached for
  `cache_ttl_secs`; negative results for a shorter time.
  `query.prefetch` keeps named types warm in the background.

False positives send a query into a subtree with no provider. The TTL
bounds the damage, and with a few hundred keys in the service filter
the rate is far below that of the routing filter.

#### Partial deployment

`dispatch_link_message` (`src/node/dataplane/dispatch.rs`) logs and
drops unknown link types. Neither service filters nor queries are sent
to a node that has not announced a service filter itself, so nothing
is lost silently: discovery covers the part of the tree that is
connected to the origin through upgraded nodes, the pre-check reports
exactly that part, and a search ends with "no providers" instead of a
row of timeouts. An old node between two upgraded regions separates
them; non-tree links between upgraded nodes bridge that only for
direct neighbours (ring 1).

No existing message changes, so the feature is additive rather than
wire-format-breaking in the sense of
[../branching.md](../branching.md).

### Fetch: reading the records

The provider serves its records on FSP port **257**, the first free
port in the "FIPS standard services" tier (256–1023; 256 is the IPv6
shim — `src/native/protocol.rs`,
[../reference/native-api.md](../reference/native-api.md)). Native API
clients are refused ports in this tier in both directions, so a local
program cannot impersonate the directory port. Dispatch is one more
arm beside `FSP_PORT_IPV6_SHIM` in `src/node/handlers/session.rs`.

The protocol is a few datagrams inside an established session:

| Message | Direction | Body |
| ------- | --------- | ---- |
| `LIST` | client → provider | `req_id`, service key (or zero for "all I may see"), optional group proof |
| `RECORD` | provider → client | `req_id`, index, total, one Nostr event |
| `END` | provider → client | `req_id`, count |

FSP is unreliable; the client retries `LIST` on timeout. A provider
answers only with its own records, only with those the requester's
scope allows, and at most `max_announcements` of them.

**Why two steps instead of putting the record in `ServiceResponse`?**
The link-layer budget is about 1243 bytes on a 1280-byte transport and
less on others, coordinates grow with tree depth, and a Nostr event is
JSON. More importantly, the link-layer query is unauthenticated, while
the session knows who is asking — and that is what scoping needs.
Inlining public records as an optimization is an open question.

### Scopes and groups

FIPS has no group concept today; the closest thing is the peer ACL
(`src/node/acl.rs`, `peers.allow` / `peers.deny`). This proposal adds
two kinds of group and lets every announcement choose.

| Scope | Locate | Fetch | Hidden from outsiders | Still visible |
| ----- | ------ | ----- | --------------------- | ------------- |
| `public` | Plain key, anyone is answered | Anyone | Nothing | — |
| `allow: <group>` | **None.** Members ask the nodes they know directly | Only session peers whose npub is in the group | That the service exists, its type, and the record contents | On-path nodes see that a member opened a session to the provider |
| `secret: <group>` | Blinded key, only authenticated queries are answered, response sealed | Requester must prove knowledge of the group secret | The service type, the record contents, and the provider's identity from everyone who is not next to it | A stable opaque key in service filters; the provider's tree neighbours can tell that the key originates in its subtree; query-then-session correlation |
| both | As `secret` | Both checks | As `secret`, plus per-member revocation at Fetch | As `secret` |

**npub-list groups** copy the peer ACL idiom: a file per group under
`/etc/fips/groups/` (platform path as for `peers.allow`), one npub or
host alias per line, hot-reloaded once per tick. Membership is checked
against the Noise-authenticated session peer. Removing a member is
deleting a line.

An `allow` announcement stays out of the Locate plane entirely: no
service key, no filter entry, no answer to a `ServiceQuery`. The
link-layer query is unauthenticated, so a provider could not tell a
member from a stranger there; answering would tell the whole mesh
which node runs the family's Blossom server and fill everyone's
`find blossom` with providers that then refuse the fetch. Instead a
member finds the group's services by sending `LIST` to the nodes it
already knows — by default every entry of its own copy of the group
file, at most `max_group_poll` (64) of them. Groups of this kind are
small, the sessions are cheap and the results are cached. An `allow`
group that outgrows polling wants a secret.

**Shared-secret groups** supply two 32-byte values: a *locate secret*
that stays stable, and an *epoch secret* that may rotate. Keys are
derived with HKDF-SHA256 (already a dependency): `k_key` from the
locate secret, `k_auth` and `k_seal` from the epoch secret. Where the
two values come from is a provisioning choice:

- **Static.** One file, mode 0600, distributed out of band. Both
  values derive from it and never change until the operator replaces
  the file on every member. Simple, no dependencies, no revocation.
- **Managed.** An external process pushes the values, and optionally
  a roster, over the control socket and pushes them again when they
  change; see
  [Group files and provisioning](#group-files-and-provisioning). Any
  group key agreement can sit behind that interface.
  [fips-service-discovery-marmot.md](fips-service-discovery-marmot.md)
  proposes Marmot (MLS with Nostr identities) for it, and compares
  the two kinds side by side, costs included, under
  [Static or Marmot-managed?](fips-service-discovery-marmot.md#static-or-marmot-managed).

The discovery wire protocol is identical in both cases:

- The service key is blinded:
  `HMAC(k_key, type)[..16]`. Outsiders see an opaque key in filters
  and queries and cannot tell what it names or build it themselves.
- The query carries `auth = HMAC(k_auth, request_id ‖ key ‖
  timestamp)[..16]`. A provider answers only if it verifies and the
  timestamp is within 30 seconds of its clock. Without `auth`, any
  transit node could replay an observed key and learn that someone
  answers; without the timestamp it could replay a whole observed
  query after the dedup window and learn the direction and round-trip
  time of a provider. The timestamp alone does not close that: the
  lookup dedup table forgets a `request_id` after
  `recent_expiry_secs` (10 s), well inside the 30-second tolerance. A
  provider therefore keeps its own set of authenticated `request_id`s
  for the full acceptance window (60 s, both directions of skew) and
  answers each one once.
- The response is sealed. After `path_mtu` it carries a random 12-byte
  `nonce` and one ChaCha20-Poly1305 ciphertext over `responder ‖
  coords ‖ issued_at`, under a key derived from `k_seal` and
  `request_id`, with `version ‖ flags ‖ request_id ‖ key` as
  associated data. Several providers answer the same `request_id`
  under the same key by design, so the nonce is explicit and random;
  it must never be derived from the request. Transit nodes need only
  `request_id` for reverse-path routing, and deduplicate sealed
  responses by body hash.
- **A sealed response carries no Schnorr proof.** The AEAD tag proves
  the responder knows the group secret, and the Noise XK session that
  follows proves it holds the claimed key. A signature over the
  blinded key would be a transferable proof that the node serves this
  group — exactly what the unsigned record avoids. The price: a member
  can forge a locator that names another member. The session to that
  node then finds no such service, and the forger has gained nothing
  it could not do by lying in a record. Because the coordinates in it
  are unauthenticated too, the requester caches them as `Hint`
  (`src/cache/entry.rs`); cached as `Verified`, a forged locator would
  let a member plant wrong coordinates for another member's address.
- On the fetch port the requester adds `HMAC(k_auth, "fetch" ‖
  client pubkey ‖ provider pubkey ‖ req_id)`, binding the proof to
  this session.
- A provider checks `auth` against the current epoch secret and a
  small window of previous ones, so members that lag an epoch are
  still served.

Limits that the document should not hide:

- An on-path node still sees a query and, shortly afterwards, a
  session from the same direction to node *P*. Session traffic is
  opaque, but the correlation exists. This is the same class of
  metadata exposure described under "Privacy Considerations" in
  [fips-mesh-operation.md](fips-mesh-operation.md).
- A blinded key is testable in filters. Whoever has seen it in a query
  can test the service filters of its own peers for it, and the tree
  neighbours of a provider see it appear in a filter that covers only
  a small subtree. The key hides *what* is offered from everyone, and
  *who* offers it only from nodes that are not adjacent.
- A blinded key is stable, so an observer can track "the same unknown
  thing" over time. A managed group can rotate the locate secret; a
  static group cannot without touching every member.
- Every (secret group, type) pair is an entry that the whole mesh
  carries. The announcement cap bounds honest nodes; the service
  filter's inbound FPR cap bounds the total; a node that floods keys
  degrades discovery, never routing.
- Removing a member from a *static* group means replacing the file
  everywhere. Combine it with an npub list, or use a managed group,
  when per-member revocation matters.

### Index: in-mesh directories

Locate and Fetch answer "who offers *this type*". They are poor at
"what is out there". For that, a Nostr relay inside the mesh can act
as a directory:

- It announces itself with `s = nostr-relay` and, if it accepts
  service records, also `s = fips-directory`.
- Nodes configured to do so publish their `public` records to the
  directories they discover, over `fips0`, with the `nostr-sdk` client
  that is already linked in for rendezvous.
- Clients run ordinary filters, for example
  `{"kinds":[37196],"#s":["blossom"]}`.

Records from a directory are authentic (signed, address-bound) but say
nothing about liveness, so they enter the cache as unverified until a
Locate or a lookup confirms the provider is reachable. A directory can
withhold records but not forge them.

Restricted records never reach a directory: they are unsigned, and the
author pubkey alone would reveal that a node belongs to *some* group.

Directories are never required. Locate and Fetch work with zero
relays, and that is what breaks the circle of needing a relay to find
a relay.

### Names

There is no global namespace and this proposal does not add one. A
`name` tag is a self-asserted label, displayed next to the npub and
never used as an identifier. Trust in a provider comes from places
that already exist: the operator's `/etc/fips/hosts` aliases
(`src/upper/hosts.rs`), configured peers, and group membership.

## Configuration and tooling

### Announcing

```yaml
node:
  services:
    enabled: true
    announce:
      - type: nostr-relay
        port: 7777
        proto: tcp
        scheme: ws
        name: "andre's relay"
        scope: public
      - type: blossom
        port: 3000
        scope: { allow: family }
      - type: http
        port: 8080
        scope: { secret: lab, allow: lab-admins }
    groups:
      family:     { members_file: /etc/fips/groups/family }
      lab-admins: { members_file: /etc/fips/groups/lab-admins }
      lab:        { secret_file: /etc/fips/groups/lab.key }   # static
      guild:      { managed: true }   # secrets and roster pushed over
                                      # the control socket at runtime
    query:
      ttl_steps: [1, 2, 4, 8, 16, 64]
      max_responses: 8
      cache_ttl_secs: 300
      prefetch: [nostr-relay]         # types kept warm in the cache
      max_group_poll: 64
    locator_refresh_secs: 30
    locator_max_age_secs: 120
    cache_forward_every: 4            # a full transit cache still
                                      # forwards one query in four
    publish_to_directories: false
    trust:
      providers: any          # any | known
```

`node.services.*` sits beside `node.lookup.*` (resolve a known
address) and `node.rendezvous.*` (find peers) in `src/config/node.rs`
and follows the same struct, merge and validate conventions.

Announcing is always explicit. The daemon never announces a port just
because something listens on it. The existing listener inventory
(`src/control/listening.rs`) is used the other way round: to warn when
an announced port has no listener or is closed in the `fips0` nftables
baseline ([fips-security.md](fips-security.md)).

Applications can also register at runtime over the control socket
(`services_announce` / `services_withdraw`), with a lease that lapses
if it is not renewed, so a relay that exits stops being announced. The
daemon signs on the application's behalf, since a public record must
be signed by the node key. The control socket never offers a generic
signing command; it signs specific statement types only — here,
kind-37196 records and locators.

### Group files and provisioning

The three kinds of group are provisioned differently, and who needs
what differs too.

**npub-list group.** A plain list of public keys, one member per line:
an npub, or a host alias that `/etc/fips/hosts` maps to an npub. The
format is that of `peers.allow` (`src/node/acl.rs`): `#` starts a
comment, the file is hot-reloaded.

```text
# /etc/fips/groups/family
npub1q7x…k3m        # andre laptop
npub1zx4…9ua        # living-room server
mum-phone           # alias from /etc/fips/hosts
```

- The file contains nothing secret. A requester's identity is already
  proven by its FSP session; the provider only looks the pubkey up.
- A provider uses the file to decide whom to serve. A client uses it
  as the list of nodes to ask, because `allow` services are not in the
  Locate plane. A client that only ever asks one node can name it
  instead: `fipsctl services find blossom --from living-room`.
- Each node keeps its own copy. Removing a member is deleting the
  line on the providers.
- The `ALL` wildcard of `peers.allow` is deliberately **not**
  supported. It would silently turn a group scope into a public one.

**Static secret group.** The file holds the 32-byte secret and no
public keys.

```text
# /etc/fips/groups/lab.key   (mode 0600)
9f2c…64 hex characters…e1
```

- Every member needs a copy, clients included, because a client has to
  compute the blinded key and authenticate its query.
- There is no member list. Whoever has the file is a member, and
  removing someone means replacing the file everywhere.

**Managed group.** There is no file to edit; the configuration entry
is only `guild: { managed: true }`. An external process pushes three
things over the control socket (`services_group_update`), and pushes
them again whenever the group changes:

| Pushed value | Used for |
| ------------ | -------- |
| Locate secret | Blinding the service keys. Stable; may be rotated |
| Epoch secrets, current and a few retained | Authenticating queries, sealing responses |
| Roster (optional) | The npub list checked on the fetch port; with it, `secret` and `allow` collapse into one group |

Every member runs the managing process, providers and clients alike.
The daemon links nothing for it and the forwarding path never sees it.

`fipsctl show services --groups` lists what the daemon currently
holds, without ever printing a secret:

```text
$ fipsctl show services --groups
GROUP   KIND     EPOCH  MEMBERS  SOURCE
family  list     —      3        /etc/fips/groups/family
lab     static   —      —        /etc/fips/groups/lab.key
guild   managed  42     7        control socket (updated 3m ago)
```

### Finding

```text
$ fipsctl services find nostr-relay
NAME            NPUB            ADDRESS              PORT      DIST  SCOPE   EXPIRES
andre's relay   npub1q7…k3m     fd3a:91c2:…:7e10     7777/tcp  2     public  58m
—               npub1zx…9ua     fd71:0be4:…:02c9     4848/tcp  5     public  12m

$ fipsctl services find blossom --group family --json
```

`find` is asynchronous and follows the start/poll pattern of `fipsctl
probe` (`src/control/probe.rs`, `src/control/commands.rs`). With
`--group` it runs a blinded Locate for a secret group and polls the
members for an npub-list group. `fipsctl show services` lists local
announcements and the discovery cache from the read-only snapshot
path. The same commands over the control socket are the API for local
applications
([../reference/control-socket.md](../reference/control-socket.md)).

### DNS view

The `.fips` responder (`src/upper/dns.rs`) answers only AAAA today.
Service discovery gives it a second data source. The view is complete
DNS-SD (RFC 6763) over unicast DNS, so that browsing clients work, and
it also answers a plain RFC 2782 SRV query on the service name for
applications that only know that.

| Query | Type | Answer |
| ----- | ---- | ------ |
| `_services._dns-sd._udp.fips` | PTR | Known service types (DNS-SD browse) |
| `_nostr-relay._tcp.fips` | PTR | One instance name per provider: `<instance>._nostr-relay._tcp.fips` |
| `<instance>._nostr-relay._tcp.fips` | SRV | Port, target `<npub>.fips` |
| `<instance>._nostr-relay._tcp.fips` | TXT | `scheme=`, `path=`, `name=` of *that* instance |
| `_nostr-relay._tcp.fips` | SRV | One record per provider, as above. Priority follows tree distance |
| `_blossom._tcp.family.group.fips` | any of the above | Same, restricted to group `family` |
| `nostr-relay.svc.fips` | AAAA | Address of the nearest verified provider, sticky for the DNS TTL |

`<instance>` is `<NodeAddr as 32 hex digits>-<port>`, which is unique,
fits a label and is mapped back through the discovery cache. An npub
is 63 characters, exactly the DNS label limit, so `<npub>.fips` works
as an SRV target and resolves through the existing path, which also
primes the identity cache.

`svc` and `group` become reserved labels in `validate_hostname`, so a
group label can never collide with a host alias.

Points the implementation has to handle:

- **Latency.** An SRV answer needs a Locate *and* a Fetch per
  provider, which does not fit into a resolver timeout. The responder
  therefore answers from the discovery cache only. On a miss it starts
  the search, waits a bounded time (1.5 s), and then answers empty
  with a negative TTL of one second, so that the application's retry
  hits the cache. Types listed under `query.prefetch` are always warm.
- **Size.** The responder uses a 512-byte buffer and no EDNS0. An SRV
  answer is roughly 90 bytes, so about four providers fit. Either cap
  the answer count or add EDNS0.
- **AAAA cannot carry a port.** `<type>.svc.fips` helps applications
  that cannot do SRV only when the port is conventional.
- **Locality of the DNS view.** The responder keeps dropping queries
  that arrive on `fips0` (`is_mesh_interface_query`). DNS is a view of
  *this node's* discovery results, not a mesh-wide directory service.

#### Trust in the DNS view

A DNS answer is unsigned, and it is only as trustworthy as the node
that serves it. That is acceptable because of where the two hops of a
lookup run:

```text
app --DNS--> own daemon --ServiceQuery--> peers --> ... --> provider
    (loopback, unsigned)  (mesh: ring search, forwarded hop by hop)
                       <--ServiceResponse-- signed locator
                       == FSP session ==> provider: fetch the record
            discovery cache <-- verified record
app <--DNS-- answer built from the cache
```

- **The DNS hop is local.** The responder binds to `::1` by default
  and drops queries that arrive on `fips0`, so no node elsewhere in
  the mesh can inject or alter a DNS answer.
- **The mesh hop is verified by the node that runs the search.**
  Locators and public records are signed, restricted records arrive
  over an authenticated session, and only verified records enter the
  cache the answer is built from. Transit nodes and other providers
  can drop answers or compete for "nearest"; they cannot forge a
  record.
- **The DNS view then discards the proof.** An answer is a name, a
  port and an address; the signatures stay in the daemon's cache.
  Verification happens inside the node that searched, and DNS hands
  out the result without evidence.

DNSSEC does not fit. `.fips` is a synthetic zone without a delegation,
so no validator has a trust anchor for it, and answers differ per node
by design ("nearest to me"), so there is no one zone to sign. Signing
answers on the fly would protect a loopback hop that only the local
machine uses.

The view therefore needs care wherever the answering node is *not* the
client's own daemon: a LAN host behind `fips-gateway`
([fips-gateway.md](fips-gateway.md)), or a responder whose
`dns.bind_addr` was widened beyond loopback. What such a node can do:

| Name | A malicious answering node can | Detectable by the client? |
| ---- | ------------------------------ | ------------------------- |
| `<npub>.fips` | Return another node's address | In principle: the address is `fd` plus the first 15 bytes of `SHA-256(pubkey)`. An unmodified application never checks, and behind a gateway the address is a virtual one from the NAT pool |
| Host alias | Point it anywhere | No. Aliases come from the answering node's own hosts file |
| `_<type>._tcp.fips`, `<type>.svc.fips` | Return its own node, omit or reorder providers | No |

A host behind a gateway already trusts that gateway with all of its
plaintext traffic, so DNS adds no new trust there. What limits the
damage is the application layer: Nostr events are signed by their
authors and Blossom blobs are addressed by their hash, so a false
relay or server can withhold and observe but not forge. Plain HTTP has
no such check, and no certificate authority exists for `.fips` names.

Guidance:

- A client that must not trust its resolver uses `fipsctl services
  find`, the control socket of its *own* node, or the in-mesh
  directories. All three carry records that the client's own node
  verifies.
- An application that knows an npub should compute the address rather
  than ask for it.
- `dns.bind_addr` stays on loopback unless the hosts it serves trust
  this node as they would trust a gateway.

## Rate limits and safeguards

| Guard | Where | Proposed default |
| ----- | ----- | ---------------- |
| Announcements per node | provider | 16 |
| Record size | provider, consumer | 1024 bytes |
| Record lifetime | provider | 1 h, refreshed at half-life |
| Locator signatures | provider | one per key per 30 s, and on coordinate change |
| Locator lifetime | consumer, transit | 120 s, 30 s future skew |
| Expanding ring | origin | `[1, 2, 4, 8, 16, 64]` |
| `ttl`, `max_responses` clamp | transit | local lookup TTL, local `max_responses` |
| Responses relayed per request | transit | 8, at most half from one downstream peer while others are pending |
| Forward interval per (key, inbound peer) | transit | 2 s; suppressed plain queries are answered from cache |
| Locator cache | transit | 8 per key, at most half from one downstream peer, 256 keys, LRU |
| Forwarding with a full cache | transit | one query in 4 per key |
| Answered group `request_id`s | provider | remembered for 60 s, each answered once |
| Queries per link peer | transit | token bucket |
| Service filter inbound FPR | every node | own cap, default as `node.bloom.max_inbound_fpr` |
| Fetch requests per session | provider | token bucket |
| Group members polled | origin | 64 |
| Discovery cache | origin | 2048 records, expired-first eviction |
| DNS wait on cache miss | responder | 1.5 s |

## Security and threat model

- **Impersonation.** Not possible at the record level: the address is
  derived from the key that signs the record or terminates the
  session. A record can lie about *what* runs on a port, never about
  *where*.
- **Spam and sybils.** Announcing is permissionless, so anyone can
  claim to be a relay. Defences are `trust.providers: known` (only
  npubs from hosts, peers or groups), group scopes, and optionally
  NIP-13 proof of work on records. Ranking by tree distance is a
  convenience, *not* a defence: it favours whoever sits closest to the
  victim. A reputation or web-of-trust layer is out of scope.
- **Suppression and eclipse.** A transit node can drop responses, and
  a sybil near the origin can answer first with many identities and
  fill the `max_responses` counters downstream, so that honest answers
  are dropped. The same sybil next to an honest transit node could
  fill that node's locator cache and, if a full cache stopped
  forwarding, end the search for everyone behind it. The
  per-downstream-peer share of the counter *and of the cache* limits
  what one branch can crowd out, a full cache still forwards one
  query in four, the origin prefers answers that
  arrived over different first-hop peers when it has a choice, and a
  client that needs more than "some provider" sets
  `trust.providers: known`. Against a hostile node on the only path,
  discovery has no more defence than routing has.
- **Filter poisoning.** Inserting many service keys harms the service
  filter only, and is bounded by its inbound FPR cap. The routing
  filter is a separate object.
- **Amplification.** A query triggers no signatures. One query can
  still fan out over the tree; the clamps, the forward limiter per
  (key, inbound peer), `max_responses`, the per-peer buckets and the
  transit cache bound it. Responses go back by reverse path, and the
  query names no origin that could be forged.
- **Interest privacy.** On-path nodes see the key, not the origin. A
  direct neighbour can usually tell that a query started next door.
  For public keys that reveals what that neighbour is looking for;
  blinded keys hide the *what*.
- **Stale and replayed records.** Mandatory expiry, replaceable
  semantics on `created_at`, and the verified/unverified distinction
  in the cache. A replayed locator is at most two minutes old and
  leads to a session that either works or fails.
- **Cache pressure.** Every verified locator primes the identity and
  coordinate caches with a pubkey the requester did not choose.
  Discovery entries are therefore evicted before entries that routing
  or configuration created.
- **Deniability of restricted scopes.** Neither the record nor the
  sealed locator carries a signature, so a member cannot prove to an
  outsider that a node serves a group.
- **The DNS view.** Answers are unsigned and carry no proof. They are
  safe when the answering node is the client's own daemon, which
  verified the records; a client that borrows another node's resolver
  trusts that node completely. For service names the larger risk is
  not packet forgery but provider selection: announcing is
  permissionless, so a hostile node nearby can legitimately *be* the
  answer for `<type>.svc.fips`. See
  [Trust in the DNS view](#trust-in-the-dns-view).
- **Exposure by announcement.** Announcing a port tells the mesh where
  to knock. The default-deny `fips0` firewall still decides who gets
  in; an announcement is not an access grant.

## Alternatives considered

- **Flood or gossip every announcement to every node.** Simple and
  gives instant local answers, but state and traffic grow with the
  number of services in the whole mesh, and group scoping becomes
  encryption-only.
- **A DHT keyed by service type.** FIPS routes to exact addresses
  present in bloom filters; it has no "closest key" routing, and
  adding one is a second routing system.
- **Anchor a directory at the tree root.** Easy to find, but it
  centralizes load and trust on whichever node has the smallest
  `NodeAddr`, and the root changes.
- **Multicast, mDNS-style.** The mesh has no multicast; `src/mdns/`
  works on a LAN segment below FIPS, not across it.
- **Add a service list to the kind-37195 overlay advert.** It already
  exists and is signed, but it lives on public relays on the legacy
  internet, which this feature must not depend on.
- **Service keys in the routing bloom filter.** The first draft of
  this proposal. It needs no new announce message and old nodes carry
  the keys without knowing. That second property is the problem: old
  nodes *propagate* a key but *drop* the query, and because
  `plan_forward` falls back to non-tree peers only when no tree peer
  matches, a query dies at the first old tree peer while the pre-check
  keeps promising providers. The capacity argument against sharing
  (1 KB, k = 5, practical up to about 2,000 entries, 20 % inbound cap)
  fades if routing filters grow as planned, but larger filters need a
  protocol version of their own — v1 nodes must reject any other
  `size_class` — so transparency through old nodes is lost either way,
  a v1 `FilterAnnounce` payload is already 1,035 bytes of a link
  budget of about 1,243 on a 1,280-byte transport (roughly 1,071 of
  1,280 on the wire), and constrained nodes that fold filters down
  would
  carry the pollution at the worst FPR.
- **A per-request proof, as in `LookupResponse`.** Signing
  `request_id ‖ key ‖ coords` makes every answer fresh, but it cannot
  be cached, costs a provider one signature per query per requester,
  and for a blinded key it is a transferable proof of group service. A
  lookup needs the freshness because the answer *is* the result; a
  locator is followed by a session that proves more than the signature
  did.
- **Locating `allow` services with the plain key.** The first draft.
  It reveals the provider and the type to the whole mesh and pollutes
  public results with providers that refuse the fetch.

## Phasing

| Phase | Content | Verified by |
| ----- | ------- | ----------- |
| 1 | Public scope: service filter, `ServiceQuery`/`ServiceResponse` with locators, transit cache, fetch port, `node.services.announce`, `fipsctl services find`, `fipsctl show services` | Sans-IO tests beside the new `src/proto/` subsystem, node tests in `src/node/tests/`, a `testing/service-discovery/` Docker suite asserting on `fipsctl` JSON (pattern: `testing/acl-allowlist/test.sh`), including a mixed old/new topology |
| 2 | npub-list groups (fetch-only) and static shared-secret groups (blinded keys, sealed responses) | Same suite, with per-node group files mounted |
| 2b | Managed groups: `services_group_update` on the control socket. The Marmot companion is specified in [fips-service-discovery-marmot.md](fips-service-discovery-marmot.md) | Suite that pushes secrets and rosters from a test driver; add and remove a member and assert who can still discover |
| 3 | DNS view (DNS-SD PTR/SRV/TXT, plain SRV, `svc` AAAA) | Extend `testing/dns-resolver/` |
| 4 | Directory relays, `publish_to_directories` | Suite with an in-mesh relay container |
| 5 | Local port mapping for unmodified apps, announcing LAN services behind `fips-gateway` ([fips-gateway.md](fips-gateway.md)) | — |

A chaos scenario (`testing/chaos/scenarios/`) with many providers of
one type and many concurrent clients should accompany phase 1 and be
read *before* phase 2 is designed in detail: it measures the search
bound, the cache hit rate and how long a new provider stays invisible.

## Open questions

- **Event kind.** 37196 is a placeholder. Is a FIPS-specific kind
  right, or should this be proposed as a NIP so other overlays can
  share it?
- **Inline public records.** Put a compact record in
  `ServiceResponse` when it fits, saving a session per provider? With
  cacheable locators this would make transit nodes small directories.
- **Ring schedule for rare keys.** A single far-away provider is found
  only after every smaller ring has timed out, although the filter
  already guides such a query down one branch. Should the origin skip
  rings when few peers match, or should ring timeouts scale with
  `ttl`?
- **Service filter size and mixed size classes.** Size class 0 is a
  guess. If routing filters move to mixed sizes with folding, does the
  service filter follow or stay fixed?
- **Cache stickiness.** A full transit cache forwards one query in
  four and gives no downstream peer more than half of its slots. Both
  numbers are guesses. They trade search traffic against how long a
  new provider stays invisible, how much load the nearest providers
  carry, and how much of a cache a well-placed sybil can hold.
- **Clocks.** Locators and sealed queries assume clocks within 30
  seconds. Nostr events already assume roughly that; is it acceptable
  for small devices without a battery-backed clock?
- **Old nodes that split the tree.** Should upgraded nodes tunnel
  service filters and queries to each other across an old tree peer
  (as FSP datagrams), or is "upgrade the path" the answer?
- **Retained-epoch window.** How many previous epoch secrets a
  provider accepts. It trades tolerance for lagging members against
  how long a removed member is still answered.
- **Persisting pushed group state.** Should the daemon keep the
  secrets and roster of a managed group across a restart? Writing them
  under `/var/lib/fips/` keeps the group working when the managing
  process is down, but puts epoch secrets on disk. Memory-only avoids
  that and requires the process after every restart.
- **Static secret file format**: encoding, a bech32 form for sharing.
- **Service type registry**: who maintains it and where.
- **Small-MTU transports** (BLE, serial): is a 1024-byte record cap
  low enough, or does fetch need chunking?

## See also

- [fips-service-discovery-marmot.md](fips-service-discovery-marmot.md)
  — Marmot/MLS as the key agreement behind managed groups; a separate
  proposal on top of this one.
- [fips-mesh-operation.md](fips-mesh-operation.md) — the lookup
  protocol this design borrows its forwarding and deduplication from.
- [fips-bloom-filters.md](fips-bloom-filters.md) — filter format,
  propagation rules, FPR analysis, size classes and the inbound
  antipoison cap.
- [fips-session-layer.md](fips-session-layer.md) — FSP port-based
  service dispatch, where the fetch port lives.
- [fips-ipv6-adapter.md](fips-ipv6-adapter.md) — the `.fips` DNS
  responder and the identity cache it primes.
- [fips-nostr-discovery.md](fips-nostr-discovery.md) — *peer*
  discovery over public relays; a different problem, and one that does
  use the legacy internet.
- [fips-security.md](fips-security.md) — the `fips0` default-deny
  baseline that still governs access to an announced service.
- [../tutorials/host-a-service.md](../tutorials/host-a-service.md) —
  hosting a service today, without discovery.
- [../reference/wire-formats.md](../reference/wire-formats.md) — the
  message catalogs the new types would extend.
