//! Connection lifecycle rules shared by the stream transports.
//!
//! TCP and the SOCKS5-proxied Tor and Nym transports each give a pooled
//! connection its own writer task and receive loop, and either loop can
//! outlive the pool entry it was created with. The rules for when such a loop
//! may touch the pool are written once here.

use std::collections::HashMap;

use portable_atomic::{AtomicU64, Ordering};

use crate::transport::TransportAddr;

/// Identity of one pooled stream connection.
///
/// The pool is keyed by address, and a newer connection can take an address
/// while an older connection's writer or receive loop is still running. The
/// id tells the two apart.
pub(crate) type ConnId = u64;

/// Source of connection ids. Process-wide rather than per transport, because
/// the accept loops that build connections are free functions with no
/// transport instance to hold a counter.
static NEXT_CONN_ID: AtomicU64 = AtomicU64::new(1);

/// Hand out an id no other connection in this process has had.
pub(crate) fn next_conn_id() -> ConnId {
    NEXT_CONN_ID.fetch_add(1, Ordering::Relaxed)
}

/// A pooled stream connection that knows its own [`ConnId`].
pub(crate) trait PooledConn {
    /// The id this connection's writer and receive loop were given.
    fn conn_id(&self) -> ConnId;
}

/// Remove the entry at `addr`, but only if it is connection `id`.
///
/// This is the only way a connection's own writer or receive loop removes a
/// pool entry. An entry with another id belongs to a newer connection at the
/// same address, and is left alone.
pub(crate) fn remove_own<C: PooledConn>(
    pool: &mut HashMap<TransportAddr, C>,
    addr: &TransportAddr,
    id: ConnId,
) -> Option<C> {
    if pool.get(addr)?.conn_id() != id {
        return None;
    }
    pool.remove(addr)
}
