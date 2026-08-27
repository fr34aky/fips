//! UDP Transport Implementation
//!
//! Provides UDP-based transport for FIPS peer communication.

use super::{
    DiscoveredPeer, PacketTx, ReceivedPacket, Transport, TransportAddr, TransportError,
    TransportId, TransportState, TransportType,
};
pub(crate) mod io;
#[cfg(target_os = "macos")]
pub(crate) mod sockopts_macos;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use io::open_connected_fd;
mod stats;
use super::resolve_socket_addr;
use crate::config::UdpConfig;
use crate::nostr::is_punch_packet;
use io::{AsyncUdpSocket, UdpRawSocket};
use stats::UdpStats;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use tracing::{debug, info, trace, warn};

/// DNS cache TTL for hostname resolution (60 seconds).
const DNS_CACHE_TTL: Duration = Duration::from_secs(60);

/// A parsed `dial_prefixes` entry: network address + prefix length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DialPrefix {
    net: IpAddr,
    len: u8,
}

impl DialPrefix {
    /// Parse `"addr/len"` CIDR notation. Returns `None` on malformed
    /// input or an out-of-range prefix length for the address family.
    pub fn parse(s: &str) -> Option<Self> {
        let (addr, len) = s.trim().split_once('/')?;
        let net: IpAddr = addr.parse().ok()?;
        let len: u8 = len.parse().ok()?;
        let max = match net {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        };
        (len <= max).then_some(Self { net, len })
    }

    /// Whether `ip` falls inside this prefix (same family, leading
    /// `len` bits equal).
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.net, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = if self.len == 0 { 0 } else { u32::MAX << (32 - self.len as u32) };
                (u32::from(net) & mask) == (u32::from(ip) & mask)
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = if self.len == 0 { 0 } else { u128::MAX << (128 - self.len as u32) };
                (u128::from(net) & mask) == (u128::from(ip) & mask)
            }
            _ => false,
        }
    }

    /// Prefix length in bits.
    pub fn len(&self) -> u8 {
        self.len
    }
}

/// UDP transport for FIPS.
///
/// Provides connectionless, unreliable packet delivery over UDP/IP.
/// A single socket serves all peers; links are virtual tuples of
/// (transport_id, remote_addr).
pub struct UdpTransport {
    /// Unique transport identifier.
    transport_id: TransportId,
    /// Optional instance name (for named instances in config).
    name: Option<String>,
    /// Configuration.
    config: UdpConfig,
    /// Current state.
    state: TransportState,
    /// Bound socket (None until started).
    socket: Option<AsyncUdpSocket>,
    /// Channel for delivering received packets to Node.
    packet_tx: PacketTx,
    /// Receive loop task handle.
    recv_task: Option<JoinHandle<()>>,
    /// Local bound address (after start).
    local_addr: Option<SocketAddr>,
    /// Transport statistics.
    stats: Arc<UdpStats>,
    /// DNS resolution cache for hostname addresses.
    dns_cache: StdMutex<HashMap<TransportAddr, (SocketAddr, Instant)>>,
    /// Embedder socket-protect hook (Android `VpnService.protect`), applied
    /// to the socket on start/adopt before any traffic is sent.
    socket_protect: Option<crate::transport::SocketProtect>,
    /// Parsed `dial_prefixes` (empty = unscoped). Malformed config
    /// entries are dropped with a warning at construction.
    dial_prefixes: Vec<DialPrefix>,
}

impl UdpTransport {
    /// Create a new UDP transport.
    pub fn new(
        transport_id: TransportId,
        name: Option<String>,
        config: UdpConfig,
        packet_tx: PacketTx,
    ) -> Self {
        let dial_prefixes = config
            .dial_prefixes()
            .iter()
            .filter_map(|s| {
                let parsed = DialPrefix::parse(s);
                if parsed.is_none() {
                    warn!(prefix = %s, "udp: ignoring malformed dial_prefixes entry");
                }
                parsed
            })
            .collect();
        Self {
            transport_id,
            name,
            config,
            state: TransportState::Configured,
            socket: None,
            packet_tx,
            recv_task: None,
            local_addr: None,
            stats: Arc::new(UdpStats::new()),
            dns_cache: StdMutex::new(HashMap::new()),
            socket_protect: None,
            dial_prefixes,
        }
    }

    /// Whether this instance is dial-scoped (has at least one parsed
    /// `dial_prefixes` entry).
    pub fn dial_scoped(&self) -> bool {
        !self.dial_prefixes.is_empty()
    }

    /// Longest configured dial prefix containing `ip`, as a prefix
    /// length in bits. `None` when unscoped or no prefix matches.
    pub fn dial_prefix_match(&self, ip: IpAddr) -> Option<u8> {
        self.dial_prefixes
            .iter()
            .filter(|p| p.contains(ip))
            .map(|p| p.len())
            .max()
    }

    /// Install the embedder socket-protect hook (Android
    /// `VpnService.protect`). Must be called before `start_async` /
    /// `adopt_socket_async`; the hook runs with the socket's raw handle
    /// before any traffic is sent on it.
    pub fn set_socket_protect(&mut self, hook: crate::transport::SocketProtect) {
        self.socket_protect = Some(hook);
    }

    /// Get the instance name (if configured as a named instance).
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Get the local bound address (only valid after start).
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.local_addr
    }

    /// Configured recv buffer size — used when opening per-peer
    /// `ConnectedPeerSocket`s so they get the same buffer ceiling as
    /// the wildcard listen socket.
    pub fn recv_buf_size(&self) -> usize {
        self.config.recv_buf_size()
    }

    /// Configured send buffer size — companion to `recv_buf_size`.
    pub fn send_buf_size(&self) -> usize {
        self.config.send_buf_size()
    }

    /// Clone the `PacketTx` end of the packet channel for off-task
    /// receive paths (per-peer connected-socket drains).
    pub fn clone_packet_tx(&self) -> PacketTx {
        self.packet_tx.clone()
    }

    /// Get the transport statistics.
    pub fn stats(&self) -> &Arc<UdpStats> {
        &self.stats
    }

    /// Resolve a transport address (numeric `1.2.3.4:5678` or hostname)
    /// to a `SocketAddr` via the per-transport DNS cache. Public
    /// companion to `async_socket()` for off-task workers.
    pub async fn resolve_for_off_task(
        &self,
        addr: &TransportAddr,
    ) -> Result<SocketAddr, TransportError> {
        self.resolve_cached(addr).await
    }

    /// Clone the underlying async UDP socket. Returns `None` if the
    /// transport hasn't been started yet. The clone is just an `Arc`
    /// refcount bump on `AsyncFd<UdpRawSocket>`.
    pub fn async_socket(&self) -> Option<AsyncUdpSocket> {
        self.socket.clone()
    }

    /// Resolve a transport address, using cached results for hostnames.
    ///
    /// Numeric IP addresses bypass the cache entirely. Hostnames are
    /// resolved via DNS and cached for `DNS_CACHE_TTL` to avoid
    /// per-packet resolution overhead.
    async fn resolve_cached(&self, addr: &TransportAddr) -> Result<SocketAddr, TransportError> {
        // Fast path: try numeric IP parse (no cache, no DNS)
        if let Some(s) = addr.as_str()
            && let Ok(sock_addr) = s.parse::<SocketAddr>()
        {
            return Ok(sock_addr);
        }

        // Check cache
        {
            let cache = self.dns_cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((resolved, cached_at)) = cache.get(addr)
                && cached_at.elapsed() < DNS_CACHE_TTL
            {
                return Ok(*resolved);
            }
        }

        // Cache miss or expired — resolve via DNS
        let resolved = resolve_socket_addr(addr).await?;

        // Store in cache
        {
            let mut cache = self.dns_cache.lock().unwrap_or_else(|e| e.into_inner());
            cache.insert(addr.clone(), (resolved, Instant::now()));
        }

        Ok(resolved)
    }

    /// Query transport-local congestion indicators.
    pub fn congestion(&self) -> super::TransportCongestion {
        super::TransportCongestion {
            recv_drops: Some(
                self.stats
                    .kernel_drops
                    .load(std::sync::atomic::Ordering::Relaxed),
            ),
        }
    }

    /// Start the transport asynchronously.
    ///
    /// Binds the UDP socket and spawns the receive loop.
    pub async fn start_async(&mut self) -> Result<(), TransportError> {
        if !self.state.can_start() {
            return Err(TransportError::AlreadyStarted);
        }

        self.state = TransportState::Starting;

        if self.config.outbound_only() && self.config.bind_addr.is_some() {
            warn!(
                configured_bind_addr = ?self.config.bind_addr,
                "udp.outbound_only = true; configured bind_addr is ignored, binding to 0.0.0.0:0"
            );
        }

        // Parse bind address
        let bind_addr: SocketAddr = self
            .config
            .bind_addr()
            .parse()
            .map_err(|e| TransportError::StartFailed(format!("invalid bind address: {}", e)))?;

        // Create, bind, and configure UDP socket
        let raw_socket = UdpRawSocket::open(
            bind_addr,
            self.config.recv_buf_size(),
            self.config.send_buf_size(),
        )?;

        // Protect before the recv loop spawns / any send happens.
        crate::transport::apply_socket_protect(self.socket_protect.as_ref(), &raw_socket);

        let actual_recv = raw_socket.recv_buffer_size()?;
        let actual_send = raw_socket.send_buffer_size()?;
        self.local_addr = Some(raw_socket.local_addr());

        // Wrap in AsyncFd for tokio integration
        let async_socket = raw_socket.into_async()?;
        self.socket = Some(async_socket.clone());

        // Spawn receive loop
        let transport_id = self.transport_id;
        let packet_tx = self.packet_tx.clone();
        let mtu = self.config.mtu();
        let stats = self.stats.clone();

        let recv_task = tokio::spawn(async move {
            udp_receive_loop(async_socket, transport_id, packet_tx, mtu, stats).await;
        });

        self.recv_task = Some(recv_task);
        self.state = TransportState::Up;

        if let Some(ref name) = self.name {
            info!(
                name = %name,
                local_addr = %self.local_addr.map_or_else(|| "<unbound>".to_string(), |a| a.to_string()),
                recv_buf = actual_recv,
                send_buf = actual_send,
                "UDP transport started"
            );
        } else {
            info!(
                local_addr = %self.local_addr.map_or_else(|| "<unbound>".to_string(), |a| a.to_string()),
                recv_buf = actual_recv,
                send_buf = actual_send,
                "UDP transport started"
            );
        }

        Ok(())
    }

    /// Start the transport using an already-bound UDP socket.
    ///
    /// This preserves an existing NAT mapping established by another
    /// subsystem, such as STUN or UDP hole punching.
    pub async fn adopt_socket_async(
        &mut self,
        socket: std::net::UdpSocket,
    ) -> Result<(), TransportError> {
        if !self.state.can_start() {
            return Err(TransportError::AlreadyStarted);
        }

        self.state = TransportState::Starting;

        // Re-announce the adopted fd to the protect hook. Bootstrap code
        // that created the socket already protected it (same underlying
        // socket), but the hook is documented idempotent and this keeps
        // every socket the transport owns covered regardless of origin.
        crate::transport::apply_socket_protect(self.socket_protect.as_ref(), &socket);

        let raw_socket = UdpRawSocket::adopt(
            socket,
            self.config.recv_buf_size(),
            self.config.send_buf_size(),
        )?;

        let actual_recv = raw_socket.recv_buffer_size()?;
        let actual_send = raw_socket.send_buffer_size()?;
        self.local_addr = Some(raw_socket.local_addr());

        let async_socket = raw_socket.into_async()?;
        self.socket = Some(async_socket.clone());

        let transport_id = self.transport_id;
        let packet_tx = self.packet_tx.clone();
        let mtu = self.config.mtu();
        let stats = self.stats.clone();

        let recv_task = tokio::spawn(async move {
            udp_receive_loop(async_socket, transport_id, packet_tx, mtu, stats).await;
        });

        self.recv_task = Some(recv_task);
        self.state = TransportState::Up;

        if let Some(ref name) = self.name {
            info!(
                name = %name,
                local_addr = %self.local_addr.map_or_else(|| "<unbound>".to_string(), |a| a.to_string()),
                recv_buf = actual_recv,
                send_buf = actual_send,
                "UDP transport adopted existing socket"
            );
        } else {
            info!(
                local_addr = %self.local_addr.map_or_else(|| "<unbound>".to_string(), |a| a.to_string()),
                recv_buf = actual_recv,
                send_buf = actual_send,
                "UDP transport adopted existing socket"
            );
        }

        Ok(())
    }

    /// Stop the transport asynchronously.
    pub async fn stop_async(&mut self) -> Result<(), TransportError> {
        if !self.state.is_operational() {
            return Err(TransportError::NotStarted);
        }

        // Abort receive task
        if let Some(task) = self.recv_task.take() {
            task.abort();
            let _ = task.await; // Ignore JoinError from abort
        }

        // Drop socket
        self.socket.take();
        self.local_addr = None;

        self.state = TransportState::Down;

        info!(
            transport_id = %self.transport_id,
            "UDP transport stopped"
        );

        Ok(())
    }

    /// Send a packet asynchronously.
    pub async fn send_async(
        &self,
        addr: &TransportAddr,
        data: &[u8],
    ) -> Result<usize, TransportError> {
        if !self.state.is_operational() {
            return Err(TransportError::NotStarted);
        }

        if data.len() > self.config.mtu() as usize {
            self.stats.record_mtu_exceeded();
            return Err(TransportError::MtuExceeded {
                packet_size: data.len(),
                mtu: self.config.mtu(),
            });
        }

        let socket_addr = self.resolve_cached(addr).await?;
        let socket = self.socket.as_ref().ok_or(TransportError::NotStarted)?;

        match socket.send_to(data, &socket_addr).await {
            Ok(bytes_sent) => {
                self.stats.record_send(bytes_sent);
                trace!(
                    transport_id = %self.transport_id,
                    remote_addr = %socket_addr,
                    bytes = bytes_sent,
                    "UDP packet sent"
                );
                Ok(bytes_sent)
            }
            Err(e) => {
                self.stats.record_send_error();
                Err(e)
            }
        }
    }
}

impl Transport for UdpTransport {
    fn transport_id(&self) -> TransportId {
        self.transport_id
    }

    fn transport_type(&self) -> &TransportType {
        &TransportType::UDP
    }

    fn state(&self) -> TransportState {
        self.state
    }

    fn mtu(&self) -> u16 {
        self.config.mtu()
    }

    fn start(&mut self) -> Result<(), TransportError> {
        // Synchronous start not supported - use start_async()
        Err(TransportError::NotSupported(
            "use start_async() for UDP transport".into(),
        ))
    }

    fn stop(&mut self) -> Result<(), TransportError> {
        // Synchronous stop not supported - use stop_async()
        Err(TransportError::NotSupported(
            "use stop_async() for UDP transport".into(),
        ))
    }

    fn send(&self, _addr: &TransportAddr, _data: &[u8]) -> Result<(), TransportError> {
        // Synchronous send not supported - use send_async()
        Err(TransportError::NotSupported(
            "use send_async() for UDP transport".into(),
        ))
    }

    fn discover(&self) -> Result<Vec<DiscoveredPeer>, TransportError> {
        // UDP discovery not yet implemented (would use multicast/DNS-SD)
        // Peer configuration is handled at the node level, not transport level
        Ok(Vec::new())
    }

    /// Whether the transport accepts inbound handshake initiations.
    /// `outbound_only` mode forces this to false; otherwise reflects the
    /// `accept_connections` config field (default: true). Note that the
    /// hard gate is at the Node level (see ISSUE-2026-0004 fix in
    /// `src/node/handlers/handshake.rs`); this method is what that gate
    /// consults for transports that lack runtime-state-based filtering.
    fn accept_connections(&self) -> bool {
        if self.config.outbound_only() {
            false
        } else {
            self.config.accept_connections()
        }
    }
}

impl Drop for UdpTransport {
    fn drop(&mut self) {
        let had_task = self.recv_task.is_some();
        let had_socket = self.socket.is_some();
        if had_task || had_socket {
            debug!(
                transport_id = %self.transport_id,
                state = ?self.state,
                had_recv_task = had_task,
                had_socket = had_socket,
                "UdpTransport dropped without stop_async(); cleaning up",
            );
        }
        if let Some(task) = self.recv_task.take() {
            task.abort();
        }
        self.socket.take();
        self.local_addr = None;
    }
}

/// UDP receive loop - runs as a spawned task.
///
/// Drains the kernel UDP queue in 32-packet bursts via `recvmmsg` (Linux) or
/// `recvmsg_x` (macOS) to amortise the per-syscall + per-task-wakeup overhead.
/// Other unix targets and Windows fall through to single-packet `recv_from`.
/// Either way every datagram is forwarded to `packet_tx` in arrival order.
async fn udp_receive_loop(
    socket: AsyncUdpSocket,
    transport_id: TransportId,
    packet_tx: PacketTx,
    mtu: u16,
    stats: Arc<UdpStats>,
) {
    debug!(transport_id = %transport_id, "UDP receive loop starting");

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        const BATCH: usize = 32;
        let buf_size = mtu as usize + 100;
        // One Vec per recvmmsg / recvmsg_x slot. When a packet lands, move the
        // filled buffer directly into ReceivedPacket and install a fresh empty
        // buffer for the next syscall, avoiding a per-packet memcpy.
        let mut backing: Vec<Vec<u8>> = (0..BATCH).map(|_| vec![0u8; buf_size]).collect();
        let mut addrs: [Option<std::net::SocketAddr>; BATCH] = std::array::from_fn(|_| None);
        let mut lens: [usize; BATCH] = [0; BATCH];

        loop {
            // Build mutable slice references for the syscall layer.
            // Drawing from a single `iter_mut()` keeps the borrows disjoint
            // without `MaybeUninit`/`transmute`.
            let mut bufs: [&mut [u8]; BATCH] = {
                let mut iter = backing.iter_mut();
                std::array::from_fn(|_| iter.next().unwrap().as_mut_slice())
            };

            match socket.recv_batch(&mut bufs, &mut addrs, &mut lens).await {
                Ok((count, kernel_drops)) => {
                    stats.set_kernel_drops(kernel_drops as u64);
                    for i in 0..count {
                        let len = lens[i];
                        let Some(remote_addr) = addrs[i] else {
                            continue;
                        };
                        stats.record_recv(len);

                        // Peek before swap — punch probes / acks are
                        // discarded without consuming a buffer move.
                        if is_punch_packet(&backing[i][..len]) {
                            trace!(
                                transport_id = %transport_id,
                                remote_addr = %remote_addr,
                                bytes = len,
                                "Dropping stray punch probe/ack on UDP transport"
                            );
                            continue;
                        }

                        // Move the filled buffer out of the slot and
                        // refill with a fresh one. `mem::replace`
                        // returns the OLD Vec and writes the new one —
                        // single pointer swap, no per-packet memcpy of
                        // the ~MTU-sized payload (previously
                        // `buf.to_vec()` cost ~150 MB/sec of memory
                        // bandwidth on the RX hot path at 100 kpps).
                        let mut data = std::mem::replace(&mut backing[i], vec![0u8; buf_size]);
                        data.truncate(len);
                        let addr = TransportAddr::from_socket_addr(remote_addr);
                        let packet = ReceivedPacket::new(transport_id, addr, data);

                        trace!(
                            transport_id = %transport_id,
                            remote_addr = %remote_addr,
                            bytes = len,
                            "UDP packet received"
                        );

                        if packet_tx.send(packet).await.is_err() {
                            debug!(
                                transport_id = %transport_id,
                                "Packet channel closed, stopping receive loop"
                            );
                            return;
                        }
                    }
                }
                Err(e) => {
                    stats.record_recv_error();
                    warn!(
                        transport_id = %transport_id,
                        error = %e,
                        "UDP receive error"
                    );
                }
            }
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let mut buf = vec![0u8; mtu as usize + 100];

        loop {
            match socket.recv_from(&mut buf).await {
                Ok((len, remote_addr, kernel_drops)) => {
                    stats.record_recv(len);
                    stats.set_kernel_drops(kernel_drops as u64);

                    if is_punch_packet(&buf[..len]) {
                        trace!(
                            transport_id = %transport_id,
                            remote_addr = %remote_addr,
                            bytes = len,
                            "Dropping stray punch probe/ack on UDP transport"
                        );
                        continue;
                    }

                    let data = buf[..len].to_vec();
                    let addr = TransportAddr::from_socket_addr(remote_addr);
                    let packet = ReceivedPacket::new(transport_id, addr, data);

                    trace!(
                        transport_id = %transport_id,
                        remote_addr = %remote_addr,
                        bytes = len,
                        "UDP packet received"
                    );

                    if packet_tx.send(packet).await.is_err() {
                        debug!(
                            transport_id = %transport_id,
                            "Packet channel closed, stopping receive loop"
                        );
                        break;
                    }
                }
                Err(e) => {
                    stats.record_recv_error();
                    warn!(
                        transport_id = %transport_id,
                        error = %e,
                        "UDP receive error"
                    );
                }
            }
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::packet_channel;
    use tokio::time::{Duration, timeout};

    fn make_config(port: u16) -> UdpConfig {
        UdpConfig {
            bind_addr: Some(format!("127.0.0.1:{}", port)),
            mtu: Some(1280),
            ..Default::default()
        }
    }

    /// CIDR parsing: valid v4/v6, malformed input, out-of-range lengths.
    #[test]
    fn dial_prefix_parse() {
        assert!(DialPrefix::parse("192.168.49.0/24").is_some());
        assert!(DialPrefix::parse("fd00::/8").is_some());
        assert!(DialPrefix::parse(" 10.0.0.0/8 ").is_some(), "trims whitespace");
        assert!(DialPrefix::parse("192.168.49.0").is_none(), "missing /len");
        assert!(DialPrefix::parse("192.168.49.0/33").is_none(), "v4 len > 32");
        assert!(DialPrefix::parse("fd00::/129").is_none(), "v6 len > 128");
        assert!(DialPrefix::parse("not-an-ip/24").is_none());
        assert!(DialPrefix::parse("192.168.49.0/abc").is_none());
    }

    /// Containment: family mismatch never matches; /0 matches everything
    /// in-family; boundary bits respected.
    #[test]
    fn dial_prefix_contains() {
        let p = DialPrefix::parse("192.168.49.0/24").unwrap();
        assert!(p.contains("192.168.49.7".parse().unwrap()));
        assert!(!p.contains("192.168.50.7".parse().unwrap()));
        assert!(!p.contains("fd00::1".parse().unwrap()), "family mismatch");

        let v6 = DialPrefix::parse("fe80::/10").unwrap();
        assert!(v6.contains("fe80::1234".parse().unwrap()));
        assert!(!v6.contains("fd00::1".parse().unwrap()));

        let all4 = DialPrefix::parse("0.0.0.0/0").unwrap();
        assert!(all4.contains("203.0.113.9".parse().unwrap()));
        assert!(!all4.contains("::1".parse().unwrap()), "v4 /0 is still v4-only");
    }

    /// Transport-level scoping: malformed entries dropped, longest
    /// matching prefix wins, unscoped transport reports no match.
    #[test]
    fn dial_prefix_scoping_on_transport() {
        let (tx, _rx) = packet_channel(10);
        let config = UdpConfig {
            dial_prefixes: Some(vec![
                "192.168.0.0/16".to_string(),
                "192.168.49.0/24".to_string(),
                "bogus".to_string(),
            ]),
            ..Default::default()
        };
        let t = UdpTransport::new(TransportId::new(1), None, config, tx.clone());
        assert!(t.dial_scoped());
        assert_eq!(t.dial_prefix_match("192.168.49.7".parse().unwrap()), Some(24));
        assert_eq!(t.dial_prefix_match("192.168.2.7".parse().unwrap()), Some(16));
        assert_eq!(t.dial_prefix_match("10.0.0.1".parse().unwrap()), None);

        let unscoped = UdpTransport::new(TransportId::new(2), None, UdpConfig::default(), tx);
        assert!(!unscoped.dial_scoped());
        assert_eq!(unscoped.dial_prefix_match("192.168.49.7".parse().unwrap()), None);
    }

    #[tokio::test]
    async fn test_start_stop() {
        let (tx, _rx) = packet_channel(100);
        let mut transport = UdpTransport::new(TransportId::new(1), None, make_config(0), tx);

        assert_eq!(transport.state(), TransportState::Configured);

        transport.start_async().await.unwrap();
        assert_eq!(transport.state(), TransportState::Up);
        assert!(transport.local_addr().is_some());

        transport.stop_async().await.unwrap();
        assert_eq!(transport.state(), TransportState::Down);
    }

    /// The embedder socket-protect hook (Android `VpnService.protect`) must
    /// fire exactly once — with the listen socket's handle — on `start_async`.
    #[tokio::test]
    async fn test_socket_protect_fires_on_start() {
        let (tx, _rx) = packet_channel(100);
        let mut transport = UdpTransport::new(TransportId::new(1), None, make_config(0), tx);

        let seen = Arc::new(StdMutex::new(Vec::new()));
        let sink = seen.clone();
        transport.set_socket_protect(Arc::new(move |handle| {
            sink.lock().unwrap().push(handle);
        }));

        transport.start_async().await.unwrap();
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "listen socket protected exactly once"
        );

        transport.stop_async().await.unwrap();
    }

    /// The hook must also fire for adopted sockets (NAT-traversal handoff),
    /// with the adopted socket's own handle.
    #[tokio::test]
    async fn test_socket_protect_fires_on_adopt() {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        #[cfg(unix)]
        let expected = {
            use std::os::unix::io::AsRawFd;
            socket.as_raw_fd()
        };

        let (tx, _rx) = packet_channel(100);
        let mut transport = UdpTransport::new(TransportId::new(1), None, make_config(0), tx);

        let seen = Arc::new(StdMutex::new(Vec::new()));
        let sink = seen.clone();
        transport.set_socket_protect(Arc::new(move |handle| {
            sink.lock().unwrap().push(handle);
        }));

        transport.adopt_socket_async(socket).await.unwrap();
        let handles = seen.lock().unwrap().clone();
        assert_eq!(handles.len(), 1, "adopted socket protected exactly once");
        #[cfg(unix)]
        assert_eq!(handles[0], expected, "hook saw the adopted socket's fd");

        transport.stop_async().await.unwrap();
    }

    #[tokio::test]
    async fn test_double_start_fails() {
        let (tx, _rx) = packet_channel(100);
        let mut transport = UdpTransport::new(TransportId::new(1), None, make_config(0), tx);

        transport.start_async().await.unwrap();

        let result = transport.start_async().await;
        assert!(matches!(result, Err(TransportError::AlreadyStarted)));

        transport.stop_async().await.unwrap();
    }

    #[tokio::test]
    async fn test_stop_not_started_fails() {
        let (tx, _rx) = packet_channel(100);
        let mut transport = UdpTransport::new(TransportId::new(1), None, make_config(0), tx);

        let result = transport.stop_async().await;
        assert!(matches!(result, Err(TransportError::NotStarted)));
    }

    #[tokio::test]
    async fn test_send_recv() {
        let (tx1, _rx1) = packet_channel(100);
        let (tx2, mut rx2) = packet_channel(100);

        let mut t1 = UdpTransport::new(TransportId::new(1), None, make_config(0), tx1);
        let mut t2 = UdpTransport::new(TransportId::new(2), None, make_config(0), tx2);

        t1.start_async().await.unwrap();
        t2.start_async().await.unwrap();

        let addr1 = t1.local_addr().unwrap();
        let addr2 = t2.local_addr().unwrap();

        // Send from t1 to t2
        let data = b"hello world";
        let bytes_sent = t1
            .send_async(&TransportAddr::from_string(&addr2.to_string()), data)
            .await
            .unwrap();
        assert_eq!(bytes_sent, data.len());

        // Receive on t2
        let packet = timeout(Duration::from_secs(1), rx2.recv())
            .await
            .expect("timeout")
            .expect("channel closed");

        assert_eq!(packet.data, data);
        assert_eq!(
            packet.remote_addr.as_str(),
            Some(addr1.to_string().as_str())
        );

        t1.stop_async().await.unwrap();
        t2.stop_async().await.unwrap();
    }

    #[tokio::test]
    async fn test_bidirectional() {
        let (tx1, mut rx1) = packet_channel(100);
        let (tx2, mut rx2) = packet_channel(100);

        let mut t1 = UdpTransport::new(TransportId::new(1), None, make_config(0), tx1);
        let mut t2 = UdpTransport::new(TransportId::new(2), None, make_config(0), tx2);

        t1.start_async().await.unwrap();
        t2.start_async().await.unwrap();

        let addr1 = TransportAddr::from_string(&t1.local_addr().unwrap().to_string());
        let addr2 = TransportAddr::from_string(&t2.local_addr().unwrap().to_string());

        // Send from t1 to t2
        t1.send_async(&addr2, b"ping").await.unwrap();

        // Receive on t2
        let packet = timeout(Duration::from_secs(1), rx2.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        assert_eq!(packet.data, b"ping");

        // Send from t2 to t1
        t2.send_async(&addr1, b"pong").await.unwrap();

        // Receive on t1
        let packet = timeout(Duration::from_secs(1), rx1.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        assert_eq!(packet.data, b"pong");

        t1.stop_async().await.unwrap();
        t2.stop_async().await.unwrap();
    }

    #[tokio::test]
    async fn test_mtu_exceeded() {
        let (tx, _rx) = packet_channel(100);
        let mut transport = UdpTransport::new(
            TransportId::new(1),
            None,
            UdpConfig {
                mtu: Some(100),
                ..make_config(0)
            },
            tx,
        );

        transport.start_async().await.unwrap();

        let oversized = vec![0u8; 200];
        let result = transport
            .send_async(&TransportAddr::from_string("127.0.0.1:9999"), &oversized)
            .await;

        assert!(matches!(result, Err(TransportError::MtuExceeded { .. })));

        transport.stop_async().await.unwrap();
    }

    #[tokio::test]
    async fn test_send_not_started() {
        let (tx, _rx) = packet_channel(100);
        let transport = UdpTransport::new(TransportId::new(1), None, make_config(0), tx);

        let result = transport
            .send_async(&TransportAddr::from_string("127.0.0.1:9999"), b"test")
            .await;

        assert!(matches!(result, Err(TransportError::NotStarted)));
    }

    #[tokio::test]
    async fn test_discover_returns_empty() {
        let (tx, _rx) = packet_channel(100);
        let transport = UdpTransport::new(TransportId::new(1), None, make_config(0), tx);

        // Discovery returns empty until multicast/DNS-SD is implemented
        let peers = transport.discover().unwrap();
        assert!(peers.is_empty());
    }

    #[test]
    fn test_transport_type() {
        let (tx, _rx) = packet_channel(100);
        let transport = UdpTransport::new(TransportId::new(1), None, make_config(0), tx);

        assert_eq!(transport.transport_type().name, "udp");
        assert!(!transport.transport_type().connection_oriented);
        assert!(!transport.transport_type().reliable);
    }

    #[test]
    fn test_sync_methods_return_not_supported() {
        let (tx, _rx) = packet_channel(100);
        let mut transport = UdpTransport::new(TransportId::new(1), None, make_config(0), tx);

        assert!(matches!(
            transport.start(),
            Err(TransportError::NotSupported(_))
        ));
        assert!(matches!(
            transport.stop(),
            Err(TransportError::NotSupported(_))
        ));
        assert!(matches!(
            transport.send(&TransportAddr::from_string("test"), b"data"),
            Err(TransportError::NotSupported(_))
        ));
    }

    #[tokio::test]
    async fn test_resolve_socket_addr_ip() {
        let addr = TransportAddr::from_string("192.168.1.1:2121");
        let result = resolve_socket_addr(&addr).await.unwrap();
        assert_eq!(result.to_string(), "192.168.1.1:2121");
    }

    #[tokio::test]
    async fn test_resolve_socket_addr_invalid() {
        let invalid = TransportAddr::from_string("nonexistent.invalid:2121");
        assert!(resolve_socket_addr(&invalid).await.is_err());

        let binary = TransportAddr::new(vec![0xff, 0x80]);
        assert!(resolve_socket_addr(&binary).await.is_err());
    }

    #[tokio::test]
    async fn test_resolve_socket_addr_hostname() {
        let addr = TransportAddr::from_string("localhost:2121");
        let result = resolve_socket_addr(&addr).await.unwrap();
        // localhost should resolve to 127.0.0.1 or [::1]
        assert!(result.ip().is_loopback());
        assert_eq!(result.port(), 2121);
    }

    #[tokio::test]
    async fn test_congestion_reports_kernel_drops() {
        let (tx, _rx) = packet_channel(100);
        let transport = UdpTransport::new(TransportId::new(1), None, make_config(0), tx);

        // Before start, congestion should still report (from stats)
        let cong = transport.congestion();
        assert_eq!(cong.recv_drops, Some(0));
    }

    #[test]
    fn test_accept_connections_default_true() {
        let (tx, _rx) = packet_channel(100);
        let transport = UdpTransport::new(TransportId::new(1), None, make_config(0), tx);
        // Default UdpConfig has accept_connections unset → true.
        assert!(transport.accept_connections());
    }

    #[test]
    fn test_accept_connections_false_when_configured() {
        let (tx, _rx) = packet_channel(100);
        let transport = UdpTransport::new(
            TransportId::new(1),
            None,
            UdpConfig {
                bind_addr: Some("127.0.0.1:0".to_string()),
                accept_connections: Some(false),
                ..Default::default()
            },
            tx,
        );
        assert!(!transport.accept_connections());
    }

    #[test]
    fn test_accept_connections_forced_false_in_outbound_only() {
        let (tx, _rx) = packet_channel(100);
        let transport = UdpTransport::new(
            TransportId::new(1),
            None,
            UdpConfig {
                outbound_only: Some(true),
                accept_connections: Some(true), // explicit true; outbound_only wins
                ..Default::default()
            },
            tx,
        );
        assert!(!transport.accept_connections());
    }

    #[tokio::test]
    async fn test_outbound_only_binds_ephemeral() {
        // outbound_only=true must override bind_addr to 0.0.0.0:0 so the
        // kernel picks a source port and there is no listener on a known
        // port. The runtime should bind successfully even if `bind_addr`
        // is explicitly set in the config (a warn fires; not asserted
        // here).
        let (tx, _rx) = packet_channel(100);
        let mut transport = UdpTransport::new(
            TransportId::new(1),
            None,
            UdpConfig {
                bind_addr: Some("127.0.0.1:65535".to_string()),
                outbound_only: Some(true),
                ..Default::default()
            },
            tx,
        );

        transport.start_async().await.unwrap();
        let local = transport.local_addr().unwrap();
        // Ephemeral port: kernel-assigned, non-zero, never matches the
        // configured 65535 (since outbound_only ignored bind_addr).
        assert_ne!(local.port(), 65535);
        assert!(local.port() > 0);
        // Source IP picked by the kernel; v4 INADDR_ANY before binding,
        // resolves to 0.0.0.0 on the local end.
        assert!(local.ip().is_unspecified());
        transport.stop_async().await.unwrap();
    }

    #[tokio::test]
    async fn test_punch_probe_dropped() {
        let (tx_recv, mut rx_recv) = packet_channel(100);
        let (tx_send, _rx_send) = packet_channel(100);

        let mut t_recv = UdpTransport::new(TransportId::new(1), None, make_config(0), tx_recv);
        let mut t_send = UdpTransport::new(TransportId::new(2), None, make_config(0), tx_send);

        t_recv.start_async().await.unwrap();
        t_send.start_async().await.unwrap();

        let recv_addr = t_recv.local_addr().unwrap();
        let recv_addr_str = TransportAddr::from_string(&recv_addr.to_string());

        // Probe (PUNCH_MAGIC = "NPTC", be) followed by sequence + payload.
        let mut probe = vec![0u8; 16];
        probe[..4].copy_from_slice(&0x4E505443u32.to_be_bytes());
        t_send.send_async(&recv_addr_str, &probe).await.unwrap();

        // Ack (PUNCH_ACK_MAGIC = "NPTA", be).
        let mut ack = vec![0u8; 16];
        ack[..4].copy_from_slice(&0x4E505441u32.to_be_bytes());
        t_send.send_async(&recv_addr_str, &ack).await.unwrap();

        // A real (non-punch) packet must still arrive.
        let real = b"valid-fmp-frame";
        t_send.send_async(&recv_addr_str, real).await.unwrap();

        // First message read should be the real one — punch probe + ack
        // both filtered silently.
        let packet = timeout(Duration::from_secs(1), rx_recv.recv())
            .await
            .expect("timeout waiting for real packet")
            .expect("channel closed");
        assert_eq!(packet.data, real);

        // No further packets should be queued (probe + ack dropped).
        let no_more = timeout(Duration::from_millis(200), rx_recv.recv()).await;
        assert!(no_more.is_err(), "punch probe/ack leaked through filter");

        t_recv.stop_async().await.unwrap();
        t_send.stop_async().await.unwrap();
    }

    #[test]
    fn test_is_punch_packet_helper() {
        use crate::nostr::is_punch_packet;
        // PUNCH_MAGIC ("NPTC", be)
        assert!(is_punch_packet(&[0x4E, 0x50, 0x54, 0x43, 0xAA, 0xBB]));
        // PUNCH_ACK_MAGIC ("NPTA", be)
        assert!(is_punch_packet(&[0x4E, 0x50, 0x54, 0x41]));
        // Non-magic packet
        assert!(!is_punch_packet(&[0x01, 0x02, 0x03, 0x04]));
        // Too short
        assert!(!is_punch_packet(&[0x4E, 0x50, 0x54]));
        assert!(!is_punch_packet(&[]));
    }

    #[tokio::test]
    async fn test_send_recv_ip_string() {
        let (tx1, _rx1) = packet_channel(100);
        let (tx2, mut rx2) = packet_channel(100);

        let mut t1 = UdpTransport::new(TransportId::new(1), None, make_config(0), tx1);
        let mut t2 = UdpTransport::new(TransportId::new(2), None, make_config(0), tx2);

        t1.start_async().await.unwrap();
        t2.start_async().await.unwrap();

        let port2 = t2.local_addr().unwrap().port();

        // Send using IP string address
        let data = b"hello via ip string";
        let bytes_sent = t1
            .send_async(
                &TransportAddr::from_string(&format!("127.0.0.1:{}", port2)),
                data,
            )
            .await
            .unwrap();
        assert_eq!(bytes_sent, data.len());

        // Receive on t2
        let packet = timeout(Duration::from_secs(1), rx2.recv())
            .await
            .expect("timeout")
            .expect("channel closed");

        assert_eq!(packet.data, data);

        t1.stop_async().await.unwrap();
        t2.stop_async().await.unwrap();
    }

    /// Burst more than one datagram into the kernel queue before yielding to
    /// the receive loop, then assert all are delivered in arrival order. On
    /// Linux/macOS this exercises the recvmmsg / recvmsg_x batching path
    /// (multiple datagrams reaped per syscall); on other unix targets it
    /// degrades to N single-packet recvmsg calls and still must pass.
    #[tokio::test]
    async fn test_burst_recv_batch() {
        let (tx1, _rx1) = packet_channel(100);
        let (tx2, mut rx2) = packet_channel(100);

        let mut t1 = UdpTransport::new(TransportId::new(1), None, make_config(0), tx1);
        let mut t2 = UdpTransport::new(TransportId::new(2), None, make_config(0), tx2);

        t1.start_async().await.unwrap();
        t2.start_async().await.unwrap();

        let addr2 = TransportAddr::from_string(&t2.local_addr().unwrap().to_string());

        // Fire BURST datagrams back-to-back. Each carries its index in the
        // first 4 bytes so we can verify per-datagram boundaries (recvmsg_x
        // must not coalesce them).
        const BURST: u32 = 10;
        for i in 0..BURST {
            let mut payload = vec![0u8; 32];
            payload[..4].copy_from_slice(&i.to_be_bytes());
            payload[4..].fill(b'x');
            t1.send_async(&addr2, &payload).await.unwrap();
        }

        // Drain. Order must match send order (UDP loopback is in-order, and
        // recvmmsg/recvmsg_x preserve it across the batch).
        for expected in 0..BURST {
            let packet = timeout(Duration::from_secs(1), rx2.recv())
                .await
                .expect("timeout draining burst")
                .expect("channel closed");
            assert_eq!(packet.data.len(), 32);
            let got = u32::from_be_bytes(packet.data[..4].try_into().unwrap());
            assert_eq!(got, expected, "datagram out of order");
        }

        t1.stop_async().await.unwrap();
        t2.stop_async().await.unwrap();
    }
}
