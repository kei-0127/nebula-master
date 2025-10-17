//! # Raw Socket Implementation
//! 
//! This module provides raw socket functionality for low-level network
//! packet handling. Raw sockets allow direct access to network packets
//! at the IP layer, enabling custom protocol implementations and
//! network-level packet manipulation.
//! 
//! ## Key Features
//! 
//! - **Raw IP Sockets**: Direct access to IP layer packets
//! - **UDP Raw Sockets**: Raw UDP packet handling
//! - **Async Operations**: Non-blocking socket operations
//! - **Packet Manipulation**: Direct packet header access
//! - **Low-level Control**: Fine-grained network control
//! 
//! ## Use Cases
//! 
//! - **Custom Protocols**: Implement custom network protocols
//! - **Packet Analysis**: Analyze network traffic at IP level
//! - **Network Testing**: Test network behavior and responses
//! - **Protocol Development**: Develop new network protocols
//! 
//! ## Usage
//! 
//! ```rust
//! use nebula_media::socket::RawSocket;
//! 
//! // Create raw UDP socket
//! let socket = RawSocket::new(true)?;
//! 
//! // Receive raw packets
//! let mut buf = [0u8; 1500];
//! let len = socket.recv(&mut buf).await?;
//! ```

use libc;
use nix::sys::socket::SockAddr;
use std::future::Future;
use std::io;
use std::io::{Error, ErrorKind, Result};
use std::mem;
use std::os::unix::io::{AsRawFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll, Poll::Pending, Poll::Ready};
use tokio::io::unix::AsyncFd;

/// Async raw IPv4 socket.
/// Provides direct IP-level access; `udp=true` uses IPPROTO_UDP, `udp=false` uses raw IP with IP_HDRINCL.
pub struct RawSocket {
    fd: AsyncFd<RawFd>,  // Async file descriptor wrapper
}

impl Drop for RawSocket {
    /// Clean up raw socket resources
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd.as_raw_fd());
        }
    }
}

impl RawSocket {
    /// Create a raw IPv4 socket.
    /// `udp=true` opens IPPROTO_UDP; `udp=false` opens raw IP with IP_HDRINCL. Socket is non-blocking.
    pub fn new(udp: bool) -> Result<RawSocket> {
        let fd = if udp {
            unsafe { libc::socket(libc::AF_INET, libc::SOCK_RAW, libc::IPPROTO_UDP) }
        } else {
            let fd = unsafe {
                libc::socket(libc::AF_INET, libc::SOCK_RAW, libc::IPPROTO_RAW)
            };
            unsafe {
                libc::setsockopt(
                    fd as libc::c_int,
                    libc::IPPROTO_IP,
                    libc::IP_HDRINCL,
                    &mut 1 as *mut libc::c_int as *mut libc::c_void,
                    mem::size_of::<libc::c_int>() as libc::socklen_t,
                )
            };
            fd
        };
        let mut opt = 1 as libc::c_ulong;
        unsafe { libc::ioctl(fd, libc::FIONBIO, &mut opt) };
        Ok(RawSocket {
            fd: AsyncFd::new(fd)?,
        })
    }

    /// Async receive: fills `buf` with the next packet and returns its length.
    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        poll_fn(|cx| self.poll_recv(cx, buf)).await
    }

    /// Poll-based receive used by `recv`; clears readiness on EWOULDBLOCK and retries.
    pub fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            let mut ready = match self.fd.poll_read_ready(cx) {
                Ready(x) => x?,
                Pending => return Pending,
            };

            let ret = unsafe {
                libc::recv(
                    self.fd.as_raw_fd(),
                    buf.as_ptr() as *mut libc::c_void,
                    buf.len() as libc::size_t,
                    0,
                )
            };

            return if ret < 0 {
                let e = Error::last_os_error();
                if e.kind() == ErrorKind::WouldBlock {
                    ready.clear_ready();
                    continue;
                } else {
                    Ready(Err(e))
                }
            } else {
                let n = ret as usize;
                Ready(Ok(n))
            };
        }
    }

    /// Async sendto: writes `buf` to `target` and returns bytes sent.
    pub async fn send_to(&self, buf: &[u8], target: &SockAddr) -> io::Result<usize> {
        poll_fn(|cx| self.poll_send_to(cx, buf, target)).await
    }

    /// Poll-based send used by `send_to`; clears readiness on EWOULDBLOCK and retries.
    pub fn poll_send_to(
        &self,
        cx: &mut Context<'_>,
        buf: &[u8],
        target: &SockAddr,
    ) -> Poll<io::Result<usize>> {
        loop {
            let mut ready = match self.fd.poll_write_ready(cx) {
                Ready(x) => x?,
                Pending => return Pending,
            };

            let ret = unsafe {
                let (ptr, len) = target.as_ffi_pair();
                libc::sendto(
                    self.fd.as_raw_fd(),
                    buf.as_ptr() as *const libc::c_void,
                    buf.len() as libc::size_t,
                    0,
                    ptr,
                    len,
                )
            };

            return if ret < 0 {
                let e = Error::last_os_error();
                if e.kind() == ErrorKind::WouldBlock {
                    ready.clear_ready();
                    continue;
                } else {
                    Ready(Err(e))
                }
            } else {
                Ready(Ok(ret as usize))
            };
        }
    }
}

/// Minimal future adapter that polls a closure until it returns `Ready`.
struct PollFn<F> {
    f: F,
}

impl<F> Unpin for PollFn<F> {}

impl<T, F> Future for PollFn<F>
where
    F: FnMut(&mut Context<'_>) -> Poll<T>,
{
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        (&mut self.f)(cx)
    }
}

/// Build a `Future` from a polling closure.
fn poll_fn<T, F>(f: F) -> PollFn<F>
where
    F: FnMut(&mut Context<'_>) -> Poll<T>,
{
    PollFn { f }
}
