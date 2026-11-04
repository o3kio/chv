//! The G5 vsock transport for the guest-side monitoring agent
//! (prompt-06 task 4): lets the agent speak its UNCHANGED TLS/HTTP
//! stack to the manager over the VM's vsock device instead of the
//! management network.
//!
//! Design (D1/D1a, ADR-026, `docs/specs/contracts/chv-monitoring-vsock-v1.md`):
//!
//! - The VM's vsock device exists only when the deployment opts the VM
//!   in (`vsock_enabled`); the guest agent opts into USING it with
//!   `[transport] type = "vsock"`.
//! - The transport is a local loopback forwarder: a TCP listener on
//!   `127.0.0.1` at the `server_url`'s port (the HTTP client's
//!   resolver override redirects the host to 127.0.0.1 but keeps the
//!   URL's port, so the forwarder must own exactly that port — which
//!   is why vsock mode requires an explicit unprivileged port in
//!   `server_url`) whose connections are piped byte-for-byte into an
//!   AF_VSOCK stream to the host (CID 2, [`VSOCK_MONITOR_PORT`]). The
//!   HTTP client is redirected onto it with reqwest's `.resolve()`
//!   override, so the DIAL goes to the forwarder while TLS SNI and
//!   certificate validation keep using the `server_url` host — the
//!   guest runs the exact same TLS/HTTP stack over vsock as over TCP,
//!   and enroll/ingest/rotate all ride the pipe unchanged.
//! - On the host, chv-agent's per-VM relay accepts the VMM's connect
//!   and pipes the same bytes to the control plane's mTLS listener.
//!   End-to-end the TLS session is guest↔control-plane; neither the
//!   forwarder nor the relay ever parses it. The vsock CID and the
//!   loopback port are transport plumbing, never authentication
//!   (ADR-026) — the claim and the client certificate remain the only
//!   credentials.
//!
//! No vsock-specific dependency: the connector is built on `libc`
//! (already a dependency for the plugin sandbox) plus `tokio`'s
//! family-agnostic fd readiness (`AsyncFd`).

use std::io;
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// The fixed monitoring contract port over vsock. Mirror of
/// `chv_hypervisor_api::VSOCK_MONITOR_PORT` (the guest package cannot
/// depend on the host's hypervisor API crate); the G5 rig asserts the
/// two sides agree at runtime, and the contract doc pins the value.
pub const VSOCK_MONITOR_PORT: u32 = 1024;

/// The host's well-known vsock CID (Linux `VMADDR_CID_HOST`, re-exported
/// so it cannot drift from libc): the guest's connection to CID 2 lands
/// on the host-side Unix socket the node's relay listens on.
pub const VMADDR_CID_HOST: u32 = libc::VMADDR_CID_HOST;

/// How long a guest→host vsock connect may take before the forwarder
/// gives up on it (closing the loopback side so the client retries).
/// Real connects complete in microseconds; this bound exists so a
/// wedged connect cannot park per-connection tasks indefinitely.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Extract the host and port the agent's `server_url` designates
/// (defaulting to 443 for a portless https URL). The loopback
/// forwarder MUST listen on exactly this port: the HTTP client's
/// resolver override redirects the host to 127.0.0.1 but the dial
/// keeps the URL's port, so the forwarder has to own it. An explicit
/// unprivileged port in `server_url` is therefore a vsock-transport
/// requirement (the agent runs as a non-root service and cannot bind
/// 443 on 127.0.0.1).
pub fn server_host_port(server_url: &str) -> Option<(String, u16)> {
    let rest = server_url.strip_prefix("https://")?;
    let authority = rest.split('/').next()?;
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().ok()?),
        None => (authority, 443),
    };
    if host.is_empty() {
        None
    } else {
        Some((host.to_string(), port))
    }
}

/// Extract the host component of the agent's `server_url` — the name
/// the reqwest `.resolve()` override must match. The dial is redirected
/// to the local forwarder; TLS SNI and certificate validation keep
/// using this host (D1a: `server_url` keeps its TLS/SNI semantics).
pub fn server_host(server_url: &str) -> Option<String> {
    server_host_port(server_url).map(|(host, _)| host)
}

/// A connected AF_VSOCK stream. Readiness goes through `AsyncFd`
/// (epoll is family-agnostic); the data plane is raw `read(2)`/`write(2)`
/// on the socket fd.
struct VsockStream {
    fd: AsyncFd<OwnedFd>,
    /// Bytes read from the socket that did not fit the caller's
    /// `ReadBuf` (stable `ReadBuf` cannot be written into its
    /// uninitialized tail, so reads go through a scratch buffer).
    /// Almost always empty; bounded by the scratch size.
    pending: Vec<u8>,
}

impl VsockStream {
    /// Connect to `cid`:`port` without blocking the runtime: the socket
    /// is non-blocking from creation, `connect(2)`'s `EINPROGRESS` is
    /// awaited through the reactor, and completion is verified with
    /// `SO_ERROR` (a failed async connect would otherwise surface as a
    /// confusing error on the first write).
    async fn connect(cid: u32, port: u32) -> io::Result<Self> {
        // SAFETY: every libc call below is given a valid fd produced by
        // the preceding call; the fd is wrapped in OwnedFd exactly once,
        // on the success path only (error paths close it first).
        let fd = unsafe {
            let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // Non-blocking from the start: writable()/readable() drive
            // the reactor's edge-triggered registration.
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags < 0 {
                let err = io::Error::last_os_error();
                libc::close(fd);
                return Err(err);
            }
            if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                let err = io::Error::last_os_error();
                libc::close(fd);
                return Err(err);
            }
            let mut addr: libc::sockaddr_vm = std::mem::zeroed();
            addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
            addr.svm_cid = cid;
            addr.svm_port = port;
            let rc = libc::connect(
                fd,
                &addr as *const libc::sockaddr_vm as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
            );
            if rc != 0 {
                let err = io::Error::last_os_error();
                if err.raw_os_error() != Some(libc::EINPROGRESS) {
                    // SAFETY: fd is a valid, not-yet-wrapped socket.
                    libc::close(fd);
                    return Err(err);
                }
                // EINPROGRESS is the non-blocking connect's "in
                // flight", not a failure — completion is awaited
                // below, so the fd stays open and wrapped.
            }
            OwnedFd::from_raw_fd(fd)
        };
        // SAFETY: `fd` is a freshly-created `OwnedFd`; ownership moves into
        // the `AsyncFd`, which keeps it open and unchanged for its lifetime.
        let fd = unsafe { AsyncFd::register(fd) }?;
        loop {
            let mut guard = fd.writable().await?;
            let mut so_error: libc::c_int = 0;
            let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            // SAFETY: so_error/len are valid, correctly-sized locals.
            let rc = unsafe {
                libc::getsockopt(
                    fd.get_ref().as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_ERROR,
                    &mut so_error as *mut libc::c_int as *mut libc::c_void,
                    &mut len,
                )
            };
            if rc != 0 {
                return Err(io::Error::last_os_error());
            }
            match so_error {
                0 => {
                    return Ok(Self {
                        fd,
                        pending: Vec::new(),
                    })
                }
                // Still connecting: clear the (spurious) writability
                // and wait for the completion edge.
                libc::EINPROGRESS => guard.clear_ready(),
                code => return Err(io::Error::from_raw_os_error(code)),
            }
        }
    }
}

impl AsyncRead for VsockStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        // Leftovers from a previous read go out first.
        if !this.pending.is_empty() {
            let n = this.pending.len().min(buf.remaining());
            let tail = this.pending.split_off(n);
            buf.put_slice(&this.pending);
            this.pending = tail;
            return Poll::Ready(Ok(()));
        }
        // Scratch read, then copy into the caller's buffer; anything
        // beyond its remaining capacity is kept in `pending` for the
        // next poll (a stable-API necessity, not a fast path).
        let mut scratch = [0u8; 4096];
        loop {
            let mut guard = match this.fd.poll_read_ready(cx) {
                Poll::Ready(Ok(guard)) => guard,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            };
            // SAFETY: scratch is a valid, correctly-sized buffer.
            let result = guard.try_io(|inner| unsafe {
                let n = libc::read(
                    inner.as_raw_fd(),
                    scratch.as_mut_ptr() as *mut libc::c_void,
                    scratch.len(),
                );
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            });
            match result {
                // try_io's Err means "readiness lost" (WouldBlock) —
                // poll again; the guard has re-registered interest.
                Err(_would_block) => continue,
                Ok(Ok(0)) => return Poll::Ready(Ok(())), // EOF
                Ok(Ok(n)) => {
                    let fit = n.min(buf.remaining());
                    buf.put_slice(&scratch[..fit]);
                    if n > fit {
                        this.pending.extend_from_slice(&scratch[fit..n]);
                    }
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(e)) => return Poll::Ready(Err(e)),
            }
        }
    }
}

impl AsyncWrite for VsockStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        loop {
            let mut guard = match this.fd.poll_write_ready(cx) {
                Poll::Ready(Ok(guard)) => guard,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            };
            // SAFETY: source is buf with its own length.
            let result = guard.try_io(|inner| unsafe {
                let n = libc::write(
                    inner.as_raw_fd(),
                    buf.as_ptr() as *const libc::c_void,
                    buf.len(),
                );
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            });
            match result {
                Err(_would_block) => continue,
                Ok(result) => return Poll::Ready(result),
            }
        }
    }

    /// No user-space buffering: writes go straight to the socket.
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        // SAFETY: the fd is owned and registered.
        let rc = unsafe { libc::shutdown(this.fd.get_ref().as_raw_fd(), libc::SHUT_WR) };
        if rc != 0 {
            return Poll::Ready(Err(io::Error::last_os_error()));
        }
        Poll::Ready(Ok(()))
    }
}

/// The loopback half of the transport. Held for the agent's lifetime
/// (dropping it stops the forwarder).
pub struct VsockTransport {
    loopback: SocketAddr,
    _task: tokio::task::JoinHandle<()>,
}

impl VsockTransport {
    /// Spawn the forwarder for the production path: accept on
    /// `127.0.0.1:<server_url's port>` (the HTTP client's resolver
    /// override redirects the host to 127.0.0.1 but keeps the URL's
    /// port, so the forwarder must own exactly that port) and pipe
    /// each connection to the HOST over vsock on the fixed monitoring
    /// port.
    pub fn spawn(server_url: &str) -> io::Result<Self> {
        let (host, port) = server_host_port(server_url).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("cannot extract host and port from {server_url:?}"),
            )
        })?;
        // Fail loud on an IP-literal host: reqwest's `.resolve()`
        // override only applies to HOSTNAMES, so an IP-literal URL
        // would silently dial the management network instead of the
        // forwarder — the authenticated traffic would leave over the
        // wrong network with no error. Refusing to start is the only
        // honest behavior (ADR-026: transport config must never
        // quietly change which network carries the session). Bracketed
        // IPv6 literals ("[::1]") are stripped before the parse so the
        // canonical form is caught too.
        let bare_host = host.trim_start_matches('[').trim_end_matches(']');
        if bare_host.parse::<std::net::IpAddr>().is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "server_url host {host:?} is an IP literal; vsock mode requires a DNS \
                     name (the client's resolver override only applies to hostnames — an \
                     IP literal would silently dial the management network instead of the \
                     vsock forwarder)"
                ),
            ));
        }
        Self::spawn_to(VMADDR_CID_HOST, VSOCK_MONITOR_PORT, port)
    }

    /// Spawn the forwarder against an explicit CID/port, listening on
    /// an explicit loopback port (0 = ephemeral). The production path
    /// is [`Self::spawn`]; the local CID and ephemeral loopback port
    /// exist for tests that prove the pipe against an in-kernel
    /// listener. Synchronous by design: `Agent::start` is sync, and
    /// the listener must exist before any client is built against it.
    fn spawn_to(cid: u32, port: u32, listen_port: u16) -> io::Result<Self> {
        let std_listener =
            std::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], listen_port)))?;
        std_listener.set_nonblocking(true)?;
        let loopback = std_listener.local_addr()?;
        let listener = tokio::net::TcpListener::from_std(std_listener)?;
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut conn, _)) = listener.accept().await else {
                    return;
                };
                let connect = VsockStream::connect(cid, port);
                tokio::spawn(async move {
                    // Bound the connect: a wedged AF_VSOCK connect would
                    // otherwise park this task (and hold the accepted
                    // loopback socket) until the guest process goes
                    // away, and each client retry would accumulate
                    // another one.
                    match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
                        Ok(Ok(mut vsock)) => {
                            // The whole transport: splice bytes both
                            // ways until either side closes.
                            let _ = tokio::io::copy_bidirectional(&mut conn, &mut vsock).await;
                        }
                        Ok(Err(e)) => {
                            tracing::debug!(
                                error = %e,
                                port = port,
                                "vsock transport: host connect failed — closing the loopback connection (the client retries with backoff)"
                            );
                        }
                        Err(_) => {
                            tracing::debug!(
                                port = port,
                                "vsock transport: host connect timed out — closing the loopback connection (the client retries with backoff)"
                            );
                        }
                    }
                });
            }
        });
        Ok(Self {
            loopback,
            _task: task,
        })
    }

    /// The local endpoint the HTTP client's dial is redirected to.
    pub fn loopback(&self) -> SocketAddr {
        self.loopback
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    #[test]
    fn server_host_port_extracts_the_url_authority() {
        // The forwarder must own the URL's port: the resolver override
        // moves the host to 127.0.0.1 but the dial keeps it.
        assert_eq!(
            server_host_port("https://manager.chv.internal:8443"),
            Some(("manager.chv.internal".to_string(), 8443))
        );
        // A portless https URL means 443 — which a non-root service
        // cannot bind on 127.0.0.1, hence the documented requirement
        // for an explicit unprivileged port in vsock mode.
        assert_eq!(
            server_host_port("https://manager.chv.internal"),
            Some(("manager.chv.internal".to_string(), 443))
        );
        assert_eq!(server_host_port("http://manager.example"), None);
        assert_eq!(server_host_port("https://"), None);
        assert_eq!(server_host_port("https://mgr:invalid/"), None);
    }

    #[tokio::test]
    async fn spawn_binds_the_server_url_port() {
        let transport = VsockTransport::spawn("https://m.example:18443").unwrap();
        assert_eq!(transport.loopback().port(), 18443);
        assert_eq!(transport.loopback().ip().to_string(), "127.0.0.1");
    }

    #[test]
    fn spawn_rejects_an_ip_literal_host() {
        // reqwest's .resolve() override only applies to hostnames; an
        // IP-literal server_url would silently dial the management
        // network instead of the forwarder — the transport must refuse
        // to start rather than ship traffic over the wrong network.
        let err = match VsockTransport::spawn("https://192.168.64.1:18443") {
            Err(e) => e,
            Ok(_) => panic!("vsock transport must reject an IP-literal server_url"),
        };
        assert!(err.to_string().contains("IP literal"), "got: {err}");
        // The canonical bracketed IPv6 form must be caught too (the
        // brackets are stripped before the parse).
        assert!(
            VsockTransport::spawn("https://[::1]:18443").is_err(),
            "bracketed IPv6 literal must be rejected"
        );
    }

    #[test]
    fn server_host_extracts_the_url_host() {
        assert_eq!(
            server_host("https://manager.chv.internal:8443").as_deref(),
            Some("manager.chv.internal")
        );
        assert_eq!(
            server_host("https://manager.chv.internal").as_deref(),
            Some("manager.chv.internal")
        );
        assert_eq!(
            server_host("https://manager.chv.internal/").as_deref(),
            Some("manager.chv.internal")
        );
        assert_eq!(server_host("http://manager.example"), None);
        assert_eq!(server_host("https://"), None);
        assert_eq!(server_host("https:///path"), None);
    }

    #[test]
    fn contract_constants_are_pinned() {
        // The mirror of chv_hypervisor_api::VSOCK_MONITOR_PORT and the
        // Linux VMADDR_CID_HOST. The G5 rig asserts both sides agree at
        // runtime; this pins the guest-side values the rig depends on.
        assert_eq!(VSOCK_MONITOR_PORT, 1024);
        assert_eq!(VMADDR_CID_HOST, 2);
        assert_eq!(VMADDR_CID_HOST, libc::VMADDR_CID_HOST);
    }

    #[tokio::test]
    async fn forwarder_closes_loopback_connections_when_vsock_dial_fails() {
        // A port nothing listens on: on any host (with or without a
        // vsock device) the dial fails fast, and the loopback peer must
        // see EOF rather than a hang. 61024 is deliberately NOT the
        // contract port — a rig host listening on 1024 must not make
        // this test dial it.
        let transport = VsockTransport::spawn_to(VMADDR_CID_HOST, 61024, 0).unwrap();
        let mut conn = tokio::net::TcpStream::connect(transport.loopback())
            .await
            .unwrap();
        let mut buf = [0u8; 16];
        // This asserts eventual EOF, not a 5 s latency SLO: libtest
        // scheduling on a loaded CI runner can delay the forwarder wake-up
        // past the old 5 s bound. A generous 30 s still fails on a real hang
        // while removing the timing flake that blocked the coverage job.
        let n = tokio::time::timeout(std::time::Duration::from_secs(30), conn.read(&mut buf))
            .await
            .expect("the failed dial must close the loopback connection eventually (30 s bound)")
            .unwrap();
        assert_eq!(n, 0, "EOF, not data and not a hang");
    }

    #[tokio::test]
    async fn forwarder_pipes_loopback_bytes_to_a_vsock_listener() {
        // Prove the pipe end-to-end against a REAL AF_VSOCK listener on
        // the local CID (kernel loopback, no host needed). Skipped when
        // the kernel offers no vsock family or exposes AF_VSOCK without a
        // working local loopback transport (CI containers, bare metal) —
        // the g5 rig covers the full path on real hardware.
        let listen =
            unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        if listen < 0 {
            eprintln!("skipping: no AF_VSOCK support in this environment");
            return;
        }
        // Bind to the wildcard CID and a self-chosen port: this
        // kernel's vsock (loopback transport, no host) does not
        // auto-assign ephemeral ports for port 0, so getsockname would
        // report 0. The port is pid-mixed to dodge collisions with a
        // concurrently running copy of this test binary.
        let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
        addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
        addr.svm_cid = libc::VMADDR_CID_ANY;
        addr.svm_port = 40_000 + (std::process::id() % 20_000);
        // SAFETY: listen/addr are valid, correctly sized.
        let rc = unsafe {
            libc::bind(
                listen,
                &addr as *const libc::sockaddr_vm as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
            )
        };
        assert_eq!(rc, 0, "bind: {}", io::Error::last_os_error());
        assert_eq!(unsafe { libc::listen(listen, 16) }, 0);
        let port = addr.svm_port;
        assert_ne!(port, 0, "the port must be the one we chose");

        // Some kernels expose AF_VSOCK but do not route VMADDR_CID_LOCAL
        // (GitHub-hosted runners): socket/bind/listen succeed, but the
        // forwarder's dial resets and the TCP side fails with a bare
        // `ConnectionReset` that says nothing about the pipe. Probe the
        // loopback transport once and skip when it is not truly usable.
        let mut probe_addr = addr;
        probe_addr.svm_cid = libc::VMADDR_CID_LOCAL;
        let probe =
            unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        let probe_ok = probe >= 0
            && unsafe {
                libc::connect(
                    probe,
                    &probe_addr as *const libc::sockaddr_vm as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
                )
            } == 0;
        if !probe_ok {
            let err = io::Error::last_os_error();
            // SAFETY: both fds are valid on the paths that reach here.
            unsafe {
                if probe >= 0 {
                    libc::close(probe);
                }
                libc::close(listen);
            }
            eprintln!(
                "skipping: AF_VSOCK exists but the local loopback transport is unusable: {err}"
            );
            return;
        }
        // Drain the probe connection so the echo thread accepts the
        // forwarder's connection (the listen backlog would otherwise hand
        // it the already-closed probe first).
        let accepted = unsafe { libc::accept(listen, std::ptr::null_mut(), std::ptr::null_mut()) };
        // SAFETY: probe is a valid connected fd; accepted is valid on success.
        unsafe {
            if accepted >= 0 {
                libc::close(accepted);
            }
            libc::close(probe);
        }

        // Echo server on the vsock listener, in a blocking thread.
        let echo = std::thread::spawn(move || {
            // SAFETY: listen is a valid listening socket.
            let conn = unsafe { libc::accept(listen, std::ptr::null_mut(), std::ptr::null_mut()) };
            if conn < 0 {
                // SAFETY: listen is a valid fd.
                unsafe { libc::close(listen) };
                return;
            }
            let mut buf = [0u8; 64];
            // SAFETY: conn is a valid stream socket; buf is sized.
            let n = unsafe { libc::read(conn, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n > 0 {
                // SAFETY: echo back exactly the n bytes read.
                unsafe {
                    libc::write(conn, buf.as_ptr() as *const libc::c_void, n as usize);
                }
            }
            // SAFETY: both fds are valid.
            unsafe {
                libc::close(conn);
                libc::close(listen);
            }
        });

        let transport = VsockTransport::spawn_to(libc::VMADDR_CID_LOCAL, port, 0).unwrap();
        let mut conn = tokio::net::TcpStream::connect(transport.loopback())
            .await
            .unwrap();
        conn.write_all(b"vsock-pipe").await.unwrap();
        let mut buf = [0u8; 10];
        tokio::time::timeout(std::time::Duration::from_secs(5), conn.read_exact(&mut buf))
            .await
            .expect("echo within 5 s")
            .unwrap();
        assert_eq!(&buf, b"vsock-pipe");
        echo.join().expect("echo thread");
    }
}
