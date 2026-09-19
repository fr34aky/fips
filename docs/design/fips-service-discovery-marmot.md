# FIPS Service Discovery: Marmot-Managed Groups

> **Status: proposal, on top of another proposal.** Nothing in this
> document is implemented. It depends on
> [fips-service-discovery.md](fips-service-discovery.md) and only on
> one seam of it: the *managed group*, whose secrets and roster are
> pushed to the daemon over the control socket
> (`services_group_update`). Exporter labels, component names and the
> delegation format are tentative, and several of them need decisions
> upstream in Marmot.

A shared-secret group in service discovery needs two 32-byte values —
a stable *locate secret* and a rotating *epoch secret* — and
optionally a roster of npubs. A static group reads them from a file
and can never remove a member without replacing the file everywhere.
This document proposes [Marmot](https://github.com/marmot-protocol/marmot)
as the source of those values for groups that need invitations,
removal and key rotation.

It is a separate proposal because it is separable: the daemon, the
wire protocol and the forwarding path are the same for a static and
a managed group, and nothing here has to be decided before service
discovery phases 1 and 2 can be built.

## Role

Marmot is an end-to-end encrypted group protocol that uses Nostr
pubkeys as identity and MLS (RFC 9420) for continuous group key
agreement. Its identity is a secp256k1 key, the same kind of key that
is a FIPS node's identity, which makes it a natural fit. What it
provides that the static mode lacks:

| Need | Static file | Marmot-managed |
| ---- | ----------- | -------------- |
| Invite a member | Copy a file out of band | Admin commits an MLS Add against the invitee's published KeyPackage (kind `30443`); the Welcome (kind `444` rumor) arrives NIP-59 gift-wrapped (kind `1059`) |
| Remove a member | Replace the file on every node | Admin commits a Remove, or the member sends SelfRemove; the group moves to a new epoch whose secrets the removed member cannot derive |
| Key rotation | Manual | Every commit starts a new epoch; members also self-update (Marmot: SHOULD, soon after joining) for forward secrecy and post-compromise security |
| Who may change membership | Whoever has the file | Only admins listed in the group's admin-policy component |
| Authenticated member list | None | The MLS roster: one Nostr pubkey per member, proven by an account identity proof |

## Under the covers

### Mapping onto discovery

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
  member can still recognize the group's blinded keys in service
  filters, but can no longer get a query answered.
- **The roster doubles as the npub list.** `secret` and `allow`
  collapse into one group: the fetch port checks the session peer
  against the roster. How an MLS member maps to a node npub is the
  subject of [Identity and keys](#identity-and-keys).
- **Lagging members** are handled as Marmot handles them — by trying
  the retained epochs — which is the window rule of the discovery
  protocol.

### Where MLS runs

Not in the `fips` daemon. A companion process (or any Marmot client)
holds the MLS state, talks to relays over `fips0`, and pushes three
values to the daemon with `services_group_update`:

| Pushed value | Source in the Marmot group | Used for |
| ------------ | -------------------------- | -------- |
| Locate secret | `discovery_id`, an application component in group state; stable across epochs | Blinding the service keys |
| Epoch secrets | MLS exporter of the current epoch and a few retained ones | Authenticating queries, sealing responses |
| Roster | Node npubs of the current MLS members | The npub list checked on the fetch port |

The forwarding path stays free of MLS, and a node that never uses
Marmot groups links nothing new. Every member runs the companion,
providers and clients alike.

### Identity and keys

The roster-as-allow-list and the address binding only hold if a group
member can be tied to a node npub. The first draft of the discovery
proposal said the companion "needs the node key or a signing command
on the control socket". A signing command is **not** enough: Welcomes
arrive NIP-59 gift-wrapped, and unwrapping is a NIP-44 ECDH with the
recipient's *private* key. A control socket that signs and does ECDH
for a client has handed over the node key in all but name — and the
node key is also the Noise static key that routing depends on.

Three options:

| Option | Roster check | Cost |
| ------ | ------------ | ---- |
| The companion holds the node key | MLS member = node npub, trivially | The most sensitive key on the machine lives in a second process that talks to relays |
| The control socket offers sign and NIP-44 operations | As above | A signing and decryption oracle for the node identity; equivalent to the first option for an attacker who reaches the socket |
| **A delegated account key (proposed)** | MLS member = account npub; a delegation statement maps it to one or more node npubs | One more signed object, and the roster becomes a derived list |

With delegation, the companion has its own Nostr key. The node key
signs one statement — "account *A* speaks for node *N* until *T*" —
which the daemon produces once over the control socket (a narrow
command that signs only this statement type). The companion sends it
to the group as an application message. Each member's companion
builds the pushed roster from the MLS members whose delegation
verifies, and drops a node when its delegation expires or its account
leaves the group. A user npub that delegates to several nodes falls
out of the same mechanism.

### Records as group messages

Members of a Marmot group have a better channel for records than
Locate and Fetch: the record is sent as a Marmot application message
through the in-mesh relays. It has two layers. The inner event is what
members read. It follows Marmot's application payload shape: the
fields of a Nostr event with `id` but without `sig`, which Marmot
forbids on inner events. MLS authenticates the sender, and a receiver
checks that `pubkey` is a node npub that the sender's credential may
speak for (the node itself, or a verified delegation), so the record
stays bound to the node's address.

```json
{
  "id": "<sha256 of the NIP-01 serialization>",
  "pubkey": "<node pubkey>",
  "created_at": 1790000000,
  "kind": 37196,
  "tags": [
    ["d", "http:tcp:8080"],
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

The inner record uses `valid_until` rather than `expiration` — the
only place where a service record does — because Marmot treats
retention as group state, not a sender preference: a sender-supplied
`expiration` tag is replaced or removed according to the group's
message-retention component, and the outer kind `445` carries an
`expiration` tag only when that component enables retention. A group
used for discovery SHOULD enable retention of about the record
lifetime (one hour), so that relays drop stale announcements. The
companion translates `valid_until` to `expiration` when it hands the
record to the daemon's discovery cache.

Two consequences. Members learn the group's services from group
messages alone, so Locate becomes optional for them and mainly adds
nearest-first ordering and a liveness check. And because MLS
application messages are forward-secret, a new member cannot read
announcements sent before it joined — which is harmless, since records
expire within the hour and are re-announced anyway.

One difference from static groups: these records are **not deniable
among members**. MLS signs application messages with the sender's leaf
key, so a member holds a transcript that attributes the record.
Towards the relay the record stays invisible: the outer event is
signed by a fresh ephemeral key, tagged only with the group's `h`
routing id, and encrypted under the epoch's group-event key. A relay
learns that *someone* posted to *some* group.

### Removing a member

1. The Remove commit moves the group to a new epoch, say 42 → 43.
2. Each remaining member's companion pushes the epoch-43 secret and
   the shorter roster to its daemon.
3. The removed member cannot derive the epoch-43 secret. Its queries
   are still answered while epoch 42 is inside the retained window,
   and not afterwards. The fetch port refuses it immediately, because
   it is no longer on the roster.
4. Optionally, an admin rotates `discovery_id` in a *later* commit, so
   that the removed member can no longer recognize the group's keys in
   service filters.

### What it costs

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
- **A second process and a second key** on every member, with the
  delegation that ties them to the node.

## Phasing

Service discovery phase 2b delivers `services_group_update` and tests
it with a driver that pushes secrets directly. This proposal starts
after that:

| Phase | Content | Verified by |
| ----- | ------- | ----------- |
| M1 | Companion process: joins a Marmot group over in-mesh relays, exports the epoch secret, pushes secrets and roster | Suite with an in-mesh relay container; add and remove a member and assert who can still discover |
| M2 | Delegation statements, roster derived from them | Same suite; expire a delegation and assert the node drops off the roster |
| M3 | Records as group messages | Same suite; a member with Locate disabled still learns the group's services |

## Open questions

- **Marmot registration.** The exporter label/context and the
  `discovery_id` application component both need entries in Marmot's
  registries. Is a FIPS-specific component acceptable upstream, or
  should discovery derive everything from one exporter and accept that
  lagging members miss until they catch up?
- **Delegation format.** A Nostr event kind of its own, or a field of
  the group's application state? How long should a delegation live,
  and is NIP-26 close enough to reuse?
- **Marmot without relays.** Marmot's core is transport-agnostic (the
  spec already has a second, experimental QUIC binding). A FIPS-native
  binding that carries MLS bytes over FSP between members would remove
  the relay dependency for small groups.
- **Time-based rotation.** MLS epochs advance only on commits. Should
  the companion self-update on a timer to bound how long a blinded key
  and an epoch secret live?
- **Persisting pushed group state.** Shared with the discovery
  proposal: memory-only requires the companion after every daemon
  restart; persisting puts epoch secrets on disk.

## See also

- [fips-service-discovery.md](fips-service-discovery.md) — the
  discovery protocol, scopes and the managed-group interface this
  document plugs into.
- [Marmot protocol](https://github.com/marmot-protocol/marmot) — MLS
  group key agreement with Nostr identities; see its
  `foundation/mls-protocol.md`, `protocol-core/member-departure.md`,
  `app-components/nostr-routing-v1.md` and `transports/nostr.md` for
  the rules referenced here.
- [../reference/control-socket.md](../reference/control-socket.md) —
  where `services_group_update` would live.
