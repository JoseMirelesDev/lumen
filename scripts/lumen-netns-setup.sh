#!/usr/bin/env bash
# Sets up a network namespace "lumen2" with a veth pair + NAT so a second
# Lumen client can run inside it and see REAL packet-level latency/loss
# (tc netem) on its RTP, simulating a distant peer without a second machine.
#
# Run with sudo. Idempotent: cleans up any previous partial setup first.
#
#   sudo bash scripts/lumen-netns-setup.sh   # create + apply tc
#   sudo bash scripts/lumen-netns-setup.sh down   # tear everything down

set -euo pipefail

NS=lumen2
VETH_A=veth0    # host side
VETH_B=veth1    # in the netns
NET=10.77.0.0/24
HOST_IP=10.77.0.1
NS_IP=10.77.0.2
# Latency/loss profile (RTT ~100ms total path + 1% loss) — the netem delay is
# applied on the host side so it hits the SECOND client's outbound RTP.
# Single delay value + explicit limit: some tc versions reject `delay A B`
# (jitter form) with "Illegal latency" unless a queue limit is given.
LATENCY="50ms"
LOSS="1%"
LIMIT="1000"

down() {
  echo "[*] tearing down $NS"
  ip link del "$VETH_A" 2>/dev/null || true
  ip netns del "$NS" 2>/dev/null || true
  # drop any leftover netem on the netns side's host-facing link
  echo "[*] done"
  exit 0
}

if [ "${1:-}" = "down" ]; then down; fi

echo "[*] cleaning previous state"
ip link del "$VETH_A" 2>/dev/null || true
ip netns del "$NS" 2>/dev/null || true

echo "[*] setting up netns DNS (/etc/netns/$NS/resolv.conf)"
mkdir -p "/etc/netns/$NS"
echo "nameserver 8.8.8.8" > "/etc/netns/$NS/resolv.conf"

echo "[*] creating netns $NS"
ip netns add "$NS"

echo "[*] creating veth pair $VETH_A <-> $VETH_B"
ip link add "$VETH_A" type veth peer name "$VETH_B"

echo "[*] moving $VETH_B into $NS"
ip link set "$VETH_B" netns "$NS"

echo "[*] configuring host side $VETH_A = $HOST_IP"
ip addr add "$HOST_IP/24" dev "$VETH_A"
ip link set "$VETH_A" up

echo "[*] configuring netns side $VETH_B = $NS_IP"
ip netns exec "$NS" ip addr add "$NS_IP/24" dev "$VETH_B"
ip netns exec "$NS" ip link set "$VETH_B" up
ip netns exec "$NS" ip link set lo up

echo "[*] netns default route via host"
ip netns exec "$NS" ip route add default via "$HOST_IP" dev "$VETH_B"

echo "[*] enabling NAT (masquerade) so the netns reaches the internet"
iptables -t nat -C POSTROUTING -s "$NET" ! -o "$VETH_A" -j MASQUERADE 2>/dev/null || \
  iptables -t nat -A POSTROUTING -s "$NET" ! -o "$VETH_A" -j MASQUERADE
# forwarding: host <-> netns
sysctl -w net.ipv4.ip_forward=1 >/dev/null

echo "[*] applying tc netem on host side $VETH_A (hits client2 outbound)"
tc qdisc add dev "$VETH_A" root netem delay "$LATENCY" loss "$LOSS" limit "$LIMIT"

echo ""
echo "=== setup complete ==="
echo "  host side:  $VETH_A = $HOST_IP (tc: delay $LATENCY loss $LOSS limit $LIMIT)"
echo "  netns side: $VETH_B = $NS_IP"
echo "  run a command in the netns:  sudo ip netns exec $NS <cmd>"
echo "  test:  sudo ip netns exec $NS ping -c3 8.8.8.8"
echo "  teardown:  sudo bash $0 down"
