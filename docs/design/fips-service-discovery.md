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

- **Announcing.** A node publishes a signed *service record* for each
  service it wants found: type, port, protocol and a little metadata.
  The record is a Nostr event signed with the node's FIPS identity
  key. Each announcement has a scope: public, a list of npubs, a
  shared-secret group, or a combination.
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
   address, and records can be cached or relayed by untrusted parties
   without losing authenticity.
2. **Bloom filters already answer "which direction".** Every node
   gossips a filter of what is reachable through it
   ([fips-bloom-filters.md](fips-bloom-filters.md)). A service type
   hashed into the same filter reuses all of that propagation.
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
| Locate | Which nodes offer service key *K*, and where are they in the tree? | Bloom-guided `ServiceQuery` / `ServiceResponse` link messages |
| Fetch | What exactly does that node offer? | Signed Nostr events over an FSP session to a reserved port |
| Index | What is out there? (browse, search) | In-mesh Nostr relays acting as directories |

A typical public lookup:

```text
client                         transit                      provider
  |                               |                             |
  |  bloom pre-check: some peer's filter contains K             |
  |-- ServiceQuery(K, ttl=2) ---->|  (no match in range)        |
  |-- ServiceQuery(K, ttl=4) ---->|-- forwarded along tree ---->|
  |                               |                             |  sign proof
  |<-- ServiceResponse(pubkey, coords, proof) -- reverse path --|
  |  verify proof, cache coords, prime identity cache           |
  |                                                             |
  |== FSP session (Noise XK) to port 257 ======================>|
  |-- LIST nostr-relay ---------------------------------------->|
  |<-- RECORD (signed Nostr event) -----------------------------|
  |  verify event signature and that event.pubkey == session peer
  |
  `-> connect to [fd..]:7777
```

### Service records

A service record is a parameterized replaceable Nostr event, tentative
**kind 37196**, the sibling of the overlay advert kind 37195
(`src/nostr/types.rs`,
[../reference/nostr-events.md](../reference/nostr-events.md)). It is
signed with the node's identity key; there is no separate service key.

```json
{
  "kind": 37196,
  "pubkey": "<node pubkey>",
  "created_at": 1790000000,
  "tags": [
    ["d", "nostr-relay:7777"],
    ["s", "nostr-relay"],
    ["port", "7777", "tcp"],
    ["scheme", "ws"],
    ["name", "andre's relay"],
    ["expiration", "1790003600"]
  ],
  "content": ""
}
```

| Tag | Required | Meaning |
| --- | -------- | ------- |
| `d` | yes | Instance identifier, `<type>:<port>`. Makes the event replaceable per service instance. |
| `s` | yes | Service type. Single-letter so that Nostr relays index it and `{"#s": [...]}` filters work on directories. |
| `port` | yes | Port and transport protocol (`tcp` or `udp`) on the node's FIPS address. |
| `scheme` | no | URL scheme a client should use (`http`, `ws`, …). |
| `path` | no | URL path prefix. |
| `name` | no | Self-asserted display label. Not unique, not trusted. |
| `expiration` | signed records | NIP-40 expiry. Records are short-lived and refreshed. |
| `valid_until` | Marmot inner records | Same meaning as `expiration`, for records sent inside a Marmot group, where a sender's `expiration` tag does not survive. See [Records by scope](#records-by-scope). |
| `-` | restricted records | NIP-70 protected-event marker. A receiver must not republish or re-serve the record; NIP-70-aware relays refuse it from anyone but the author. |

`content` may hold service-specific JSON (for a relay, a subset of its
NIP-11 document). A record is capped at 1024 bytes so that it always
fits in one FSP datagram; FIPS does not fragment
([fips-mtu.md](fips-mtu.md)).

Rules a consumer applies:

- The event signature must verify, and the address to connect to is
  *derived from `pubkey`*. The record has no address field on purpose.
  An unsigned record is accepted only in the two forms described under
  [Records by scope](#records-by-scope), where the channel it arrived
  on authenticates `pubkey` instead.
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
| `allow` (npub list) | Signed event with a `["-"]` tag | Fetch port only, to session peers on the list |
| `secret`, static | Unsigned event with a `["-"]` tag | Fetch port only, after the group proof |
| `secret`, Marmot-managed | Unsigned Marmot inner event inside a kind `445` group message | In-mesh relays; also the fetch port |

An **npub-list** record is the public record plus the protected
marker. It is never published to a relay.

```json
{
  "kind": 37196,
  "pubkey": "<node pubkey>",
  "created_at": 1790000000,
  "tags": [
    ["d", "blossom:3000"],
    ["s", "blossom"],
    ["port", "3000", "tcp"],
    ["scheme", "http"],
    ["name", "family photos"],
    ["expiration", "1790003600"],
    ["-"]
  ],
  "content": "",
  "id": "…",
  "sig": "<signed by the node key>"
}
```

A **static secret group** record has the same fields and tags but no
`sig`. The Noise XK session on the fetch port already authenticates
the provider, and `pubkey` must equal the session peer. A signed copy
would be transferable: a member who leaks it could prove to outsiders
that the node offers the service. Without the signature the record is
deniable, and it cannot be cached or relayed by anyone else, which is
what a secret scope wants.

A **Marmot-managed group** record has two layers. The inner event is
what members read. It follows Marmot's application payload shape: the
fields of a Nostr event with `id` but without `sig`, which Marmot
forbids on inner events. MLS authenticates the sender, the sender's
credential is the node pubkey, and a receiver checks that `pubkey`
matches it, so the record stays bound to the node's address.

```json
{
  "id": "<sha256 of the NIP-01 serialization>",
  "pubkey": "<node pubkey>",
  "created_at": 1790000000,
  "kind": 37196,
  "tags": [
    ["d", "http:8080"],
    ["s", "http"],
    ["port", "8080", "tcp"],
    ["name", "lab dashboard"],
    ["valid_until", "1790003600"]
  ],
  "content": ""
}
```

The inner event travels in an MLS application message, published as
Marmot's kind `445`. This is all a relay sees:

```json
{
  "kind": 445,
  "pubkey": "<fresh ephemeral key, used once>",
  "created_at": 1790000003,
  "tags": [
    ["h", "<nostr_group_id, 64 hex characters>"]
  ],
  "content": "<base64(nonce || ChaCha20-Poly1305(group_event_key, MLS message))>",
  "id": "…",
  "sig": "<signed by the ephemeral key>"
}
```

The inner record uses `valid_until` rather than `expiration` because
Marmot treats retention as group state, not a sender preference: a
sender-supplied `expiration` tag is replaced or removed according to
the group's message-retention component, and the outer kind `445`
carries an `expiration` tag only when that component enables
retention. A group used for discovery SHOULD enable retention of about
the record lifetime (one hour), so that relays drop stale
announcements.

### Locate: finding providers

#### Service keys in the bloom filter

Each announcement contributes one 16-byte **service key** to the
node's own bloom filter entries:

```text
public key  = SHA-256("fips-svc-v1" || type)[..16]
```

The key goes in next to the node's own address and its leaf
dependents, in `BloomState::compute_outgoing_filter` and `base_filter`
(`src/proto/bloom/state.rs`), using the existing
`BloomFilter::insert_bytes` (`src/proto/bloom/core.rs`). From there it
propagates exactly like a node address: along tree edges, split
horizon, debounced, subject to the inbound FPR cap.

Consequences worth stating:

- **No new propagation protocol.** `FilterAnnounce` is unchanged.
- **Cost is per type, not per provider.** Every provider of
  `nostr-relay` inserts the same key, which sets the same bits. A
  thousand relays cost the mesh one filter entry.
- **Old nodes participate without knowing.** To a node that predates
  this feature a service key is just more bits to merge and forward.
- **Withdrawal works.** Filters are rebuilt from scratch on every
  recompute, so a removed announcement disappears with the next
  update. Nothing relies on deleting from a bloom filter.
- A service key is shaped like a `NodeAddr`, so the existing
  `RoutingView::peers_reaching` (`src/proto/lookup/core.rs`) answers
  "which peers may reach a provider" unchanged. A collision with a
  real node address has probability 2⁻¹²⁸ per pair.

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
| `origin` | 16 | Requester `NodeAddr` |
| `ttl` | 1 | Hop limit, decremented per hop |
| `max_responses` | 1 | Cap on responses relayed per request |
| `min_mtu` | 2 | As in `LookupRequest` |
| `origin_coords` | 2 + 16×n | Fallback for response routing |
| `auth` | 16 | Only with `A`; see [Scopes](#scopes-and-groups) |

`ServiceResponse = 0x33`:

| Field | Size | Notes |
| ----- | ---- | ----- |
| `version` | 1 | `0x01` |
| `flags` | 1 | bit 0 `E`: body is sealed |
| `request_id` | 8 | Echo |
| `key` | 16 | Echo |
| `path_mtu` | 2 | Transit annotation, outside the signature |
| `responder` | 32 | Responder x-only pubkey |
| `coords` | 2 + 16×n | Responder tree coordinates |
| `proof` | 64 | Schnorr over `request_id ‖ key ‖ coords` |

This is `LookupResponse` plus the responder's pubkey. A lookup already
knows whose signature to expect; a service query does not, so the
response has to say. The requester checks the proof against the
carried pubkey, derives the `NodeAddr` from it, caches the coordinates
as `Verified` (`src/cache/entry.rs`) and primes the identity cache the
same way a DNS resolution does today (`DnsResolvedIdentity` in
`src/upper/dns.rs`). After that the provider is routable.

With flag `E` everything after `path_mtu` is one AEAD ciphertext; see
[Scopes](#scopes-and-groups).

#### Forwarding

Forwarding reuses the lookup machinery
([fips-mesh-operation.md](fips-mesh-operation.md), "Bloom-Guided Tree
Routing"): `plan_forward` sends the query to tree peers whose filter
contains the key, falling back to non-tree matches; responses return
by reverse path through the `recent_requests` table
(`src/proto/lookup/state.rs`) with `origin_coords` as the fallback;
the per-peer eviction accounting, the transit forward limiter and the
per-peer signing budget (`LookupSignRateLimiter`,
`src/node/rate_limit.rs`) apply as they do to lookups.

A node that holds a matching announcement answers **and** keeps
forwarding, because other providers may lie further on.

#### The flooding problem

This is where a service query differs from a lookup, and it is the
main cost of the design. A `NodeAddr` lives in exactly one place, so a
lookup follows one branch. A popular service key is present in almost
every filter on almost every tree edge, so a naive query reaches the
whole tree.

The proposal bounds this with:

- **Expanding-ring search.** The origin tries `ttl` 2, 4, 8, 16, then
  the full lookup TTL, with a fresh `request_id` each time, and stops
  as soon as it has enough verified answers. Inner rings are
  re-visited, but the cost is geometric and the common case — a
  provider nearby — ends after the first or second ring. This also
  gives **locality for free**: nearest providers answer first.
- **`max_responses`.** A transit node relays at most that many
  distinct responses per `request_id`, replacing the lookup's single
  `response_forwarded` flag with a small counter and a set of
  responder digests.
- **Bloom pre-check.** If no peer's filter contains the key the origin
  reports "no providers" without sending anything, as lookups do.
- **Origin caching.** Positive results are cached for
  `cache_ttl_secs`; negative results for a shorter time.
- **Rate limits.** Per-key transit forward interval, per-peer query
  token bucket, per-peer signing budget on the provider.
- **Later: transit answers.** Because records are self-signed, a
  transit node that has recently seen a verified response could answer
  from cache and stop forwarding. This is left out of the first
  version; see [Open questions](#open-questions).

False positives send a query into a subtree with no provider. The TTL
bounds the damage, and the rate is whatever the filter FPR already is
for routing.

#### Partial deployment

`dispatch_link_message` (`src/node/dataplane/dispatch.rs`) logs and
drops unknown link types. A query therefore stops at a node that does
not implement it, and service keys still flow through that node's
filter. Discovery degrades to "providers reachable through upgraded
nodes". No existing message changes, so the feature is additive
rather than wire-format-breaking in the sense of
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

| Scope | Locate | Fetch | Hidden from outsiders |
| ----- | ------ | ----- | --------------------- |
| `public` | Plain key, anyone is answered | Anyone | Nothing |
| `allow: <group>` | Plain key, anyone is answered | Only session peers whose npub is in the group | Record contents (port, name, metadata) |
| `secret: <group>` | Blinded key, only authenticated queries are answered, response sealed | Requester must prove knowledge of the group secret | That the service exists, its type, and who provides it |
| both | As `secret` | Both checks | As `secret`, plus per-member revocation |

**npub-list groups** copy the peer ACL idiom: a file per group under
`/etc/fips/groups/` (platform path as for `peers.allow`), one npub or
host alias per line, hot-reloaded once per tick. Membership is checked
against the Noise-authenticated session peer. Removing a member is
deleting a line.

**Shared-secret groups** supply two 32-byte values: a *locate secret*
that stays stable, and an *epoch secret* that may rotate. Keys are
derived with HKDF-SHA256 (already a dependency): `k_key` from the
locate secret, `k_auth` and `k_seal` from the epoch secret. Where the
two values come from is a provisioning choice:

- **Static.** One file, mode 0600, distributed out of band. Both
  values derive from it and never change until the operator replaces
  the file on every member. Simple, no dependencies, no revocation.
- **Marmot-managed.** The group is a
  [Marmot](https://github.com/marmot-protocol/marmot) group, and MLS
  supplies an epoch secret that changes whenever membership changes.
  See [Marmot-managed groups](#marmot-managed-groups).

The discovery wire protocol is identical in both cases:

- The bloom key is blinded:
  `HMAC(k_key, type)[..16]`. Outsiders see an opaque key in filters
  and queries and cannot tell what it names or build it themselves.
- The query carries `auth = HMAC(k_auth, request_id ‖ key ‖
  origin)[..16]`. A provider answers only if it verifies. Without
  this, any transit node could replay an observed key and learn who
  answers.
- The response body (`responder`, `coords`, `proof`) is sealed with an
  AEAD key derived from `k_seal` and `request_id`. Transit nodes need
  only `request_id` for reverse-path routing, and deduplicate sealed
  responses by body hash.
- On the fetch port the requester adds `HMAC(k_auth, "fetch" ‖
  client pubkey ‖ provider pubkey ‖ req_id)`, binding the proof to
  this session.
- A provider checks `auth` against the current epoch secret and a
  small window of previous ones, so members that lag an epoch are
  still served.

Limits that the document should not hide:

- An on-path node still sees that origin *O* sent a query and shortly
  afterwards opened a session to node *P*. Session traffic is opaque,
  but the correlation exists. This is the same class of metadata
  exposure described under "Privacy Considerations" in
  [fips-mesh-operation.md](fips-mesh-operation.md).
- A blinded key is stable, so an observer can track "the same unknown
  thing" over time. A Marmot-managed group can rotate it; a static
  group cannot without touching every member.
- Removing a member from a *static* group means replacing the file
  everywhere. Combine it with an npub list, or use a Marmot-managed
  group, when per-member revocation matters.

#### Marmot-managed groups

[Marmot](https://github.com/marmot-protocol/marmot) is an end-to-end
encrypted group protocol that uses Nostr pubkeys as identity and MLS
(RFC 9420) for continuous group key agreement. Its identity is the
same secp256k1 key that is a FIPS node's identity, which makes it a
natural fit. What it provides that the static mode lacks:

| Need | Static file | Marmot-managed |
| ---- | ----------- | -------------- |
| Invite a member | Copy a file out of band | Admin commits an MLS Add against the invitee's published KeyPackage (kind `30443`); the Welcome (kind `444` rumor) arrives NIP-59 gift-wrapped (kind `1059`) |
| Remove a member | Replace the file on every node | Admin commits a Remove, or the member sends SelfRemove; the group moves to a new epoch whose secrets the removed member cannot derive |
| Key rotation | Manual | Every commit starts a new epoch; members also self-update (Marmot: SHOULD, soon after joining) for forward secrecy and post-compromise security |
| Who may change membership | Whoever has the file | Only admins listed in the group's admin-policy component |
| Authenticated member list | None | The MLS roster: one Nostr pubkey per member, proven by an account identity proof |

Mapping onto discovery:

- **Epoch secret** = an MLS exporter,
  `MLS-Exporter("marmot", "fips-service-discovery", 32)` (label and
  context tentative). Marmot requires every exporter use to have its
  own registered label/context pair and forbids reusing the
  `"group-event"` key for anything else, so this needs an entry in
  Marmot's exporter registry.
- **Locate secret** = a random 32-byte `discovery_id` kept in group
  state as an application component. This copies Marmot's own split:
  its relay routing handle `nostr_group_id` is random, explicitly
  *not* derived from any key or epoch, and stays stable so that
  members who lag an epoch still find the group, while the encryption
  key underneath changes per epoch. `nostr_group_id` itself cannot be
  reused here because it appears in clear in `h` tags on relays.
- **Rotating the locate secret** follows Marmot's routing-rotation
  rule: first commit the removal, then rotate in a *later* commit. A
  rotation carried in the removal commit itself is readable by the
  member being removed. Rotation is optional; without it a removed
  member can still recognize the group's blinded keys in bloom
  filters, but can no longer get a query answered.
- **The roster doubles as the npub list.** `secret` and `allow`
  collapse into one group: the fetch port checks the session peer
  against the MLS roster.
- **Lagging members** are handled as Marmot handles them — by trying
  the retained epochs — which is the window rule above.

Where MLS runs: not in the `fips` daemon. The proposal is a companion
process (or any Marmot client) that holds the MLS state, talks to
relays over `fips0`, and pushes `discovery_id`, the current and
retained epoch secrets and the roster to the daemon over the control
socket. The forwarding path stays free of MLS, and a node that never
uses Marmot groups links nothing new. The pushed values are listed
under [Group files and provisioning](#group-files-and-provisioning).

What happens when an admin removes a member:

1. The Remove commit moves the group to a new epoch, say 42 → 43.
2. Each remaining member's companion pushes the epoch-43 secret and
   the shorter roster to its daemon.
3. The removed member cannot derive the epoch-43 secret. Its queries
   are still answered while epoch 42 is inside the retained window,
   and not afterwards. The fetch port refuses it immediately, because
   it is no longer on the roster.
4. Optionally, an admin rotates `discovery_id` in a *later* commit, so
   that the removed member can no longer recognize the group's keys in
   bloom filters.

What it costs:

- **It needs a delivery service inside the mesh.** Commits, Welcomes
  and KeyPackages travel over Nostr relays, and those must be in-mesh
  relays to honour the no-legacy-internet rule. Marmot's relay URL
  profile allows `ws://`, so `ws://<npub>.fips:7777` is valid signed
  group state. The relays are found with a *public* Locate for
  `nostr-relay`, so the layering is: public discovery finds relays,
  relays carry the group, the group keys secret discovery. If no relay
  is reachable the group cannot change, but discovery keeps working on
  the last known epoch.
- **Epochs only advance on commits.** There is no time-based rotation;
  a quiet group keeps its keys until someone self-updates.
- **The node key signs for the group member.** The roster-as-allow-list
  and the address binding only hold if the Marmot account *is* the
  node npub, so the companion needs the node key or a signing command
  on the control socket.

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
Locate or a lookup confirms the provider is reachable.

Secret-group records are never published to a directory as plain
events, because the author pubkey alone would reveal that a node
belongs to *some* group. Records of npub-list and static secret groups
therefore stay off relays entirely. A Marmot-managed group has a
better channel: the record is sent as a Marmot application message
(both layers are shown under [Records by scope](#records-by-scope)).
The outer relay event is signed by a fresh ephemeral key, tagged only
with the group's `h` routing id, and encrypted under the epoch's
group-event key. A relay learns that *someone* posted to *some* group.

Two consequences. Members of a Marmot-managed group learn the group's
services from group messages alone, so Locate becomes optional for
them and mainly adds nearest-first ordering and a liveness check. And
because MLS application messages are forward-secret, a new member
cannot read announcements sent before it joined — which is harmless,
since records expire within the hour and are re-announced anyway.

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
      guild:      { marmot: true }    # secrets and roster pushed by the
                                      # Marmot companion at runtime
    query:
      ttl_steps: [2, 4, 8, 16, 64]
      max_responses: 8
      cache_ttl_secs: 300
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
daemon signs on the application's behalf, since the record must be
signed by the node key.

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
- Only providers need the file. A client needs nothing, because the
  provider decides.
- Each provider keeps its own copy. Removing a member is deleting the
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

**Marmot-managed group.** There is no file to edit; the configuration
entry is only `guild: { marmot: true }`. The companion process pushes
three things over the control socket (`services_group_update`), and
pushes them again whenever the group changes:

| Pushed value | Source in the Marmot group | Used for |
| ------------ | -------------------------- | -------- |
| `discovery_id` | Application component in group state; stable across epochs | Blinding the bloom keys |
| Epoch secrets | MLS exporter of the current epoch and a few retained ones | Authenticating queries, sealing responses |
| Roster | Pubkeys of the current MLS members | The npub list checked on the fetch port |

Every member runs the companion, providers and clients alike.

`fipsctl show services --groups` lists what the daemon currently
holds, without ever printing a secret:

```text
$ fipsctl show services --groups
GROUP   KIND     EPOCH  MEMBERS  SOURCE
family  list     —      3        /etc/fips/groups/family
lab     static   —      —        /etc/fips/groups/lab.key
guild   marmot   42     7        companion (updated 3m ago)
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
probe` (`src/control/probe.rs`, `src/control/commands.rs`). `fipsctl
show services` lists local announcements and the discovery cache from
the read-only snapshot path. The same commands over the control socket
are the API for local applications
([../reference/control-socket.md](../reference/control-socket.md)).

### DNS view

The `.fips` responder (`src/upper/dns.rs`) answers only AAAA today.
Service discovery gives it a second data source:

| Query | Type | Answer |
| ----- | ---- | ------ |
| `_nostr-relay._tcp.fips` | SRV | One record per provider: port, target `<npub>.fips`. Priority follows tree distance. |
| `_nostr-relay._tcp.fips` | TXT | `scheme=`, `path=`, `name=` of the first provider |
| `_blossom._tcp.family.fips` | SRV | Same, restricted to group `family` |
| `nostr-relay.svc.fips` | AAAA | Address of the nearest verified provider, sticky for the DNS TTL |
| `_services._dns-sd._udp.fips` | PTR | Known service types (DNS-SD browse) |

An npub is 63 characters, exactly the DNS label limit, so `<npub>.fips`
works as an SRV target and resolves through the existing path, which
also primes the identity cache.

Points the implementation has to handle:

- **Latency.** The responder answers from the discovery cache. On a
  miss it starts a query and waits a bounded time (1–2 s) before
  answering empty; the next lookup hits the cache.
- **Size.** The responder uses a 512-byte buffer and no EDNS0. An SRV
  answer is roughly 90 bytes, so about four providers fit. Either cap
  the answer count or add EDNS0.
- **AAAA cannot carry a port.** `<type>.svc.fips` helps applications
  that cannot do SRV only when the port is conventional. `svc` becomes
  a reserved label in `validate_hostname`.
- **Locality of the DNS view.** The responder keeps dropping queries
  that arrive on `fips0` (`is_mesh_interface_query`). DNS is a view of
  *this node's* discovery results, not a mesh-wide directory service.

## Rate limits and safeguards

| Guard | Where | Proposed default |
| ----- | ----- | ---------------- |
| Announcements per node | provider | 16 |
| Record size | provider, consumer | 1024 bytes |
| Record lifetime | provider | 1 h, refreshed at half-life |
| Expanding ring | origin | `[2, 4, 8, 16, 64]` |
| Responses relayed per request | transit | 8 |
| Forward interval per key | transit | 2 s (as lookups) |
| Queries per link peer | transit | token bucket |
| Proof signatures per link peer | provider | shared with `LookupSignRateLimiter` |
| Fetch requests per session | provider | token bucket |
| Discovery cache | origin | 2048 records, expired-first eviction |
| DNS wait on cache miss | responder | 1.5 s |

## Security and threat model

- **Impersonation.** Not possible at the record level: the address is
  derived from the signing key. A record can lie about *what* runs on
  a port, never about *where*.
- **Spam and sybils.** Announcing is permissionless, so anyone can
  claim to be a relay. Defences are ranking by tree distance,
  `trust.providers: known` (only npubs from hosts, peers or groups),
  group scopes, and optionally NIP-13 proof of work on records.
  A reputation or web-of-trust layer is out of scope.
- **Bloom poisoning.** Inserting many service keys is no stronger than
  inserting many fake addresses, which is already possible and already
  bounded by `node.bloom.max_inbound_fpr` (`src/node/bloom.rs`). The
  per-node announcement cap keeps honest nodes small.
- **Amplification.** One query can trigger many signed responses. The
  ring search, `max_responses`, per-peer buckets and the provider-side
  signing budget bound it; responses go back by reverse path, so a
  forged `origin` does not redirect them.
- **Interest privacy.** On-path nodes see `origin` and the key. For
  public keys that reveals what the origin is looking for. Blinded
  keys hide the *what* but not the *who*.
- **Stale and replayed records.** Mandatory expiry, replaceable
  semantics on `created_at`, and the verified/unverified distinction
  in the cache.
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

## Phasing

| Phase | Content | Verified by |
| ----- | ------- | ----------- |
| 1 | Public scope: bloom keys, `ServiceQuery`/`ServiceResponse`, fetch port, `node.services.announce`, `fipsctl services find`, `fipsctl show services` | Sans-IO tests beside the new `src/proto/` subsystem, node tests in `src/node/tests/`, a `testing/service-discovery/` Docker suite asserting on `fipsctl` JSON (pattern: `testing/acl-allowlist/test.sh`) |
| 2 | npub-list groups and static shared-secret groups | Same suite, with per-node group files mounted |
| 2b | Marmot-managed groups: control-socket commands to push secrets and roster, companion process, records as group messages | Suite with an in-mesh relay container; add and remove a member and assert who can still discover |
| 3 | DNS view (SRV, TXT, `svc` AAAA, DNS-SD browse) | Extend `testing/dns-resolver/` |
| 4 | Directory relays, `publish_to_directories` | Suite with an in-mesh relay container |
| 5 | Local port mapping for unmodified apps, announcing LAN services behind `fips-gateway` ([fips-gateway.md](fips-gateway.md)) | — |

A chaos scenario (`testing/chaos/scenarios/`) with many providers of
one type should accompany phase 1 to measure the flooding bound in
practice.

## Open questions

- **Event kind.** 37196 is a placeholder. Is a FIPS-specific kind
  right, or should this be proposed as a NIP so other overlays can
  share it?
- **Inline public records.** Put a compact record in
  `ServiceResponse` when it fits, saving a session per provider?
- **Transit caching.** Let transit nodes answer from cache? It cuts
  flooding sharply but lets a node suppress competitors' answers.
- **Own filter or shared filter.** Service keys share the routing
  filter here. A second, smaller filter would keep routing FPR
  untouched at the price of another announce message.
- **Marmot registration.** The exporter label/context and the
  `discovery_id` application component both need entries in Marmot's
  registries. Is a FIPS-specific component acceptable upstream, or
  should discovery derive everything from one exporter and accept that
  lagging members miss until they catch up?
- **Marmot identity.** Must the group member be the node npub, or can
  a user npub join and delegate to one or more nodes? Delegation
  breaks the simple roster-equals-allow-list rule.
- **Marmot without relays.** Marmot's core is transport-agnostic (the
  spec already has a second, experimental QUIC binding). A FIPS-native
  binding that carries MLS bytes over FSP between members would remove
  the relay dependency for small groups.
- **Time-based rotation.** MLS epochs advance only on commits. Should
  the companion self-update on a timer to bound how long a blinded key
  and an epoch secret live?
- **Persisting pushed group state.** Should the daemon keep the
  secrets and roster of a Marmot-managed group across a restart?
  Writing them under `/var/lib/fips/` keeps the group working when the
  companion is down, but puts epoch secrets on disk. Memory-only
  avoids that and requires the companion after every restart.
- **Retained-epoch window.** How many previous epoch secrets a
  provider accepts. It trades tolerance for lagging members against
  how long a removed member is still answered.
- **Static secret file format**: encoding, a bech32 form for sharing.
- **Service type registry**: who maintains it and where.
- **Small-MTU transports** (BLE, serial): is a 1024-byte record cap
  low enough, or does fetch need chunking?

## See also

- [fips-mesh-operation.md](fips-mesh-operation.md) — the lookup
  protocol this design borrows its forwarding, deduplication and rate
  limiting from.
- [fips-bloom-filters.md](fips-bloom-filters.md) — filter capacity,
  FPR analysis and the inbound antipoison cap.
- [fips-session-layer.md](fips-session-layer.md) — FSP port-based
  service dispatch, where the fetch port lives.
- [fips-ipv6-adapter.md](fips-ipv6-adapter.md) — the `.fips` DNS
  responder and the identity cache it primes.
- [fips-nostr-discovery.md](fips-nostr-discovery.md) — *peer*
  discovery over public relays; a different problem, and one that does
  use the legacy internet.
- [fips-security.md](fips-security.md) — the `fips0` default-deny
  baseline that still governs access to an announced service.
- [Marmot protocol](https://github.com/marmot-protocol/marmot) — MLS
  group key agreement with Nostr identities; see its
  `foundation/mls-protocol.md`, `protocol-core/member-departure.md`,
  `app-components/nostr-routing-v1.md` and `transports/nostr.md` for
  the rules referenced under "Marmot-managed groups".
- [../tutorials/host-a-service.md](../tutorials/host-a-service.md) —
  hosting a service today, without discovery.
- [../reference/wire-formats.md](../reference/wire-formats.md) — the
  message catalogs the new types would extend.
