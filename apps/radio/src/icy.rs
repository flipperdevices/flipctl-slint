//! The station's titles, taken out of the stream they come in.
//!
//! A server asked for them puts a block in the audio every `icy-metaint` bytes: one
//! byte of length in sixteens, then that many bytes of `StreamTitle='...';` and
//! padding. The decoder must never see one, so this reader takes them out and keeps
//! the last title it found.

use std::io::{self, Read};
use std::sync::{Arc, Mutex};

pub struct Icy<R> {
    inner: R,
    metaint: Option<usize>,
    /// Audio bytes before the next block.
    left: usize,
    title: Arc<Mutex<String>>,
}

impl<R: Read> Icy<R> {
    pub fn new(inner: R, metaint: Option<usize>, title: Arc<Mutex<String>>) -> Self {
        Self { inner, metaint, left: metaint.unwrap_or(0), title }
    }

    fn block(&mut self) -> io::Result<()> {
        let mut len = [0u8; 1];
        self.inner.read_exact(&mut len)?;
        let mut block = vec![0u8; usize::from(len[0]) * 16];
        self.inner.read_exact(&mut block)?;
        // An empty block means nothing has changed, not that the title is gone.
        if let Some(title) = title(&block) {
            *self.title.lock().unwrap() = title;
        }
        Ok(())
    }
}

impl<R: Read> Read for Icy<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let Some(metaint) = self.metaint else { return self.inner.read(buf) };
        if self.left == 0 {
            self.block()?;
            self.left = metaint;
        }
        let want = buf.len().min(self.left);
        let n = self.inner.read(&mut buf[..want])?;
        self.left -= n;
        Ok(n)
    }
}

/// The title in a block, if it has one. UTF-8 when it is, and Latin-1 otherwise,
/// which is what an older server sends; the page makes it drawable.
fn title(block: &[u8]) -> Option<String> {
    const KEY: &[u8] = b"StreamTitle='";
    let at = block.windows(KEY.len()).position(|w| w == KEY)? + KEY.len();
    let rest = &block[at..];
    let end = rest.windows(2).position(|w| w == b"';").unwrap_or_else(|| {
        rest.iter().rposition(|&b| b == b'\'').unwrap_or(rest.len())
    });
    let raw = &rest[..end];
    let text = match std::str::from_utf8(raw) {
        Ok(s) => s.to_string(),
        Err(_) => raw.iter().map(|&b| char::from(b)).collect(),
    };
    Some(text.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(metaint: usize, chunks: &[(&[u8], &str)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (audio, meta) in chunks {
            assert_eq!(audio.len(), metaint);
            out.extend_from_slice(audio);
            let mut block = meta.as_bytes().to_vec();
            block.resize(block.len().div_ceil(16) * 16, 0);
            out.push((block.len() / 16) as u8);
            out.extend(block);
        }
        out
    }

    /// The audio comes through whole and in order, the blocks do not, and the last
    /// title stays after an empty block.
    #[test]
    fn blocks_are_taken_out_and_their_titles_kept() {
        let bytes = stream(
            4,
            &[(b"abcd", "StreamTitle='Artist - Song';StreamUrl='';"), (b"efgh", ""), (b"ijkl", "")],
        );
        let title = Arc::new(Mutex::new(String::new()));
        let mut icy = Icy::new(&bytes[..], Some(4), Arc::clone(&title));
        let mut audio = Vec::new();
        let mut buf = [0u8; 3];
        loop {
            match icy.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => audio.extend_from_slice(&buf[..n]),
            }
        }
        assert_eq!(audio, b"abcdefghijkl");
        assert_eq!(*title.lock().unwrap(), "Artist - Song");
    }

    #[test]
    fn a_stream_without_titles_is_read_as_it_is() {
        let title = Arc::new(Mutex::new(String::new()));
        let mut icy = Icy::new(&b"plain mp3"[..], None, title);
        let mut out = String::new();
        icy.read_to_string(&mut out).unwrap();
        assert_eq!(out, "plain mp3");
    }

    #[test]
    fn titles_in_either_alphabet() {
        assert_eq!(title(b"StreamTitle='Rock \'n\' Roll';").as_deref(), Some("Rock 'n' Roll"));
        assert_eq!(title("StreamTitle='Наше Радио';".as_bytes()).as_deref(), Some("Наше Радио"));
        assert_eq!(title(b"StreamTitle='Caf\xe9';").as_deref(), Some("Caf\u{e9}"));
        assert_eq!(title(b"StreamUrl='x';"), None);
    }
}
