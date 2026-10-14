//! A measuring program, run and read a line at a time.
//!
//! Both pipes are read, on a thread each, because the programs do not agree on
//! which one a result goes to. A line ends at `\r` as well as `\n`: cfspeedtest
//! redraws its progress bar in place, and each redraw carries the running figure.

use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

pub struct Run {
    child: Child,
    lines: Receiver<String>,
}

/// What a look at the program found.
pub enum Polled {
    Lines(Vec<String>),
    /// Every line it printed, and how it ended. Both pipes have closed, so nothing
    /// it said is still on its way.
    Exited(Vec<String>, ExitStatus),
}

impl Run {
    pub fn start(argv: &[String]) -> io::Result<Self> {
        let (program, args) = argv.split_first().ok_or(io::ErrorKind::InvalidInput)?;
        let mut command = Command::new(program);
        command.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        // SAFETY: prctl is async-signal-safe. The program dies with the app, so a
        // test cannot go on loading the link after the window that started it.
        unsafe {
            command.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        let (tx, lines) = mpsc::channel();
        if let Some(out) = child.stdout.take() {
            read_lines(out, tx.clone());
        }
        if let Some(err) = child.stderr.take() {
            read_lines(err, tx);
        }
        Ok(Self { child, lines })
    }

    pub fn poll(&mut self) -> Polled {
        let mut got = Vec::new();
        loop {
            match self.lines.try_recv() {
                Ok(line) => got.push(line),
                Err(TryRecvError::Empty) => return Polled::Lines(got),
                Err(TryRecvError::Disconnected) => break,
            }
        }
        match self.child.wait() {
            Ok(status) => Polled::Exited(got, status),
            Err(_) => Polled::Lines(got),
        }
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn read_lines(mut pipe: impl Read + Send + 'static, tx: Sender<String>) {
    std::thread::spawn(move || {
        let mut pending = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            for &byte in &chunk[..n] {
                if byte == b'\n' || byte == b'\r' {
                    send(&tx, &mut pending);
                } else {
                    pending.push(byte);
                }
            }
        }
        send(&tx, &mut pending);
    });
}

fn send(tx: &Sender<String>, pending: &mut Vec<u8>) {
    let line = strip_escapes(&String::from_utf8_lossy(pending));
    pending.clear();
    if !line.trim().is_empty() {
        let _ = tx.send(line);
    }
}

/// Colour and cursor sequences, which a program writing to a terminal puts
/// between its words and a parser does not want to see.
fn strip_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        if chars.next() == Some('[') {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        }
    }
    out
}

/// What a list command printed, or why it printed nothing usable.
pub fn output(argv: &[String]) -> Result<String, String> {
    let (program, args) = argv.split_first().ok_or("no command")?;
    let mut command = Command::new(program);
    command.args(args).stdin(Stdio::null());
    // SAFETY: as in Run::start.
    unsafe {
        command.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    let out = command.output().map_err(|e| missing(program, &e))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let said = String::from_utf8_lossy(&out.stderr);
    Err(said.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("List failed").trim().into())
}

/// A program that could not be started, said the way the panel can show it.
pub fn missing(program: &str, error: &io::Error) -> String {
    if error.kind() == io::ErrorKind::NotFound {
        format!("{program} is not installed")
    } else {
        format!("Could not start {program}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_redrawn_line_arrives_as_each_redraw() {
        let argv: Vec<String> =
            ["sh", "-c", "printf 'a 1\\ra 2\\r\\033[1mb\\033[0m\\n'; echo err >&2"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        let mut run = Run::start(&argv).expect("sh");
        let mut lines = Vec::new();
        let status = loop {
            match run.poll() {
                Polled::Lines(got) => lines.extend(got),
                Polled::Exited(got, status) => {
                    lines.extend(got);
                    break status;
                }
            }
        };
        assert!(status.success());
        lines.sort();
        assert_eq!(lines, vec!["a 1", "a 2", "b", "err"]);
    }

    #[test]
    fn a_missing_program_says_so() {
        let err = output(&["no-such-speedtest".to_string()]).unwrap_err();
        assert_eq!(err, "no-such-speedtest is not installed");
    }
}
