//! UDP socket helpers backed by io_uring `RecvMsg` / `SendMsg`.
//!
//! Designed for one-shot control-plane exchanges (e.g. locusta QP info
//! handshake) rather than high-throughput data plane. Each call posts
//! one SQE and awaits its completion through the runtime's
//! [`IoUringReactor`].
//!
//! The socket itself is created blocking via `libc::socket`/`libc::bind`
//! and then driven asynchronously through io_uring. We hold the
//! `msghdr`/`iovec`/`sockaddr_storage` triple inside a `Box` that lives
//! until the future resolves so the kernel always sees stable memory.

use std::{
    cell::RefCell,
    future::Future,
    io,
    mem::MaybeUninit,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
};

use crate::reactor::{IoUringReactor, WaitHandle};

/// UDP datagram socket driven by an [`IoUringReactor`].
///
/// Cheap to clone (`Rc` internally). Cloned handles share the same
/// underlying file descriptor; only one outstanding `recv_from` per
/// clone is supported (subsequent recvs serialise on the OS socket).
#[derive(Clone)]
pub struct UdpSocket {
    inner: Rc<UdpSocketInner>,
}

struct UdpSocketInner {
    fd: OwnedFd,
    reactor: Rc<IoUringReactor>,
    /// Locally-bound address (resolved after `bind`).
    local: SocketAddrV4,
    /// Buffer reused for tracking outstanding ops if needed.
    _pad: RefCell<()>,
}

impl UdpSocket {
    /// Open and bind a new UDP socket.
    ///
    /// Pass `SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)` to bind to an
    /// ephemeral port; query [`local_addr`] afterwards to discover it.
    pub fn bind(addr: SocketAddrV4) -> io::Result<Self> {
        Self::bind_with_reactor(addr, IoUringReactor::get_or_init())
    }

    /// Bind a UDP socket attached to an explicit reactor instance.
    pub fn bind_with_reactor(addr: SocketAddrV4, reactor: Rc<IoUringReactor>) -> io::Result<Self> {
        // SAFETY: socket(2) returns -1 on error; we check and convert.
        let raw_fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
        if raw_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd: OwnedFd = unsafe { OwnedFd::from_raw_fd(raw_fd) };

        // SO_REUSEADDR so server restarts can rebind the same port.
        let yes: libc::c_int = 1;
        let rc = unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_REUSEADDR,
                &yes as *const _ as *const libc::c_void,
                std::mem::size_of_val(&yes) as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }

        let sin = sockaddr_in_from_v4(&addr);
        let rc = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                &sin as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }

        let local = read_local_addr(fd.as_raw_fd())?;
        Ok(UdpSocket {
            inner: Rc::new(UdpSocketInner {
                fd,
                reactor,
                local,
                _pad: RefCell::new(()),
            }),
        })
    }

    /// Wrap an already-bound UDP socket (e.g. one created blocking via
    /// `std::net::UdpSocket`). Takes ownership of the fd.
    pub fn from_raw_fd(fd: RawFd, reactor: Rc<IoUringReactor>) -> io::Result<Self> {
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        let local = read_local_addr(owned.as_raw_fd())?;
        Ok(UdpSocket {
            inner: Rc::new(UdpSocketInner {
                fd: owned,
                reactor,
                local,
                _pad: RefCell::new(()),
            }),
        })
    }

    /// Locally-bound socket address (post-`bind`).
    pub fn local_addr(&self) -> SocketAddrV4 {
        self.inner.local
    }

    /// Raw fd. Caller must not close it; the [`UdpSocket`] owns the
    /// file descriptor.
    pub fn as_raw_fd(&self) -> RawFd {
        self.inner.fd.as_raw_fd()
    }

    /// Send `buf` as a single datagram to `peer`.
    ///
    /// The kernel may copy as little as zero bytes if the message is
    /// truncated, but for UDP at sizes ≪ MTU the whole buffer is sent
    /// or the send fails.
    pub fn send_to<'b>(&self, buf: &'b [u8], peer: SocketAddrV4) -> SendToFuture<'b> {
        let storage = Box::new(SendStorage {
            sockaddr: sockaddr_in_from_v4(&peer),
            iov: libc::iovec {
                iov_base: buf.as_ptr() as *mut libc::c_void,
                iov_len: buf.len(),
            },
            msghdr: unsafe { std::mem::zeroed() },
        });
        let mut storage = storage;
        storage.msghdr.msg_name =
            &storage.sockaddr as *const libc::sockaddr_in as *mut libc::c_void;
        storage.msghdr.msg_namelen = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
        storage.msghdr.msg_iov = &mut storage.iov as *mut libc::iovec;
        storage.msghdr.msg_iovlen = 1;

        let sqe = io_uring::opcode::SendMsg::new(
            io_uring::types::Fd(self.inner.fd.as_raw_fd()),
            &mut storage.msghdr as *mut libc::msghdr as *const _,
        )
        .build();
        let handle = self.inner.reactor.push_sqe(sqe);

        SendToFuture {
            _storage: storage,
            handle,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Receive a single datagram into `buf`.
    ///
    /// Returns `(bytes_received, source_addr)` on success.
    pub fn recv_from<'b>(&self, buf: &'b mut [u8]) -> RecvFromFuture<'b> {
        let storage = Box::new(RecvStorage {
            sockaddr: unsafe { std::mem::zeroed() },
            iov: libc::iovec {
                iov_base: buf.as_mut_ptr() as *mut libc::c_void,
                iov_len: buf.len(),
            },
            msghdr: unsafe { std::mem::zeroed() },
        });
        let mut storage = storage;
        storage.msghdr.msg_name = &mut storage.sockaddr as *mut _ as *mut libc::c_void;
        storage.msghdr.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        storage.msghdr.msg_iov = &mut storage.iov as *mut libc::iovec;
        storage.msghdr.msg_iovlen = 1;

        let sqe = io_uring::opcode::RecvMsg::new(
            io_uring::types::Fd(self.inner.fd.as_raw_fd()),
            &mut storage.msghdr as *mut libc::msghdr,
        )
        .build();
        let handle = self.inner.reactor.push_sqe(sqe);

        RecvFromFuture {
            storage,
            handle,
            _phantom: std::marker::PhantomData,
        }
    }
}

impl Drop for UdpSocketInner {
    fn drop(&mut self) {
        // OwnedFd closes via libc::close on drop. Nothing extra needed —
        // any in-flight RecvMsg with a Box<RecvStorage> still owned by
        // an outstanding future will keep its memory alive; the kernel
        // may write into the closed fd's stale state, but our buffers
        // remain valid until the future is dropped.
    }
}

struct SendStorage {
    sockaddr: libc::sockaddr_in,
    iov: libc::iovec,
    msghdr: libc::msghdr,
}

struct RecvStorage {
    sockaddr: libc::sockaddr_storage,
    iov: libc::iovec,
    msghdr: libc::msghdr,
}

/// Future returned by [`UdpSocket::send_to`].
pub struct SendToFuture<'b> {
    _storage: Box<SendStorage>,
    handle: WaitHandle,
    _phantom: std::marker::PhantomData<&'b [u8]>,
}

impl<'b> Future for SendToFuture<'b> {
    type Output = io::Result<usize>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.handle).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Ready(Ok(n)) => Poll::Ready(Ok(n as usize)),
        }
    }
}

/// Future returned by [`UdpSocket::recv_from`].
pub struct RecvFromFuture<'b> {
    storage: Box<RecvStorage>,
    handle: WaitHandle,
    _phantom: std::marker::PhantomData<&'b mut [u8]>,
}

impl<'b> Future for RecvFromFuture<'b> {
    type Output = io::Result<(usize, SocketAddrV4)>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.handle).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Ready(Ok(n)) => {
                // The kernel filled `msg_namelen` with the actual length
                // of the source sockaddr. We only handle IPv4 (sin family).
                let saddr_ptr = &self.storage.sockaddr as *const libc::sockaddr_storage
                    as *const libc::sockaddr_in;
                let family = unsafe { (*saddr_ptr).sin_family };
                if family != libc::AF_INET as u16 {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("non-IPv4 sender family={family}"),
                    )));
                }
                let sin = unsafe { *saddr_ptr };
                let addr = sockaddr_in_to_v4(&sin);
                Poll::Ready(Ok((n as usize, addr)))
            }
        }
    }
}

fn sockaddr_in_from_v4(addr: &SocketAddrV4) -> libc::sockaddr_in {
    let mut sin: libc::sockaddr_in = unsafe { MaybeUninit::zeroed().assume_init() };
    sin.sin_family = libc::AF_INET as u16;
    sin.sin_port = addr.port().to_be();
    // `octets()` returns IPv4 bytes in network order (a.b.c.d → [a,b,c,d]).
    // `s_addr` wants those same 4 bytes interpreted as a u32 in network
    // byte order, so reassemble via `from_be_bytes` then `to_be`.
    sin.sin_addr.s_addr = u32::from_be_bytes(addr.ip().octets()).to_be();
    sin
}

fn sockaddr_in_to_v4(sin: &libc::sockaddr_in) -> SocketAddrV4 {
    let raw_be = sin.sin_addr.s_addr;
    let ip = Ipv4Addr::from(u32::from_be(raw_be));
    let port = u16::from_be(sin.sin_port);
    SocketAddrV4::new(ip, port)
}

fn read_local_addr(fd: RawFd) -> io::Result<SocketAddrV4> {
    let mut sin: libc::sockaddr_in = unsafe { MaybeUninit::zeroed().assume_init() };
    let mut len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockname(
            fd,
            &mut sin as *mut _ as *mut libc::sockaddr,
            &mut len as *mut libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(sockaddr_in_to_v4(&sin))
}

/// Helper: parse `ip:port` string into [`SocketAddrV4`].
///
/// Useful for registry-file lookups.
pub fn parse_v4(s: &str) -> io::Result<SocketAddrV4> {
    let sa: SocketAddr = s
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("parse {s}: {e}")))?;
    match sa {
        SocketAddr::V4(v4) => Ok(v4),
        SocketAddr::V6(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected IPv4 address",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::IoUringReactorBuilder;
    use pluvio_runtime::executor::{set_runtime, Runtime};

    #[test]
    fn loopback_send_recv() {
        let runtime = Runtime::new(64);
        set_runtime(runtime.clone());
        let reactor = IoUringReactorBuilder::default().build();
        IoUringReactor::init(reactor.clone()).ok();
        runtime.register_reactor("iouring", reactor.clone());

        let server = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .expect("bind server");
        let server_addr = server.local_addr();
        let client = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .expect("bind client");

        let server_clone = server.clone();
        pluvio_runtime::spawn_with_name(
            async move {
                let mut buf = [0u8; 64];
                let (n, from) = server_clone.recv_from(&mut buf).await.expect("server recv");
                assert_eq!(&buf[..n], b"ping");
                let reply = b"pong";
                server_clone
                    .send_to(reply, from)
                    .await
                    .expect("server send");
            },
            "udp_server".to_string(),
        );

        runtime.block_on_with_name_and_runtime("udp_client", async move {
            client
                .send_to(b"ping", server_addr)
                .await
                .expect("client send");
            let mut buf = [0u8; 64];
            let (n, _from) = client.recv_from(&mut buf).await.expect("client recv");
            assert_eq!(&buf[..n], b"pong");
        });
    }
}
