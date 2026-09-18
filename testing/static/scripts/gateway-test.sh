#!/bin/bash
# Gateway integration test: non-FIPS LAN client reaches mesh HTTP server.
#
# Topology:
#   gw-client (non-FIPS) → gw-gateway (fips + fips-gateway) → gw-server (fips + http)
#
# Usage:
#   ./scripts/gateway-test.sh [inject-config]
#
# Subcommands:
#   inject-config  — post-process generated configs to add gateway section
#   (no args)      — run the test (containers must be running)
set -e

trap 'echo ""; echo "Test interrupted"; exit 130' INT

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/../../lib/wait-converge.sh"

GENERATED_DIR="$SCRIPT_DIR/../generated-configs${FIPS_CI_NAME_SUFFIX:-}"
ENV_FILE="$GENERATED_DIR/npubs.env"

GATEWAY="fips-gw-gateway${FIPS_CI_NAME_SUFFIX:-}"
SERVER="fips-gw-server${FIPS_CI_NAME_SUFFIX:-}"
SERVER2="fips-gw-server-2${FIPS_CI_NAME_SUFFIX:-}"
CLIENT="fips-gw-client${FIPS_CI_NAME_SUFFIX:-}"
CLIENT2="fips-gw-client-2${FIPS_CI_NAME_SUFFIX:-}"

# LAN-side IPv6 addressing. run_gateway claims a per-run /64 and exports
# FIPS_GW_LAN6_PREFIX; unset (standalone / GitHub) these render the base
# compose's fd02:: addresses, byte-identical to before. GW_DNS is the gateway's
# LAN address (nameserver + route next-hop); GW_CLIENT_LAN is gw-client's LAN
# address (inbound port-forward target). fd01::/112 (the virtual pool) is NOT
# claimed and stays literal below.
GW_LAN6_PREFIX="${FIPS_GW_LAN6_PREFIX:-fd02}"
GW_DNS="${GW_LAN6_PREFIX}::10"
GW_CLIENT_LAN="${GW_LAN6_PREFIX}::20"

# ── inject-config subcommand ─────────────────────────────────────────────

inject_gateway_config() {
    local config_file="$GENERATED_DIR/gateway/node-a.yaml"

    if [ ! -f "$config_file" ]; then
        echo "Error: $config_file not found. Run generate-configs.sh gateway first." >&2
        exit 1
    fi

    echo "Injecting gateway config into $config_file"
    python3 -c "
import yaml

with open('$config_file') as f:
    cfg = yaml.safe_load(f)

cfg['gateway'] = {
    'enabled': True,
    'pool': 'fd01::/112',
    # Docker assigns gateway-lan to eth1 (fips-net is eth0). The
    # LAN-side masquerade for inbound port forwards gates on this.
    'lan_interface': 'eth1',
    'dns': {
        'listen': '[::]:53',
        'ttl': 5,
    },
    'pool_grace_period': 5,
    'port_forwards': [
        {
            'listen_port': 18080,
            'proto': 'tcp',
            'target': '[${GW_CLIENT_LAN}]:8080',
        },
        # 6B: second TCP forward — exercises multiple simultaneous TCP
        # rules sharing the same LAN backend on a different listen port.
        {
            'listen_port': 18082,
            'proto': 'tcp',
            'target': '[${GW_CLIENT_LAN}]:8081',
        },
        # 6A: UDP forward — exercises the runtime UDP DNAT path (rule
        # shape + conntrack handling) end-to-end.
        {
            'listen_port': 18081,
            'proto': 'udp',
            'target': '[${GW_CLIENT_LAN}]:8081',
        },
    ],
}

with open('$config_file', 'w') as f:
    yaml.dump(cfg, f, default_flow_style=False, sort_keys=False)
"
    echo "  ✓ Gateway config injected"
}

if [ "${1:-}" = "inject-config" ]; then
    inject_gateway_config
    exit 0
fi

# ── Main test ────────────────────────────────────────────────────────────

if [ ! -f "$ENV_FILE" ]; then
    echo "Error: $ENV_FILE not found. Run generate-configs.sh gateway first." >&2
    exit 1
fi

# shellcheck source=../generated-configs/npubs.env
source "$ENV_FILE"

PASSED=0
FAILED=0

check() {
    local label="$1"
    local result="$2"
    if [ "$result" -eq 0 ]; then
        echo "  $label ... OK"
        PASSED=$((PASSED + 1))
    else
        echo "  $label ... FAIL"
        FAILED=$((FAILED + 1))
    fi
}

echo "=== FIPS Gateway Integration Test ==="
echo ""

# Phase 1: Wait for mesh convergence (gateway ↔ server, gateway ↔ server-2)
echo "Phase 1: Mesh convergence"
wait_for_peers "$GATEWAY" 2 30 || true
wait_for_peers "$SERVER" 1 30 || true
wait_for_peers "$SERVER2" 1 30 || true

# Phase 2: Wait for gateway DNS to respond
echo ""
echo "Phase 2: Gateway DNS readiness"
DNS_READY=false
for i in $(seq 1 30); do
    # Try resolving the server's npub via the gateway DNS from the client.
    # Match fd01:: specifically (the pool prefix) to avoid false-positive
    # matches on error messages containing fd02::10.
    local_result=$(docker exec "$CLIENT" dig +short AAAA "${NPUB_B}.fips" @${GW_DNS} 2>/dev/null || true)
    if echo "$local_result" | grep -q "^fd01::"; then
        echo "  Gateway DNS responding after ${i}s"
        DNS_READY=true
        break
    fi
    sleep 1
done

if [ "$DNS_READY" != true ]; then
    echo "  WARNING: Gateway DNS did not respond within 30s, continuing anyway"
fi

# Phase 3: Client network setup — route virtual IP pool via gateway
echo ""
echo "Phase 3: Client network setup"
docker exec "$CLIENT" ip -6 route add fd01::/112 via ${GW_DNS} 2>/dev/null || true
echo "  Added route fd01::/112 via ${GW_DNS} on $CLIENT"
docker exec "$CLIENT2" ip -6 route add fd01::/112 via ${GW_DNS} 2>/dev/null || true
echo "  Added route fd01::/112 via ${GW_DNS} on $CLIENT2"

# Phase 4: DNS resolution test — resolve server npub from both clients,
# exercising concurrent multi-client mappings.
echo ""
echo "Phase 4: DNS resolution"
VIRTUAL_IP=$(docker exec "$CLIENT" dig +short AAAA "${NPUB_B}.fips" @${GW_DNS} 2>/dev/null | head -1)
if [ -n "$VIRTUAL_IP" ] && echo "$VIRTUAL_IP" | grep -q "fd01"; then
    check "Resolve ${NPUB_B:0:20}...fips on $CLIENT → $VIRTUAL_IP" 0
else
    check "Resolve ${NPUB_B:0:20}...fips on $CLIENT (got: '$VIRTUAL_IP')" 1
fi

VIRTUAL_IP_2=$(docker exec "$CLIENT2" dig +short AAAA "${NPUB_C}.fips" @${GW_DNS} 2>/dev/null | head -1)
if [ -n "$VIRTUAL_IP_2" ] && echo "$VIRTUAL_IP_2" | grep -q "fd01"; then
    check "Resolve ${NPUB_C:0:20}...fips on $CLIENT2 → $VIRTUAL_IP_2" 0
else
    check "Resolve ${NPUB_C:0:20}...fips on $CLIENT2 (got: '$VIRTUAL_IP_2')" 1
fi

# Both clients must receive distinct virtual-IP mappings — this is the
# core multi-client invariant: each LAN client gets its own pool entry.
if [ -n "$VIRTUAL_IP" ] && [ -n "$VIRTUAL_IP_2" ] && [ "$VIRTUAL_IP" != "$VIRTUAL_IP_2" ]; then
    check "Distinct virtual IPs per client ($VIRTUAL_IP vs $VIRTUAL_IP_2)" 0
else
    check "Distinct virtual IPs per client (got: '$VIRTUAL_IP' vs '$VIRTUAL_IP_2')" 1
fi

# Verify gateway show_mappings reports both client mappings. Mapping
# allocation happens in the DNS response path, but the gateway control
# socket serves a snapshot that is refreshed on a 10s tick (see
# src/bin/fips-gateway.rs tick interval). Poll up to 15s so at least
# one post-allocation snapshot tick is guaranteed to land.
ACTIVE_COUNT="error"
# Control socket protocol is line-delimited JSON ({"command": "..."});
# bare "show_mappings" returns an "invalid request" error response with
# no data field and the parse below counts that as 0 mappings.
for _ in $(seq 1 15); do
    GW_MAPPINGS=$(docker exec "$GATEWAY" bash -c \
        'echo "{\"command\":\"show_mappings\"}" | nc -U -w1 /run/fips/gateway.sock 2>/dev/null' || echo "")
    ACTIVE_COUNT=$(echo "$GW_MAPPINGS" \
        | python3 -c "import sys,json; r=json.load(sys.stdin); print(len(r.get('data',{}).get('mappings',[])))" 2>/dev/null || echo "error")
    if [ "$ACTIVE_COUNT" = "2" ]; then
        break
    fi
    sleep 1
done
if [ "$ACTIVE_COUNT" = "2" ]; then
    check "Gateway reports 2 active mappings (multi-client)" 0
else
    check "Gateway active mapping count (got: $ACTIVE_COUNT)" 1
fi

# Phase 5: End-to-end HTTP test from both clients in parallel
echo ""
echo "Phase 5: HTTP through gateway"

# Use --resolve to bind the .fips hostname to the virtual IP for curl.
# Run both client requests concurrently to exercise simultaneous flows
# through distinct NAT mappings.
RESP_FILE=$(mktemp)
RESP_FILE_2=$(mktemp)
trap 'rm -f "$RESP_FILE" "$RESP_FILE_2"' EXIT

if [ -n "$VIRTUAL_IP" ]; then
    docker exec "$CLIENT" curl -6 -s --max-time 10 \
        --resolve "${NPUB_B}.fips:8000:[$VIRTUAL_IP]" \
        "http://${NPUB_B}.fips:8000/" >"$RESP_FILE" 2>&1 &
    PID1=$!
else
    PID1=""
fi

if [ -n "$VIRTUAL_IP_2" ]; then
    docker exec "$CLIENT2" curl -6 -s --max-time 10 \
        --resolve "${NPUB_C}.fips:8000:[$VIRTUAL_IP_2]" \
        "http://${NPUB_C}.fips:8000/" >"$RESP_FILE_2" 2>&1 &
    PID2=$!
else
    PID2=""
fi

[ -n "$PID1" ] && wait "$PID1" || true
[ -n "$PID2" ] && wait "$PID2" || true

RESPONSE=$(cat "$RESP_FILE")
RESPONSE_2=$(cat "$RESP_FILE_2")

if [ -n "$VIRTUAL_IP" ]; then
    if echo "$RESPONSE" | grep -q "Fuck IPs"; then
        check "HTTP GET from $CLIENT" 0
    else
        check "HTTP GET from $CLIENT (response: '${RESPONSE:0:80}')" 1
    fi
else
    check "HTTP GET from $CLIENT (skipped — no virtual IP)" 1
fi

if [ -n "$VIRTUAL_IP_2" ]; then
    if echo "$RESPONSE_2" | grep -q "Fuck IPs"; then
        check "HTTP GET from $CLIENT2" 0
    else
        check "HTTP GET from $CLIENT2 (response: '${RESPONSE_2:0:80}')" 1
    fi
else
    check "HTTP GET from $CLIENT2 (skipped — no virtual IP)" 1
fi

# Phase 6: Verify NAT state on gateway
echo ""
echo "Phase 6: Gateway NAT state"
# Check that nftables rules were created
NFT_RULES=$(docker exec "$GATEWAY" nft list table inet fips_gateway 2>/dev/null || echo "")
if echo "$NFT_RULES" | grep -q "dnat"; then
    check "nftables DNAT rules present" 0
else
    check "nftables DNAT rules" 1
fi

# Phase 7: Inbound port forwarding — UDP and a second simultaneous TCP forward.
#
# Three forwards exercised:
#   tcp 18080 → [fd02::20]:8080  (original — single TCP rule)
#   tcp 18082 → [fd02::20]:8081  (6B — second TCP rule, multiple forwards)
#   udp 18081 → [fd02::20]:8081  (6A — UDP DNAT runtime path)
#
# Mesh peer (gw-server) hits each gw-gateway fips0:<port> rule, which
# DNATs into the LAN-side gw-client. Exercises the DNAT rules + LAN-side
# masquerade installed by set_port_forwards().
echo ""
echo "Phase 7: Inbound port forwards"

# Confirm all three port-forward DNAT rules are present on the gateway.
# The distinctive listen ports identify our rules regardless of how nft
# renders the l4proto/dport predicates.
if echo "$NFT_RULES" | grep -q "18080"; then
    check "nftables port-forward DNAT rule (tcp 18080)" 0
else
    check "nftables port-forward DNAT rule (tcp 18080)" 1
fi
if echo "$NFT_RULES" | grep -q "18082"; then
    check "nftables port-forward DNAT rule (tcp 18082)" 0
else
    check "nftables port-forward DNAT rule (tcp 18082)" 1
fi
if echo "$NFT_RULES" | grep -q "18081"; then
    check "nftables port-forward DNAT rule (udp 18081)" 0
else
    check "nftables port-forward DNAT rule (udp 18081)" 1
fi

# Start marker HTTP servers on the LAN-side client.
#   :8080 → "inbound-forward-ok"   (target of tcp 18080)
#   :8081 → "inbound-forward-ok-2" (target of tcp 18082)
# `docker exec -d` is required; `docker exec bash -c 'cmd &'` doesn't
# keep the child alive past the exec session, even with nohup.
docker exec "$CLIENT" sh -c '
    mkdir -p /tmp/inbound /tmp/inbound2
    echo "inbound-forward-ok"   > /tmp/inbound/index.html
    echo "inbound-forward-ok-2" > /tmp/inbound2/index.html
    pkill -f "http.server 8080" 2>/dev/null || true
    pkill -f "http.server 8081" 2>/dev/null || true
    pkill -f "udp_echo.py" 2>/dev/null || true
' >/dev/null 2>&1 || true
docker exec -d "$CLIENT" python3 -m http.server 8080 --bind :: --directory /tmp/inbound \
    >/dev/null 2>&1 || true
docker exec -d "$CLIENT" python3 -m http.server 8081 --bind :: --directory /tmp/inbound2 \
    >/dev/null 2>&1 || true

# Start a UDP echo server on the LAN-side client at [::]:8081/udp.
# This is the target of the udp 18081 forward. Stash the script as a
# named file (`udp_echo.py`) so the cleanup pkill above can find it.
docker exec "$CLIENT" sh -c 'cat > /tmp/udp_echo.py <<'\''PYEOF'\''
import socket, sys
s = socket.socket(socket.AF_INET6, socket.SOCK_DGRAM)
s.bind(("::", 8081))
while True:
    data, addr = s.recvfrom(2048)
    s.sendto(b"udp-forward-ok:" + data, addr)
PYEOF' >/dev/null 2>&1 || true
docker exec -d "$CLIENT" python3 /tmp/udp_echo.py >/dev/null 2>&1 || true

# Give the servers a moment to bind.
for _ in 1 2 3 4 5; do
    TCP_READY=$(docker exec "$CLIENT" ss -6lnt 2>/dev/null | grep -cE ':8080|:8081' || true)
    UDP_READY=$(docker exec "$CLIENT" ss -6lnu 2>/dev/null | grep -c ':8081' || true)
    if [ "$TCP_READY" -ge 2 ] && [ "$UDP_READY" -ge 1 ]; then
        break
    fi
    sleep 1
done

# Derive the gateway's mesh IPv6 (fd00::/8 address assigned to fips0).
GW_MESH_IP=$(docker exec "$GATEWAY" bash -c \
    "ip -6 -o addr show fips0 | awk '/inet6 fd/ {print \$4}' | cut -d/ -f1 | head -1" \
    2>/dev/null || echo "")

if [ -z "$GW_MESH_IP" ]; then
    check "Gateway fips0 IPv6 address" 1
else
    echo "  Gateway mesh IPv6: $GW_MESH_IP"

    # From the mesh side (gw-server), fetch through each TCP forward.
    FWD_RESPONSE=$(docker exec "$SERVER" curl -6 -s --max-time 10 \
        "http://[${GW_MESH_IP}]:18080/" 2>&1) || true
    # 8080 backend serves "inbound-forward-ok" (no -2 suffix) — distinct
    # from the 8081 backend so a misrouted response would be detectable.
    if echo "$FWD_RESPONSE" | grep -qE '^inbound-forward-ok$'; then
        check "Inbound HTTP via TCP forward 18080 → [${GW_CLIENT_LAN}]:8080" 0
    else
        check "Inbound HTTP via TCP forward 18080 (response: '${FWD_RESPONSE:0:80}')" 1
    fi

    FWD_RESPONSE_2=$(docker exec "$SERVER" curl -6 -s --max-time 10 \
        "http://[${GW_MESH_IP}]:18082/" 2>&1) || true
    if echo "$FWD_RESPONSE_2" | grep -q "inbound-forward-ok-2"; then
        check "Inbound HTTP via TCP forward 18082 → [${GW_CLIENT_LAN}]:8081 (6B)" 0
    else
        check "Inbound HTTP via TCP forward 18082 (response: '${FWD_RESPONSE_2:0:80}')" 1
    fi

    # 6A: UDP forward. Send a probe via a one-shot Python client on
    # gw-server; the LAN-side echo server prepends "udp-forward-ok:".
    UDP_RESPONSE=$(docker exec "$SERVER" python3 -c "
import socket, sys
s = socket.socket(socket.AF_INET6, socket.SOCK_DGRAM)
s.settimeout(5)
s.sendto(b'ping-via-udp-fwd', ('${GW_MESH_IP}', 18081))
try:
    data, _ = s.recvfrom(2048)
    sys.stdout.write(data.decode('utf-8', 'replace'))
except Exception as e:
    sys.stdout.write('ERR: ' + str(e))
" 2>&1) || true
    if echo "$UDP_RESPONSE" | grep -q "udp-forward-ok:ping-via-udp-fwd"; then
        check "Inbound UDP via forward 18081 → [${GW_CLIENT_LAN}]:8081 (6A)" 0
    else
        check "Inbound UDP via forward 18081 (response: '${UDP_RESPONSE:0:80}')" 1
    fi
fi

# Cleanup: stop the LAN-side responders so Phase 8's pool-reclamation
# wait isn't interfered with by lingering sessions.
docker exec "$CLIENT" sh -c '
    pkill -f "http.server 8080" 2>/dev/null || true
    pkill -f "http.server 8081" 2>/dev/null || true
    pkill -f "udp_echo.py" 2>/dev/null || true
' >/dev/null 2>&1 || true

# Phase 8: TTL expiration and pool reclamation
echo ""
echo "Phase 8: TTL expiration and pool reclamation"
# Flush conntrack so stale sessions from Phase 5 don't keep the mapping alive.
docker exec "$GATEWAY" conntrack -F 2>/dev/null || true
# Config uses ttl=5, pool_grace_period=5. Pool tick interval is 10s, so:
#   tick 1 (~10s): TTL expired → Draining (sessions=0 after flush)
#   tick 2 (~20s): grace expired → freed
# Wait 25s to ensure two full tick cycles have passed.
echo "  Waiting 25s for TTL + grace period to expire (two tick cycles)..."
sleep 25

# Query gateway control socket for mapping count.
#
# The expected value here is zero, so the reader must not be able to
# produce a zero from a failed query: an error response carries no `data`
# field, and `r.get('data',{}).get('mappings',[])` would report that as
# zero mappings and pass this check without the gateway having answered.
# The same hazard is documented at the show_mappings poll above, which is
# safe only because it waits for a positive "2". Require the key to exist
# and exit non-zero if it does not, so the `|| echo "error"` fallback
# fires and the check reds.
MAPPING_COUNT=$(docker exec "$GATEWAY" bash -c \
    'echo "{\"command\":\"show_mappings\"}" | nc -U -w1 /run/fips/gateway.sock 2>/dev/null' \
    | python3 -c "
import sys, json
r = json.load(sys.stdin)
data = r.get('data')
if not isinstance(data, dict) or not isinstance(data.get('mappings'), list):
    sys.exit(1)
print(len(data['mappings']))
" 2>/dev/null || echo "error")
if [ "$MAPPING_COUNT" = "0" ]; then
    check "Mapping reclaimed after TTL+grace" 0
else
    check "Mapping reclaimed (count: $MAPPING_COUNT)" 1
fi

# Phase 9: SERVFAIL when daemon DNS is down
echo ""
echo "Phase 9: SERVFAIL when daemon DNS is down"
# Kill the fips daemon inside the gateway container (gateway stays running)
docker exec "$GATEWAY" pkill -f "^fips --config" 2>/dev/null || true
sleep 2

# Gateway upstream timeout is 5s, so dig must wait longer than that.
SERVFAIL_RESULT=$(docker exec "$CLIENT" dig +short +tries=1 +time=8 AAAA "test-servfail.fips" @${GW_DNS} 2>&1 || true)
SERVFAIL_STATUS=$(docker exec "$CLIENT" dig +tries=1 +time=8 AAAA "test-servfail.fips" @${GW_DNS} 2>&1 | grep -c "SERVFAIL" || true)
if [ "$SERVFAIL_STATUS" -ge 1 ]; then
    check "SERVFAIL when daemon DNS is down" 0
else
    check "SERVFAIL when daemon DNS down (got: '${SERVFAIL_RESULT:0:80}')" 1
fi

# Phase 10: Cleanup verification (nftables removed on shutdown)
echo ""
echo "Phase 10: Cleanup on shutdown"
# fips-gateway is PID 1 (exec in entrypoint), so SIGTERM stops the container.
# Verify cleanup by checking container logs for the shutdown sequence.
docker stop --time=10 "$GATEWAY" >/dev/null 2>&1 || true
sleep 1

LOGS=$(docker logs --tail=20 "$GATEWAY" 2>&1)
if echo "$LOGS" | grep -q "shutdown complete"; then
    check "Gateway shutdown completed cleanly" 0
else
    check "Gateway shutdown (no completion message in logs)" 1
fi

# Phase 11: NAT rebuild past the default netlink socket limits
#
# Every change rebuilds the whole fips_gateway table in one netlink batch.
# With the default socket buffers that batch failed from about 105 mappings
# (the acks overflowed the receive buffer, after the commit) and past about
# 313 (the batch overflowed the send buffer, and nothing was committed).
# Drive 400 new names through a gateway whose mappings outlive the phase and
# judge the result on the kernel's own table, not on the daemon's debug-level
# success line, which the suite's log level does not show. Runs after phase
# 10, so the gateway container is stopped when it starts, and it leaves it
# stopped.
echo ""
echo "Phase 11: NAT rebuild past default socket limits"

NATBIG_NAMES=400
NATBIG_CAP=180
NATBIG_SETTLE=30

natbig_now() {
    date -u +%s
}

natbig_slice() {
    NATBIG_LOG=$(docker logs --timestamps --since "$NATBIG_STARTED" "$GATEWAY" 2>&1)
}

natbig_allocated() {
    natbig_slice
    NATBIG_ALLOCATED=$(grep -c "Allocated virtual IP" <<< "$NATBIG_LOG" || true)
}

# Rules the kernel holds right now. A failed listing is recorded through
# NATBIG_RC, never read as a table with no rules.
natbig_kernel() {
    NATBIG_RC=0
    NATBIG_NFT=$(docker exec "$GATEWAY" nft list table inet fips_gateway 2>&1) || NATBIG_RC=$?
    NATBIG_DNAT=$(grep -cE "daddr fd01:[0-9a-f:]* .*dnat" <<< "$NATBIG_NFT" || true)
    NATBIG_SNAT=$(grep -cE "saddr [0-9a-f:]+ .*snat" <<< "$NATBIG_NFT" || true)
    NATBIG_MASQ=$(grep -c "masquerade" <<< "$NATBIG_NFT" || true)
}

natbig_phase() {
    local config_file="$GENERATED_DIR/gateway/node-a.yaml"
    local expect_rev
    expect_rev=$(git -C "$SCRIPT_DIR" rev-parse --short=10 HEAD)

    # Rewrite in place (same inode): the container sees the host file through
    # a single-file bind mount, which a replace-by-rename would leave behind.
    python3 - "$config_file" <<'PYEOF'
import sys, yaml
path = sys.argv[1]
with open(path, "r+") as f:
    cfg = yaml.safe_load(f)
    cfg["gateway"]["dns"]["ttl"] = 1800
    cfg["gateway"]["pool_grace_period"] = 1800
    f.seek(0)
    yaml.dump(cfg, f, default_flow_style=False, sort_keys=False)
    f.truncate()
PYEOF

    docker start "$GATEWAY" >/dev/null
    NATBIG_STARTED=$(docker inspect -f '{{.State.StartedAt}}' "$GATEWAY")
    local t0
    t0=$(natbig_now)
    echo "  Gateway started at $NATBIG_STARTED (expect rev $expect_rev)"

    local seen_ttl seen_grace
    seen_ttl=$(docker exec "$GATEWAY" grep -c "ttl: 1800" /etc/fips/fips.yaml || true)
    seen_grace=$(docker exec "$GATEWAY" grep -c "pool_grace_period: 1800" /etc/fips/fips.yaml || true)
    if [ "$seen_ttl" -ge 1 ] && [ "$seen_grace" -ge 1 ]; then
        check "NAT batch: container sees ttl 1800 and grace 1800" 0
    else
        check "NAT batch: container config rewrite (ttl: $seen_ttl, grace: $seen_grace)" 1
        return 0
    fi

    if wait_for_peers "$GATEWAY" 2 60; then
        check "NAT batch: gateway peers after restart" 0
    else
        check "NAT batch: gateway peers after restart" 1
        return 0
    fi
    local ready=false probe
    for _ in $(seq 1 60); do
        probe=$(docker exec "$CLIENT" dig +short AAAA "${NPUB_B}.fips" @${GW_DNS} 2>/dev/null || true)
        if echo "$probe" | grep -q "^fd01::"; then
            ready=true
            break
        fi
        sleep 1
    done
    if [ "$ready" = true ]; then
        check "NAT batch: gateway DNS answers after restart" 0
    else
        check "NAT batch: gateway DNS answers after restart" 1
        return 0
    fi

    sleep 1
    natbig_allocated
    local rev_lines
    rev_lines=$(grep -cE "fips-gateway [^ ]+ \(rev ${expect_rev}\) starting" <<< "$NATBIG_LOG" || true)
    if [ "$rev_lines" -eq 1 ]; then
        check "NAT batch: startup line reads rev ${expect_rev}) with no -dirty" 0
    else
        check "NAT batch: startup line for rev ${expect_rev} (found $rev_lines)" 1
        return 0
    fi
    local baseline="$NATBIG_ALLOCATED"
    local target=$((baseline + NATBIG_NAMES))
    echo "  Baseline allocations after readiness: $baseline; target $target"
    if [ "$baseline" -ge 1 ]; then
        check "NAT batch: readiness probe allocated (baseline $baseline)" 0
    else
        check "NAT batch: readiness probe allocated (baseline $baseline)" 1
        return 0
    fi

    # Names: real keys, since the daemon parses each one as a public key.
    local names_file have_names
    names_file=$(mktemp)
    docker exec "$GATEWAY" bash -c \
        "for i in \$(seq 1 $NATBIG_NAMES); do fipsctl keygen --stdout; done" \
        | grep '^npub1' >"$names_file" || true
    have_names=$(wc -l <"$names_file")
    if [ "$have_names" -lt "$NATBIG_NAMES" ]; then
        check "NAT batch: generated $NATBIG_NAMES names (got $have_names)" 1
        rm -f "$names_file"
        return 0
    fi

    # Closed-loop AAAA driver, 4 workers, one fresh socket per query.
    docker exec -i "$CLIENT" sh -c 'cat > /tmp/gw_natbig.py' <<'PYEOF'
import random, socket, struct, sys, threading
server = sys.argv[1]
names = [n.strip() for n in sys.stdin if n.strip()]
lock = threading.Lock()
counts = {"answered": 0, "servfail": 0, "timeout": 0, "other": 0}
def query(name):
    qid = random.getrandbits(16)
    pkt = struct.pack(">HHHHHH", qid, 0x0100, 1, 0, 0, 0)
    for label in (name + ".fips").split("."):
        raw = label.encode()
        pkt += bytes([len(raw)]) + raw
    pkt += b"\x00" + struct.pack(">HH", 28, 1)
    s = socket.socket(socket.AF_INET6, socket.SOCK_DGRAM)
    s.settimeout(6)
    try:
        s.sendto(pkt, (server, 53))
        while True:
            data, _ = s.recvfrom(4096)
            if len(data) >= 12 and struct.unpack(">H", data[:2])[0] == qid:
                break
    except socket.timeout:
        return "timeout"
    finally:
        s.close()
    flags, _, ancount = struct.unpack(">HHH", data[2:8])
    rcode = flags & 0xF
    if rcode == 2:
        return "servfail"
    if rcode == 0 and ancount > 0:
        return "answered"
    return "other"
def worker():
    while True:
        with lock:
            if not names:
                return
            name = names.pop()
        outcome = query(name)
        with lock:
            counts[outcome] += 1
threads = [threading.Thread(target=worker) for _ in range(4)]
for t in threads:
    t.start()
for t in threads:
    t.join()
print(" ".join(f"{k}={v}" for k, v in counts.items()))
PYEOF

    local remaining out rc=0
    remaining=$((NATBIG_CAP - ($(natbig_now) - t0)))
    # timeout reads 0 as no limit and refuses a negative duration.
    if [ "$remaining" -le 0 ]; then
        check "NAT batch: setup exceeded ${NATBIG_CAP}s cap" 1
        rm -f "$names_file"
        return 0
    fi
    out=$(docker exec -i "$CLIENT" timeout "$remaining" \
        python3 /tmp/gw_natbig.py "$GW_DNS" <"$names_file" 2>&1) || rc=$?
    rm -f "$names_file"
    echo "  [$(($(natbig_now) - t0))s] sent $NATBIG_NAMES names: $out (rc=$rc)"
    natbig_allocated
    if [ "$rc" -eq 0 ] && [ "$NATBIG_ALLOCATED" -eq "$target" ]; then
        check "NAT batch: $NATBIG_ALLOCATED live mappings allocated" 0
    else
        check "NAT batch: live mappings allocated ($NATBIG_ALLOCATED of $target, rc $rc)" 1
    fi

    # Settle on the kernel: wait until it holds a DNAT rule per allocation,
    # or the cap expires. Reaching the cap decides nothing by itself; the
    # checks below do.
    local settle=0
    natbig_kernel
    while [ "$NATBIG_DNAT" -ne "$NATBIG_ALLOCATED" ] && [ "$settle" -lt "$NATBIG_SETTLE" ]; do
        sleep 1
        settle=$((settle + 1))
        natbig_kernel
    done
    # An error logged just after the last commit still counts.
    sleep 2
    natbig_kernel
    natbig_allocated
    local nat_fail
    nat_fail=$(grep -c "Failed to add NAT rules" <<< "$NATBIG_LOG" || true)
    echo "  [$(($(natbig_now) - t0))s] allocated=$NATBIG_ALLOCATED nat_add_fail=$nat_fail" \
        "table_rc=$NATBIG_RC dnat=$NATBIG_DNAT snat=$NATBIG_SNAT masquerade=$NATBIG_MASQ settle=${settle}s"
    if [ "$nat_fail" -gt 0 ]; then
        grep "Failed to add NAT rules" <<< "$NATBIG_LOG" | sed 's/\x1b\[[0-9;]*m//g' \
            | sed -n '1p;$p' | sed 's/^/    /'
    fi

    if [ "$nat_fail" -eq 0 ]; then
        check "NAT batch: no NAT rebuild failed" 0
    else
        check "NAT batch: NAT rebuilds failed ($nat_fail)" 1
    fi
    if [ "$NATBIG_RC" -eq 0 ]; then
        check "NAT batch: nft lists the fips_gateway table" 0
    else
        check "NAT batch: nft list table failed (rc $NATBIG_RC)" 1
    fi
    if [ "$NATBIG_DNAT" -eq "$NATBIG_ALLOCATED" ] && [ "$NATBIG_SNAT" -eq "$NATBIG_ALLOCATED" ]; then
        check "NAT batch: kernel holds a DNAT and SNAT rule per mapping ($NATBIG_DNAT)" 0
    else
        check "NAT batch: kernel rules (dnat $NATBIG_DNAT, snat $NATBIG_SNAT, allocated $NATBIG_ALLOCATED)" 1
    fi
    if [ "$NATBIG_MASQ" -eq 2 ]; then
        check "NAT batch: fips0 and LAN masquerades present" 0
    else
        check "NAT batch: masquerade rules ($NATBIG_MASQ, expected 2)" 1
    fi

    docker stop --time=10 "$GATEWAY" >/dev/null 2>&1 || true
    echo "  Phase time: $(($(natbig_now) - t0))s"
}

natbig_phase

echo ""
echo "=== Results: $PASSED passed, $FAILED failed ==="
[ "$FAILED" -eq 0 ] && exit 0 || exit 1
