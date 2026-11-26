//! Unix sockets (Linux and macOS): probing whether a local address can be listened on.

use std::{io, net::SocketAddr, os::fd::AsRawFd};

use super::fs::cloexec_socket;

/// Binds `address` and closes it again: an IPv6 address with `IPV6_V6ONLY`, as ssh listens
/// (the standard library's listener is dual-stack and would also collide with IPv4
/// listeners), and `SO_REUSEADDR` on Linux only (see `try_bind`).
pub fn bind_probe(address: SocketAddr) -> io::Result<()> {
    let one: libc::c_int = 1;
    let mut options = Vec::new();
    let domain = match address {
        SocketAddr::V4(_) => libc::AF_INET,
        SocketAddr::V6(_) => {
            options.push((libc::IPPROTO_IPV6, libc::IPV6_V6ONLY));
            libc::AF_INET6
        }
    };
    if cfg!(target_os = "linux") {
        options.push((libc::SOL_SOCKET, libc::SO_REUSEADDR));
    }
    // SAFETY: plain socket calls on a descriptor owned here, with properly sized socket
    // addresses and option values that outlive each call.
    unsafe {
        let socket = cloexec_socket(domain, libc::SOCK_STREAM)?;
        for option in options {
            if libc::setsockopt(
                socket.as_raw_fd(),
                option.0,
                option.1,
                (&raw const one).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            ) != 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        let bound = match address {
            SocketAddr::V4(address) => {
                let mut raw: libc::sockaddr_in = std::mem::zeroed();
                raw.sin_family = libc::AF_INET as libc::sa_family_t;
                raw.sin_port = address.port().to_be();
                raw.sin_addr.s_addr = u32::from(*address.ip()).to_be();
                libc::bind(
                    socket.as_raw_fd(),
                    (&raw const raw).cast(),
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                )
            }
            SocketAddr::V6(address) => {
                let mut raw: libc::sockaddr_in6 = std::mem::zeroed();
                raw.sin6_family = libc::AF_INET6 as libc::sa_family_t;
                raw.sin6_port = address.port().to_be();
                raw.sin6_addr.s6_addr = address.ip().octets();
                raw.sin6_scope_id = address.scope_id();
                libc::bind(
                    socket.as_raw_fd(),
                    (&raw const raw).cast(),
                    std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                )
            }
        };
        if bound != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
