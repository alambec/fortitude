//! Streaming reader of `multipart/related` bodies (RFC 2046 §5.1).
//!
//! Parts are yielded one at a time as soon as they are complete, so memory use is bounded by
//! the largest part rather than by the request size.

use bytes::{Buf, Bytes, BytesMut};
use futures::{Stream, StreamExt};
use memchr::memmem;

#[derive(Debug)]
pub struct Part {
    pub content_type: Option<String>,
    pub body: Bytes,
}

#[derive(Debug, thiserror::Error)]
pub enum MultipartError {
    #[error("cannot read request body: {0}")]
    Body(String),
    #[error("malformed multipart body: {0}")]
    Malformed(&'static str),
    #[error("part larger than {0} bytes")]
    TooLarge(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Preamble,
    AfterBoundary,
    Headers,
    Body,
    Done,
}

const MAX_HEADERS: usize = 16 * 1024;

pub struct MultipartReader<S> {
    stream: S,
    buf: BytesMut,
    finder: memmem::Finder<'static>,
    delimiter_len: usize,
    /// Position from which to look for the next delimiter in `buf`.
    scanned: usize,
    state: State,
    max_part: usize,
    content_type: Option<String>,
}

impl<S, E> MultipartReader<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    pub fn new(stream: S, boundary: &str, max_part: usize) -> Self {
        let delimiter = format!("\r\n--{boundary}").into_bytes();
        // A leading CRLF lets the first boundary, at the very start of the body, match the
        // same delimiter as the following ones.
        let mut buf = BytesMut::with_capacity(64 * 1024);
        buf.extend_from_slice(b"\r\n");
        Self {
            stream,
            buf,
            delimiter_len: delimiter.len(),
            finder: memmem::Finder::new(&delimiter).into_owned(),
            scanned: 0,
            state: State::Preamble,
            max_part,
            content_type: None,
        }
    }

    /// Read more data into the buffer; false at the end of the stream.
    async fn fill(&mut self) -> Result<bool, MultipartError> {
        match self.stream.next().await {
            Some(Ok(chunk)) => {
                self.buf.extend_from_slice(&chunk);
                Ok(true)
            }
            Some(Err(e)) => Err(MultipartError::Body(e.to_string())),
            None => Ok(false),
        }
    }

    async fn fill_or(&mut self, error: &'static str) -> Result<(), MultipartError> {
        if self.fill().await? {
            Ok(())
        } else {
            Err(MultipartError::Malformed(error))
        }
    }

    pub async fn next_part(&mut self) -> Result<Option<Part>, MultipartError> {
        loop {
            match self.state {
                State::Done => return Ok(None),
                State::Preamble => match self.finder.find(&self.buf) {
                    Some(i) => {
                        self.buf.advance(i + self.delimiter_len);
                        self.state = State::AfterBoundary;
                    }
                    None => {
                        let keep = self.delimiter_len - 1;
                        if self.buf.len() > keep {
                            self.buf.advance(self.buf.len() - keep);
                        }
                        self.fill_or("no boundary found").await?;
                    }
                },
                State::AfterBoundary => {
                    if self.buf.len() < 2 {
                        self.fill_or("truncated boundary").await?;
                        continue;
                    }
                    if self.buf.starts_with(b"--") {
                        self.state = State::Done;
                        continue;
                    }
                    // Skip optional transport padding up to the end of the boundary line.
                    match memchr::memmem::find(&self.buf, b"\r\n") {
                        Some(i) if self.buf[..i].iter().all(|b| *b == b' ' || *b == b'\t') => {
                            self.buf.advance(i + 2);
                            self.state = State::Headers;
                        }
                        Some(_) => return Err(MultipartError::Malformed("invalid boundary line")),
                        None if self.buf.len() > 1024 => {
                            return Err(MultipartError::Malformed("invalid boundary line"));
                        }
                        None => self.fill_or("truncated boundary").await?,
                    }
                }
                State::Headers => {
                    if self.buf.starts_with(b"\r\n") {
                        self.buf.advance(2);
                        self.content_type = None;
                        self.state = State::Body;
                        continue;
                    }
                    match memchr::memmem::find(&self.buf, b"\r\n\r\n") {
                        Some(i) => {
                            self.content_type = content_type(&self.buf[..i]);
                            self.buf.advance(i + 4);
                            self.scanned = 0;
                            self.state = State::Body;
                        }
                        None if self.buf.len() > MAX_HEADERS => {
                            return Err(MultipartError::Malformed("part headers too large"));
                        }
                        None => self.fill_or("truncated part headers").await?,
                    }
                }
                State::Body => match self.finder.find(&self.buf[self.scanned..]) {
                    Some(i) => {
                        let body = self.buf.split_to(self.scanned + i).freeze();
                        self.buf.advance(self.delimiter_len);
                        self.scanned = 0;
                        self.state = State::AfterBoundary;
                        return Ok(Some(Part {
                            content_type: self.content_type.take(),
                            body,
                        }));
                    }
                    None => {
                        self.scanned = self.buf.len().saturating_sub(self.delimiter_len - 1);
                        if self.buf.len() > self.max_part {
                            return Err(MultipartError::TooLarge(self.max_part));
                        }
                        self.fill_or("missing closing boundary").await?;
                    }
                },
            }
        }
    }
}

fn content_type(headers: &[u8]) -> Option<String> {
    String::from_utf8_lossy(headers)
        .split("\r\n")
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-type")
                .then(|| value.trim().to_owned())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn parts(
        body: &[u8],
        chunk: usize,
    ) -> Result<Vec<(Option<String>, Vec<u8>)>, MultipartError> {
        let chunks: Vec<Result<Bytes, std::convert::Infallible>> = body
            .chunks(chunk)
            .map(|c| Ok(Bytes::copy_from_slice(c)))
            .collect();
        let mut reader = MultipartReader::new(futures::stream::iter(chunks), "XyZ", 1 << 20);
        let mut out = Vec::new();
        while let Some(part) = reader.next_part().await? {
            out.push((part.content_type, part.body.to_vec()));
        }
        Ok(out)
    }

    const BODY: &[u8] = b"preamble\r\n--XyZ \r\nContent-Type: application/dicom\r\n\r\nfirst\r\n--XyZ\r\n\r\n\r\n--XyZ\r\ncontent-type: a/b\r\nX: y\r\n\r\nthird\r\n--Xy\r\n--XyZ--\r\nepilogue";

    #[tokio::test]
    async fn splits_parts_whatever_the_chunking() {
        for chunk in [1, 2, 3, 7, 64, BODY.len()] {
            let parts = parts(BODY, chunk).await.unwrap();
            assert_eq!(parts.len(), 3, "chunk size {chunk}");
            assert_eq!(
                parts[0],
                (Some("application/dicom".into()), b"first".to_vec())
            );
            assert_eq!(parts[1], (None, Vec::new()));
            assert_eq!(parts[2], (Some("a/b".into()), b"third\r\n--Xy".to_vec()));
        }
    }

    #[tokio::test]
    async fn boundary_at_start_of_body() {
        let parts = parts(b"--XyZ\r\n\r\nonly\r\n--XyZ--", 4).await.unwrap();
        assert_eq!(parts, vec![(None, b"only".to_vec())]);
    }

    #[tokio::test]
    async fn rejects_truncated_bodies() {
        assert!(parts(b"--XyZ\r\n\r\nunfinished", 3).await.is_err());
        assert!(parts(b"no boundary at all", 3).await.is_err());
    }
}
