//! Socket-protect hook for embedders.
//!
//! Android's `VpnService` routes everything matching the tunnel's routes back
//! into the tunnel — including the daemon's own underlay sockets, unless each
//! one is excluded via `VpnService.protect(fd)`. An embedder installs a
//! [`SocketProtect`] callback with [`crate::Node::set_socket_protect`] before
//! `start()`; the node then invokes it with the raw handle of every underlay
//! socket it creates (UDP listen/adopted/connected-peer sockets, TCP
//! listener/accepted/dialed sockets, Nostr STUN and hole-punch sockets)
//! after creation and before any traffic is sent on it.
//!
//! Not covered (no fd access): the Nostr relay websockets owned by
//! `nostr-sdk`, mDNS sockets owned by `mdns-sd`, and the SOCKS5 dialer used
//! by Tor/Nym (on Android that proxy is a local process, e.g. Orbot, which
//! protects its own sockets).

use std::sync::Arc;

/// Raw socket handle passed to [`SocketProtect`]: the fd on Unix, the
/// `SOCKET` on Windows.
#[cfg(unix)]
pub type RawSocketHandle = std::os::unix::io::RawFd;
/// Raw socket handle passed to [`SocketProtect`]: the fd on Unix, the
/// `SOCKET` on Windows.
#[cfg(windows)]
pub type RawSocketHandle = std::os::windows::io::RawSocket;

/// Callback invoked with every underlay socket's raw handle right after the
/// socket is created, before any traffic is sent on it. May be called from
/// async tasks and from dedicated OS threads; must not block for long and
/// must tolerate being called more than once for the same handle (adopted
/// sockets are re-announced by the adopting transport).
pub type SocketProtect = Arc<dyn Fn(RawSocketHandle) + Send + Sync>;

/// Platform-neutral raw-handle accessor so call sites need no `cfg`.
pub(crate) trait AsRawSocketHandle {
    fn raw_socket_handle(&self) -> RawSocketHandle;
}

#[cfg(unix)]
impl<T: std::os::unix::io::AsRawFd> AsRawSocketHandle for T {
    fn raw_socket_handle(&self) -> RawSocketHandle {
        self.as_raw_fd()
    }
}

#[cfg(windows)]
impl<T: std::os::windows::io::AsRawSocket> AsRawSocketHandle for T {
    fn raw_socket_handle(&self) -> RawSocketHandle {
        self.as_raw_socket()
    }
}

/// Invoke `hook` (when installed) with `socket`'s raw handle.
pub(crate) fn apply_socket_protect<S: AsRawSocketHandle>(hook: Option<&SocketProtect>, socket: &S) {
    if let Some(hook) = hook {
        hook(socket.raw_socket_handle());
    }
}
