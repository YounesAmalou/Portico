//! Windows sockets: probing whether a local address can be listened on.

use std::{io, mem::size_of, net::SocketAddr};

use windows_sys::Win32::Networking::WinSock::{
    AF_INET, AF_INET6, INVALID_SOCKET, IPPROTO_IPV6, IPPROTO_TCP, IPV6_V6ONLY, SO_EXCLUSIVEADDRUSE,
    SOCK_STREAM, SOCKADDR_IN, SOCKADDR_IN6, SOCKET, SOL_SOCKET, WSACleanup, WSADATA,
    WSAGetLastError, WSAStartup, bind, closesocket, setsockopt, socket,
};

/// Binds `address` and closes it again: with `SO_EXCLUSIVEADDRUSE`, which refuses an address
/// anyone else listens on (Windows otherwise lets some sockets share it), and an IPv6 address
/// with `IPV6_V6ONLY`, as ssh listens.
pub fn bind_probe(address: SocketAddr) -> io::Result<()> {
    let _winsock = Winsock::start()?;
    let one: i32 = 1;
    let mut options = vec![(SOL_SOCKET, SO_EXCLUSIVEADDRUSE)];
    let family = match address {
        SocketAddr::V4(_) => AF_INET,
        SocketAddr::V6(_) => {
            options.push((IPPROTO_IPV6, IPV6_V6ONLY));
            AF_INET6
        }
    };
    // SAFETY: `socket` has no preconditions; a valid socket is owned at once.
    let socket = Socket(unsafe { socket(i32::from(family), SOCK_STREAM, IPPROTO_TCP) });
    if socket.0 == INVALID_SOCKET {
        return Err(last_error());
    }
    // SAFETY: plain socket calls on a socket owned here, with properly sized socket addresses
    // and option values that outlive each call.
    unsafe {
        for (level, name) in options {
            if setsockopt(
                socket.0,
                level,
                name,
                (&raw const one).cast(),
                size_of::<i32>() as i32,
            ) != 0
            {
                return Err(last_error());
            }
        }
        let bound = match address {
            SocketAddr::V4(address) => {
                let mut raw = SOCKADDR_IN {
                    sin_family: AF_INET,
                    sin_port: address.port().to_be(),
                    ..Default::default()
                };
                raw.sin_addr.S_un.S_addr = u32::from(*address.ip()).to_be();
                bind(
                    socket.0,
                    (&raw const raw).cast(),
                    size_of::<SOCKADDR_IN>() as i32,
                )
            }
            SocketAddr::V6(address) => {
                let mut raw = SOCKADDR_IN6 {
                    sin6_family: AF_INET6,
                    sin6_port: address.port().to_be(),
                    ..Default::default()
                };
                raw.sin6_addr.u.Byte = address.ip().octets();
                raw.Anonymous.sin6_scope_id = address.scope_id();
                bind(
                    socket.0,
                    (&raw const raw).cast(),
                    size_of::<SOCKADDR_IN6>() as i32,
                )
            }
        };
        if bound != 0 {
            return Err(last_error());
        }
    }
    Ok(())
}

/// The last Windows Sockets error of this thread.
fn last_error() -> io::Error {
    // SAFETY: `WSAGetLastError` has no preconditions.
    io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}

/// Windows Sockets, started for as long as this lives (starts are counted, so the standard
/// library's own start is not disturbed).
struct Winsock;

impl Winsock {
    fn start() -> io::Result<Self> {
        let mut data = WSADATA::default();
        // SAFETY: `WSAStartup` fills the `WSADATA` it is given.
        match unsafe { WSAStartup(0x0202, &mut data) } {
            0 => Ok(Self),
            code => Err(io::Error::from_raw_os_error(code)),
        }
    }
}

impl Drop for Winsock {
    fn drop(&mut self) {
        // SAFETY: balances the successful `WSAStartup` of `start`.
        unsafe { WSACleanup() };
    }
}

/// A socket closed when dropped (before Windows Sockets is cleaned up, being declared after).
struct Socket(SOCKET);

impl Drop for Socket {
    fn drop(&mut self) {
        if self.0 != INVALID_SOCKET {
            // SAFETY: the socket is owned here and not used again.
            unsafe { closesocket(self.0) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_probe_sees_a_listening_port_as_busy() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let busy = listener.local_addr().unwrap();
        assert!(bind_probe(busy).is_err());
        assert!(bind_probe("127.0.0.1:0".parse().unwrap()).is_ok());
    }
}
