//! The routing table, re-read when the kernel says it changed.
//!
//! The Routing info page used to poll `/proc/net/route` and `/proc/net/ipv6_route`
//! once a second, which is 746us of procfs generation per second measured on the
//! device. Not expensive, but the wrong shape: routes do not drift, they change on
//! events, and rtnetlink is how the kernel announces them.
//!
//! Told rather than asked, the same division `net` draws with `nmcli monitor`. The
//! difference is that the event is used only as a *signal*: the message says a
//! route changed, and the answer comes from re-reading the two files with the
//! parser that was already there. Decoding `RTM_NEWROUTE` payloads would mean a
//! second implementation of the same thing, free to disagree with the first, in
//! exchange for saving a file read that only happens when a route actually moved.
//!
//! Better than the poll on both counts: nothing at all while the table is still,
//! and the page updates the moment it is not, rather than up to a second later.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::netlink::{self, Socket, Stop, Stopper, Wake};
use crate::sysinfo::{routes, Route};

/// The `RTMGRP_*` groups: a route changing in either family.
///
/// `RTMGRP_LINK` and the address groups arrive on the same socket and are what an
/// interface or an address changing would come in on, if a screen wanted those.
const RTMGRP_IPV4_ROUTE: u32 = 0x40;
const RTMGRP_IPV6_ROUTE: u32 = 0x400;

/// The routing table as it stands, kept current by the kernel's own events.
///
/// Dropping it ends the thread, which is what closing the page does.
pub struct RouteWatch {
    state: Arc<Mutex<Vec<Route>>>,
    dirty: Arc<AtomicBool>,
    /// Both of these are None when the subscription could not be set up. The page
    /// then has the one read it opened with and does not follow, which is worse
    /// than the poll it replaced but better than no page.
    stopper: Option<Stopper>,
    thread: Option<thread::JoinHandle<()>>,
}

impl RouteWatch {
    /// Read the table once, then follow it.
    ///
    /// The first read happens here rather than on the thread, so the page has its
    /// rows on the frame it opens: waiting for an event would show an empty table
    /// until something changed, which on a settled machine is never.
    pub fn spawn() -> Self {
        let state = Arc::new(Mutex::new(routes()));
        let dirty = Arc::new(AtomicBool::new(false));
        // No eventfd means no way to end a blocking wait, so there is no thread to
        // start: a thread that cannot be stopped outlives the page it belongs to.
        let (waiter, stopper) = match Stop::new() {
            Ok(stop) => stop.split(),
            Err(e) => {
                eprintln!("routes         no eventfd, not following: {e}");
                return Self { state, dirty, stopper: None, thread: None };
            }
        };

        let socket = match Socket::subscribe(netlink::ROUTE, RTMGRP_IPV4_ROUTE | RTMGRP_IPV6_ROUTE)
        {
            Ok(socket) => socket,
            // Without the subscription the page still has the read above; it just
            // will not follow. Better than no page.
            Err(e) => {
                eprintln!("routes         no rtnetlink, not following: {e}");
                return Self { state, dirty, stopper: None, thread: None };
            }
        };

        let (s, d) = (Arc::clone(&state), Arc::clone(&dirty));
        let thread = thread::Builder::new()
            .name("watch-routes".into())
            .spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match waiter.wait(&socket) {
                        Ok(Wake::Ready) => {}
                        // Asked to stop, or poll itself failed, which is not
                        // something to spin on.
                        Ok(Wake::Stopped) | Err(_) => return,
                    }
                    // Drain what woke us. The messages themselves are not parsed:
                    // one or a dozen of them mean the same thing, which is that
                    // the table below is no longer what we last read.
                    let Ok(got) = socket.recv(&mut buf) else {
                        return;
                    };
                    if got == 0 {
                        return;
                    }
                    let fresh = routes();
                    let mut cur = s.lock().unwrap_or_else(|e| e.into_inner());
                    if *cur != fresh {
                        *cur = fresh;
                        d.store(true, Ordering::Relaxed);
                    }
                }
            })
            .ok();

        Self { state, dirty, stopper: Some(stopper), thread }
    }

    pub fn get(&self) -> Vec<Route> {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// True once per change, so the caller repaints only when something moved.
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::Relaxed)
    }
}

impl Drop for RouteWatch {
    fn drop(&mut self) {
        // End the wait, then wait for it: the thread is parked in poll and comes
        // back out within microseconds. Joining rather than detaching is what makes
        // closing the page mean the thread is gone, not merely unreferenced.
        if let Some(stopper) = self.stopper.as_ref() {
            stopper.stop();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table is there before any event arrives: a settled machine emits none,
    /// and a page that waited for one would show nothing at all.
    #[test]
    fn the_first_read_happens_on_open() {
        let watch = RouteWatch::spawn();
        // Whatever this host's table is, the same answer as a direct read.
        assert_eq!(watch.get(), routes());
        assert!(!watch.take_dirty(), "the first read is not a change");
    }

    /// Dropping it ends the thread rather than leaking it, which is the whole
    /// reason the wait is a poll over two descriptors.
    #[test]
    fn dropping_it_stops_the_thread() {
        let watch = RouteWatch::spawn();
        let thread = watch.thread.as_ref().map(|t| t.thread().id());
        assert!(thread.is_some(), "a subscription and a thread");
        // Drop returns only once the thread has joined, so returning at all is the
        // assertion. A leaked thread would hang here.
        drop(watch);
    }
}
