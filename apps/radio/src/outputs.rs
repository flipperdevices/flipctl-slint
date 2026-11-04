//! The outputs the stream can be moved to, kept up to date while the app runs.
//!
//! A display is plugged in, or WirePlumber gives a card its profile late, and a
//! sink appears; it is unplugged and one goes. The sound server says so: a
//! connection subscribed to sinks is sent an event for each one that comes, goes or
//! changes, so a thread is parked on it and the sinks are listed again when it
//! speaks. Nothing is polled. A burst of events, which is what one card coming up
//! is, is waited out first and answered with one listing.

use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use pulseaudio::protocol::{self, Command, SubscriptionMask};

use crate::pulse::Conn;

/// How long the events have to stop for before the sinks are listed again.
const SETTLE: Duration = Duration::from_millis(250);

/// The sinks as `(name, description)`: once now, and again each time they change,
/// for as long as the receiver is kept.
pub fn watch() -> Receiver<Vec<(String, String)>> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("outputs".into())
        .spawn(move || {
            let _ = run(&tx);
        })
        .expect("spawn outputs");
    rx
}

fn current(conn: &mut Conn) -> Vec<(String, String)> {
    conn.sinks()
        .unwrap_or_default()
        .into_iter()
        .map(|(name, description)| (name, flipctl_app::font::ascii(&description)))
        .collect()
}

fn run(tx: &Sender<Vec<(String, String)>>) -> std::io::Result<()> {
    let mut lister = Conn::open()?;
    let mut last = current(&mut lister);
    if tx.send(last.clone()).is_err() {
        return Ok(());
    }

    // A second connection for the events, since the lister's replies would arrive
    // between them on one.
    let mut events = Conn::open()?;
    events.act(&Command::Subscribe(SubscriptionMask::SINK))?;
    let (heard, wait) = mpsc::channel();
    std::thread::Builder::new()
        .name("sink events".into())
        .spawn(move || {
            while let Ok((_, msg)) = protocol::read_command_message(&mut events.sock, events.version) {
                if matches!(msg, Command::SubscribeEvent(_)) && heard.send(()).is_err() {
                    return;
                }
            }
        })
        .expect("spawn sink events");

    while wait.recv().is_ok() {
        while wait.recv_timeout(SETTLE).is_ok() {}
        let now = current(&mut lister);
        if now != last {
            if tx.send(now.clone()).is_err() {
                break;
            }
            last = now;
        }
    }
    Ok(())
}
