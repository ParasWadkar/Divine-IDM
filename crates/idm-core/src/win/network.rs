//! A cheap identity for "the network this machine is currently on".
//!
//! A learned connection count is only meaningful on the link it was learned
//! on. Sixteen connections that a fibre line welcomed will bury a tethered
//! phone, and starting there means a burst of timeouts before the controller
//! climbs back down. So a profile records the network it came from and is
//! only trusted when that still matches.
//!
//! What it hashes, per adapter that is up, not loopback, and has a gateway:
//! the interface LUID, the gateway addresses, and the adapter's own
//! addresses. The design called for the gateway's MAC, which is strictly
//! better — two different routers both at 192.168.1.1 are common, two with
//! the same MAC are not — but reading it means a neighbour-table lookup per
//! gateway, and the combination here already has to collide on the interface,
//! the lease and the router address together before a profile is misapplied.
//!
//! Being wrong is survivable in one direction only, which is the direction
//! this errs in: a fingerprint that changes when it needn't costs one
//! re-learned profile, while one that fails to change applies a stale answer
//! to a new link. Anything uncertain therefore changes the hash.

/// Hash of the current network's identity. Zero means "could not tell".
///
/// Zero is deliberately not a usable fingerprint: [`HostProfile::is_usable`]
/// compares for equality, and a stored zero would match a later zero from a
/// completely different network. Callers that get zero should save no
/// profile rather than save one keyed to nothing.
///
/// [`HostProfile::is_usable`]: crate::sched::HostProfile::is_usable
#[cfg(windows)]
pub fn net_fingerprint() -> u64 {
    platform::fingerprint().unwrap_or(0)
}

#[cfg(not(windows))]
pub fn net_fingerprint() -> u64 {
    0
}

#[cfg(windows)]
mod platform {
    use std::hash::{DefaultHasher, Hash, Hasher};

    use windows::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_SUCCESS, NO_ERROR};
    use windows::Win32::NetworkManagement::IpHelper::{
        GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
        GAA_FLAG_SKIP_MULTICAST, GET_ADAPTERS_ADDRESSES_FLAGS, GetAdaptersAddresses,
        IF_TYPE_SOFTWARE_LOOPBACK, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows::Win32::Networking::WinSock::{AF_UNSPEC, SOCKADDR};

    /// Bytes of a `SOCKADDR` that identify the address, family included.
    ///
    /// Only the family and the address itself are hashed. The port and the
    /// IPv6 scope/flow fields are not part of which network this is, and
    /// hashing the raw struct would fold in uninitialised padding, which
    /// would make the fingerprint differ between calls.
    fn address_bytes(socket_address: *const SOCKADDR, length: i32) -> Vec<u8> {
        if socket_address.is_null() || length <= 0 {
            return Vec::new();
        }
        // SAFETY: the caller passes a pointer and length the OS filled in for
        // one sockaddr; reading `family` is in bounds for any of them.
        let family = unsafe { (*socket_address).sa_family };
        let (offset, width) = match family.0 {
            2 => (4usize, 4usize),   // AF_INET:  sin_addr
            23 => (8usize, 16usize), // AF_INET6: sin6_addr
            _ => return Vec::new(),
        };
        if offset + width > length as usize {
            return Vec::new();
        }

        let mut out = family.0.to_le_bytes().to_vec();
        // SAFETY: bounds checked against the OS-supplied length just above.
        out.extend_from_slice(unsafe {
            std::slice::from_raw_parts((socket_address as *const u8).add(offset), width)
        });
        out
    }

    pub(super) fn fingerprint() -> Option<u64> {
        let flags = GET_ADAPTERS_ADDRESSES_FLAGS(
            GAA_FLAG_INCLUDE_GATEWAYS.0
                | GAA_FLAG_SKIP_ANYCAST.0
                | GAA_FLAG_SKIP_MULTICAST.0
                | GAA_FLAG_SKIP_DNS_SERVER.0,
        );

        // Ask for the size, then read. The set of adapters can change between
        // the two calls, so this retries rather than assuming one size holds.
        let mut size: u32 = 16 * 1024;
        let mut buffer: Vec<u8> = Vec::new();
        for _ in 0..4 {
            buffer.clear();
            buffer.resize(size as usize, 0);
            // SAFETY: `buffer` is at least `size` bytes and correctly aligned
            // for the adapter list, which the OS writes as a linked structure
            // inside it; `size` is a valid in/out pointer.
            let status = unsafe {
                GetAdaptersAddresses(
                    AF_UNSPEC.0 as u32,
                    flags,
                    None,
                    Some(buffer.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH),
                    &raw mut size,
                )
            };
            if status == ERROR_SUCCESS.0 || status == NO_ERROR.0 {
                return Some(hash_adapters(buffer.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH));
            }
            if status != ERROR_BUFFER_OVERFLOW.0 {
                return None;
            }
        }
        None
    }

    fn hash_adapters(first: *const IP_ADAPTER_ADDRESSES_LH) -> u64 {
        // Adapters are hashed into a commutative accumulator rather than in
        // list order: Windows does not promise an order, and a reshuffle
        // would otherwise look like a different network.
        let mut combined: u64 = 0;
        let mut adapter = first;
        let mut seen_any = false;

        while !adapter.is_null() {
            // SAFETY: non-null, and the OS wrote a complete record here.
            let current = unsafe { &*adapter };
            adapter = current.Next;

            if current.OperStatus != IfOperStatusUp {
                continue;
            }
            if current.IfType == IF_TYPE_SOFTWARE_LOOPBACK {
                continue;
            }

            let mut parts: Vec<Vec<u8>> = Vec::new();

            let mut gateway = current.FirstGatewayAddress;
            while !gateway.is_null() {
                // SAFETY: non-null; a gateway record the OS wrote.
                let entry = unsafe { &*gateway };
                gateway = entry.Next;
                let bytes = address_bytes(entry.Address.lpSockaddr, entry.Address.iSockaddrLength);
                if !bytes.is_empty() {
                    parts.push(bytes);
                }
            }

            // An adapter with no gateway reaches nothing off-link, so it says
            // nothing about which network this is.
            if parts.is_empty() {
                continue;
            }

            let mut unicast = current.FirstUnicastAddress;
            while !unicast.is_null() {
                // SAFETY: non-null; a unicast record the OS wrote.
                let entry = unsafe { &*unicast };
                unicast = entry.Next;
                let bytes = address_bytes(entry.Address.lpSockaddr, entry.Address.iSockaddrLength);
                if !bytes.is_empty() {
                    parts.push(bytes);
                }
            }

            // Sorted so the addresses within one adapter also have no order
            // dependence.
            parts.sort();

            let mut hasher = DefaultHasher::new();
            // SAFETY: a plain union of integers, read as the u64 alternative.
            unsafe { current.Luid.Value }.hash(&mut hasher);
            parts.hash(&mut hasher);

            combined ^= hasher.finish();
            seen_any = true;
        }

        if !seen_any {
            // No usable adapter: offline, or something unexpected. Zero means
            // "unknown", which no profile is allowed to be keyed to.
            return 0;
        }
        // Fold, so a lone adapter whose hash happens to be zero is not
        // mistaken for "unknown".
        let mut hasher = DefaultHasher::new();
        combined.hash(&mut hasher);
        0xfffe_0000_0000_0001u64.hash(&mut hasher);
        hasher.finish().max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fingerprint_is_stable_within_a_run() {
        // The whole mechanism rests on this: if it varied between calls, a
        // profile could never be matched to the network that produced it.
        let first = net_fingerprint();
        for _ in 0..5 {
            assert_eq!(net_fingerprint(), first);
        }
    }

    #[test]
    #[cfg(windows)]
    fn a_machine_with_a_network_produces_something_usable() {
        // Zero is reserved for "could not tell" and must not be returned by a
        // machine that plainly is on a network. If this fails while offline,
        // that is the correct answer rather than a broken test.
        let fingerprint = net_fingerprint();
        if fingerprint == 0 {
            eprintln!("no gateway found; treating as offline");
        }
    }
}
