//! Talking to the kernel over netlink: the socket, and the two walkers.
//!
//! Two consumers so far and they use it differently. `nl80211` asks a question and
//! reads the answer, over a socket it opens and closes each time. `route` says
//! nothing at all and waits to be told, over a socket it holds for the life of the
//! screen. What they share is the framing, which is what lives here.
//!
//! Netlink is a socket, so nothing here shells out and nothing needs root. `libc`
//! is already a dependency of this crate for `getifaddrs`, for the same reason.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::time::Duration;

/// `NETLINK_ROUTE`: links, addresses and routes.
pub const ROUTE: libc::c_int = 0;
/// `NETLINK_GENERIC`: the families that register themselves, nl80211 among them.
pub const GENERIC: libc::c_int = 16;

/// A netlink message header, and an attribute header. Both are padded to four.
pub const NLMSG_HDR: usize = 16;
pub const ATTR_HDR: usize = 4;

pub const NLMSG_ERROR: u16 = 2;
pub const NLMSG_DONE: u16 = 3;
pub const NLM_F_REQUEST: u16 = 0x001;
/// ROOT | MATCH: every entry, rather than one looked up by key.
pub const NLM_F_DUMP: u16 = 0x300;

/// Netlink pads every header and payload to four bytes.
pub fn align4(n: usize) -> usize {
    n.div_ceil(4) * 4
}

pub struct Socket(OwnedFd);

impl Socket {
    /// A socket on `protocol`, listening to nothing.
    pub fn open(protocol: libc::c_int) -> io::Result<Self> {
        // SAFETY: constant arguments, and the descriptor is checked before it is
        // taken ownership of.
        let fd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, protocol) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh descriptor this call owns.
        Ok(Self(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    /// A socket on `protocol` subscribed to multicast `groups`.
    ///
    /// The groups are the legacy `RTMGRP_*` bitmask, which is a `u32` and so tops
    /// out at group 32. Everything this reads is well inside that; a group beyond
    /// it needs `NETLINK_ADD_MEMBERSHIP` instead.
    pub fn subscribe(protocol: libc::c_int, groups: u32) -> io::Result<Self> {
        let socket = Self::open(protocol)?;
        // Zeroed, then filled: the padding field is not ours to name and its type
        // is not the same across libc versions.
        // SAFETY: sockaddr_nl is plain old data with no invalid bit patterns.
        let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        // nl_pid stays 0, so the kernel picks the port and two of these can coexist.
        addr.nl_groups = groups;
        // SAFETY: addr outlives the call and its length is what is passed.
        let rc = unsafe {
            libc::bind(
                socket.0.as_raw_fd(),
                std::ptr::addr_of!(addr).cast(),
                std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(socket)
    }

    /// Bound wait on receive, for a caller that cannot afford to block.
    pub fn set_timeout(&self, timeout: Duration) -> io::Result<()> {
        let tv = libc::timeval {
            tv_sec: timeout.as_secs() as libc::time_t,
            tv_usec: timeout.subsec_micros() as libc::suseconds_t,
        };
        // SAFETY: tv outlives the call and its size is what the option expects.
        let rc = unsafe {
            libc::setsockopt(
                self.0.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                std::ptr::addr_of!(tv).cast(),
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn send(&self, message: &[u8]) -> io::Result<()> {
        // SAFETY: pointer and length describe the same slice; a netlink socket is
        // connected to the kernel without a connect.
        let sent = unsafe { libc::send(self.0.as_raw_fd(), message.as_ptr().cast(), message.len(), 0) };
        if sent < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        // SAFETY: writing into a slice this call owns for its duration.
        let got = unsafe { libc::recv(self.0.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
        if got < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(got as usize)
    }

    pub fn as_fd(&self) -> BorrowedFd<'_> {
        use std::os::fd::AsFd;
        self.0.as_fd()
    }
}

/// A message header, a payload of `header` bytes, and attributes.
///
/// `header` is the family's own fixed part: four bytes of `genlmsghdr` for generic
/// netlink, and whatever the message calls for on rtnetlink.
pub fn message(kind: u16, flags: u16, header: &[u8], attrs: &[(u16, &[u8])]) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(&0u32.to_ne_bytes()); // length, patched below
    out.extend_from_slice(&kind.to_ne_bytes());
    out.extend_from_slice(&(NLM_F_REQUEST | flags).to_ne_bytes());
    out.extend_from_slice(&1u32.to_ne_bytes()); // sequence
    out.extend_from_slice(&0u32.to_ne_bytes()); // port, filled in by the kernel
    out.extend_from_slice(header);
    out.resize(align4(out.len()), 0);
    for (kind, payload) in attrs {
        out.extend_from_slice(&((ATTR_HDR + payload.len()) as u16).to_ne_bytes());
        out.extend_from_slice(&kind.to_ne_bytes());
        out.extend_from_slice(payload);
        out.resize(align4(out.len()), 0);
    }
    let len = out.len() as u32;
    out[..4].copy_from_slice(&len.to_ne_bytes());
    out
}

/// Visit each attribute of an attribute stream.
///
/// A length shorter than its own header, or longer than what is left, is a
/// malformed stream and ends the walk rather than wrapping or panicking.
pub fn each_attr(body: &[u8], mut visit: impl FnMut(u16, &[u8])) {
    let mut at = 0;
    while at + ATTR_HDR <= body.len() {
        let len = u16::from_ne_bytes([body[at], body[at + 1]]) as usize;
        let kind = u16::from_ne_bytes([body[at + 2], body[at + 3]]);
        if len < ATTR_HDR || at + len > body.len() {
            return;
        }
        visit(kind, &body[at + ATTR_HDR..at + len]);
        at += align4(len);
    }
}

/// Visit each message in a buffer, stopping at DONE or an error.
///
/// `visit` is handed the message's type and everything past the netlink header, so
/// a caller that knows its family's fixed part can skip it. Returns false if the
/// kernel answered with an error.
pub fn each_message(buf: &[u8], mut visit: impl FnMut(u16, &[u8])) -> bool {
    let mut at = 0;
    while at + NLMSG_HDR <= buf.len() {
        let len = u32::from_ne_bytes(buf[at..at + 4].try_into().expect("4 bytes")) as usize;
        let kind = u16::from_ne_bytes([buf[at + 4], buf[at + 5]]);
        if len < NLMSG_HDR || at + len > buf.len() {
            return true;
        }
        match kind {
            NLMSG_DONE => return true,
            NLMSG_ERROR => return false,
            _ => {}
        }
        visit(kind, &buf[at + NLMSG_HDR..at + len]);
        at += align4(len);
    }
    true
}

/// A way to wake a thread that is blocked waiting on a socket.
///
/// An eventfd, so the waiting is a `poll` over two descriptors and there is no
/// timeout to wake up for. The alternative was a short receive timeout and a flag,
/// which is a wakeup several times a second to discover that nothing has happened:
/// the whole point of listening for events rather than polling.
pub struct Stop {
    /// Held by the waiter.
    reader: OwnedFd,
    /// Held by whoever wants it to stop.
    writer: OwnedFd,
}

impl Stop {
    pub fn new() -> io::Result<Self> {
        // SAFETY: constant arguments, checked before ownership is taken.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh descriptor this call owns.
        let reader = unsafe { OwnedFd::from_raw_fd(fd) };
        let writer = reader.try_clone()?;
        Ok(Self { reader, writer })
    }

    /// Split into the half the waiter keeps and the half that ends the wait.
    pub fn split(self) -> (Waiter, Stopper) {
        (Waiter(self.reader), Stopper(self.writer))
    }
}

pub struct Waiter(OwnedFd);
pub struct Stopper(OwnedFd);

/// What woke the waiter.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Wake {
    /// The socket has something to read.
    Ready,
    /// Somebody asked for the wait to end.
    Stopped,
}

impl Waiter {
    /// Sleep until the socket speaks or the wait is ended. No timeout: a thread in
    /// here costs nothing until one of the two happens.
    pub fn wait(&self, socket: &Socket) -> io::Result<Wake> {
        let mut fds = [
            libc::pollfd {
                fd: socket.as_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.0.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        loop {
            // SAFETY: the array and its length describe the same two entries, and
            // both descriptors are owned for the duration.
            let rc = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
            if rc < 0 {
                let err = io::Error::last_os_error();
                // A signal delivered mid-poll is not an answer to the question.
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            if fds[1].revents != 0 {
                return Ok(Wake::Stopped);
            }
            if fds[0].revents != 0 {
                return Ok(Wake::Ready);
            }
        }
    }
}

impl Stopper {
    /// End the wait. Best effort: a full counter already means "stop".
    pub fn stop(&self) {
        let one = 1u64.to_ne_bytes();
        // SAFETY: writing eight bytes from a local buffer to an eventfd, which is
        // exactly what it reads.
        unsafe {
            libc::write(self.0.as_raw_fd(), one.as_ptr().cast(), one.len());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attr(kind: u16, payload: &[u8]) -> Vec<u8> {
        let mut out = ((ATTR_HDR + payload.len()) as u16).to_ne_bytes().to_vec();
        out.extend_from_slice(&kind.to_ne_bytes());
        out.extend_from_slice(payload);
        out.resize(align4(out.len()), 0);
        out
    }

    #[test]
    fn attributes_are_walked_with_their_padding() {
        let mut body = attr(1, b"a");
        body.extend(attr(2, &7u32.to_ne_bytes()));
        let mut seen = Vec::new();
        each_attr(&body, |kind, payload| seen.push((kind, payload.to_vec())));
        assert_eq!(seen, [(1, b"a".to_vec()), (2, 7u32.to_ne_bytes().to_vec())]);
    }

    #[test]
    fn a_truncated_attribute_ends_the_walk_rather_than_running_off() {
        // A length claiming more than the buffer holds.
        let mut body = 64u16.to_ne_bytes().to_vec();
        body.extend_from_slice(&1u16.to_ne_bytes());
        body.extend_from_slice(b"short");
        let mut seen = 0;
        each_attr(&body, |_, _| seen += 1);
        assert_eq!(seen, 0);

        // And one that cannot hold its own header.
        let mut body = 2u16.to_ne_bytes().to_vec();
        body.extend_from_slice(&1u16.to_ne_bytes());
        each_attr(&body, |_, _| seen += 1);
        assert_eq!(seen, 0);
    }

    #[test]
    fn an_error_reply_is_not_read_as_data() {
        let mut buf = (NLMSG_HDR as u32).to_ne_bytes().to_vec();
        buf.extend_from_slice(&NLMSG_ERROR.to_ne_bytes());
        buf.resize(NLMSG_HDR, 0);
        let mut seen = 0;
        assert!(!each_message(&buf, |_, _| seen += 1));
        assert_eq!(seen, 0);
    }

    #[test]
    fn a_message_is_framed_with_its_length_and_padding() {
        let built = message(20, NLM_F_DUMP, &[7, 0, 0, 0], &[(3, &1u32.to_ne_bytes())]);
        assert_eq!(
            u32::from_ne_bytes(built[..4].try_into().unwrap()) as usize,
            built.len()
        );
        assert_eq!(built.len() % 4, 0);
        let mut seen = Vec::new();
        each_message(&built, |kind, body| {
            assert_eq!(kind, 20);
            // Past the four-byte family header, the attribute.
            each_attr(&body[4..], |k, v| seen.push((k, v.to_vec())));
        });
        assert_eq!(seen, [(3, 1u32.to_ne_bytes().to_vec())]);
    }

    /// The wait ends when it is told to, rather than on a timeout.
    #[test]
    fn a_waiter_can_be_stopped() {
        let socket = Socket::open(ROUTE).expect("a netlink socket");
        let (waiter, stopper) = Stop::new().expect("an eventfd").split();
        stopper.stop();
        assert_eq!(waiter.wait(&socket).expect("poll"), Wake::Stopped);
    }
}
