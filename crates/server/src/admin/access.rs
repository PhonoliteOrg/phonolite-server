//! The active admin listener determines valid request hosts until restart.
use std::net::{IpAddr, SocketAddr};

#[derive(Clone, Copy, Debug)]
pub(crate) struct AdminAccess {
    address: SocketAddr,
}

impl AdminAccess {
    pub(crate) fn new(address: SocketAddr) -> Self {
        Self { address }
    }

    pub(crate) fn allows(&self, host: &str, port: u16) -> bool {
        if port != self.address.port() {
            return false;
        }
        let bind_ip = canonical_ip(self.address.ip());
        if host == "localhost" {
            return bind_ip.is_loopback() || bind_ip.is_unspecified();
        }
        let host = host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(host);
        let Ok(ip) = host.parse::<IpAddr>() else {
            // Do not accept arbitrary DNS names: an attacker could rebind one
            // to this listener while keeping malicious content on its origin.
            return false;
        };
        let ip = canonical_ip(ip);
        if ip.is_unspecified() || ip.is_multicast() {
            return false;
        }
        if bind_ip.is_unspecified() {
            // An IPv6 wildcard may also accept IPv4 through a dual-stack socket.
            return bind_ip.is_ipv6() || ip.is_ipv4();
        }
        ip == bind_ip
    }
}

fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(ip) => ip
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(ip)),
        ip => ip,
    }
}
