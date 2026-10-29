//! ZIP archives of stored instances, streamed as their files are read.
//!
//! Entries are stored without compression, as most of their bytes are pixel data, often
//! compressed already. Each entry's CRC-32 is only known once its data is sent, so it follows
//! the data in a data descriptor (APPNOTE 4.3.9) and no file is read twice. ZIP64 records are
//! used only for the sizes, offsets and counts that need them.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_stream::try_stream;
use axum::body::Body;
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use fortitude_store::Store;
use fortitude_store::retrieve::StoredInstance;
use futures::{Stream, StreamExt, stream};

/// Instance files up to this size are read in one operation, larger ones are streamed.
const WHOLE_FILE_MAX: u64 = 4 << 20;
/// Instance files read ahead of the one being sent.
const READ_AHEAD: usize = 4;

/// Sizes and offsets from this value on are stored in ZIP64 fields.
const MAX_32: u64 = 0xFFFF_FFFF;
/// Entry counts from this value on are stored in ZIP64 fields.
const MAX_16: usize = 0xFFFF;

const VERSION: u16 = 20;
const VERSION_ZIP64: u16 = 45;
/// Bit 3: CRC-32 and sizes are in the data descriptor.
const FLAGS: u16 = 0x0008;

struct Entry {
    name: String,
    offset: u64,
    size: u64,
    crc: u32,
}

impl Entry {
    /// Whether the local header and data descriptor have ZIP64 sizes.
    fn zip64(&self) -> bool {
        self.size >= MAX_32
    }
}

/// Writes the records of an archive whose entry data is sent between them.
pub struct ZipWriter {
    /// Bytes of the archive so far.
    offset: u64,
    entries: Vec<Entry>,
    time: u16,
    date: u16,
}

impl ZipWriter {
    /// An archive whose entries were last modified at `modified`.
    pub fn new(modified: SystemTime) -> Self {
        let (time, date) = dos_date_time(modified);
        Self {
            offset: 0,
            entries: Vec::new(),
            time,
            date,
        }
    }

    /// Size of the archive of entries of these names and sizes.
    pub fn length<'a>(entries: impl IntoIterator<Item = (&'a str, u64)>) -> u64 {
        let mut writer = Self::new(UNIX_EPOCH);
        for (name, size) in entries {
            writer.start(name, size);
            writer.end(0);
        }
        let end = writer.finish().len() as u64;
        writer.offset + end
    }

    /// Local header of an entry of `size` bytes, to be followed by its data then by
    /// [`end`](Self::end).
    pub fn start(&mut self, name: &str, size: u64) -> Vec<u8> {
        let entry = Entry {
            name: name.to_owned(),
            offset: self.offset,
            size,
            crc: 0,
        };
        let zip64 = entry.zip64();
        let mut out = Vec::with_capacity(30 + name.len() + 20);
        put32(&mut out, 0x0403_4b50);
        put16(&mut out, if zip64 { VERSION_ZIP64 } else { VERSION });
        put16(&mut out, FLAGS);
        put16(&mut out, 0); // stored
        put16(&mut out, self.time);
        put16(&mut out, self.date);
        put32(&mut out, 0); // CRC-32, in the data descriptor
        let unknown = if zip64 { u32::MAX } else { 0 };
        put32(&mut out, unknown);
        put32(&mut out, unknown);
        put16(&mut out, name.len() as u16);
        put16(&mut out, if zip64 { 20 } else { 0 });
        out.extend_from_slice(name.as_bytes());
        if zip64 {
            put16(&mut out, 0x0001);
            put16(&mut out, 16);
            put64(&mut out, 0);
            put64(&mut out, 0);
        }
        self.offset += out.len() as u64 + size;
        self.entries.push(entry);
        out
    }

    /// Data descriptor of the entry being written, once its data was sent.
    pub fn end(&mut self, crc: u32) -> Vec<u8> {
        let entry = self.entries.last_mut().expect("an entry was started");
        entry.crc = crc;
        let mut out = Vec::with_capacity(24);
        put32(&mut out, 0x0807_4b50);
        put32(&mut out, crc);
        if entry.zip64() {
            put64(&mut out, entry.size);
            put64(&mut out, entry.size);
        } else {
            put32(&mut out, entry.size as u32);
            put32(&mut out, entry.size as u32);
        }
        self.offset += out.len() as u64;
        out
    }

    /// Central directory and end records.
    pub fn finish(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for entry in &self.entries {
            let mut extra = Vec::new();
            if entry.size >= MAX_32 {
                put64(&mut extra, entry.size);
                put64(&mut extra, entry.size);
            }
            if entry.offset >= MAX_32 {
                put64(&mut extra, entry.offset);
            }
            let version = if extra.is_empty() {
                VERSION
            } else {
                VERSION_ZIP64
            };
            put32(&mut out, 0x0201_4b50);
            put16(&mut out, version); // made by
            put16(&mut out, version); // needed
            put16(&mut out, FLAGS);
            put16(&mut out, 0);
            put16(&mut out, self.time);
            put16(&mut out, self.date);
            put32(&mut out, entry.crc);
            put32(&mut out, entry.size.min(MAX_32) as u32);
            put32(&mut out, entry.size.min(MAX_32) as u32);
            put16(&mut out, entry.name.len() as u16);
            put16(
                &mut out,
                if extra.is_empty() {
                    0
                } else {
                    4 + extra.len() as u16
                },
            );
            put16(&mut out, 0); // comment
            put16(&mut out, 0); // disk
            put16(&mut out, 0); // internal attributes
            put32(&mut out, 0); // external attributes
            put32(&mut out, entry.offset.min(MAX_32) as u32);
            out.extend_from_slice(entry.name.as_bytes());
            if !extra.is_empty() {
                put16(&mut out, 0x0001);
                put16(&mut out, extra.len() as u16);
                out.extend_from_slice(&extra);
            }
        }

        let start = self.offset;
        let size = out.len() as u64;
        let count = self.entries.len();
        if count >= MAX_16 || size >= MAX_32 || start >= MAX_32 {
            let record = start + size;
            put32(&mut out, 0x0606_4b50);
            put64(&mut out, 44); // size of the rest of the record
            put16(&mut out, VERSION_ZIP64);
            put16(&mut out, VERSION_ZIP64);
            put32(&mut out, 0); // disk
            put32(&mut out, 0); // disk of the central directory
            put64(&mut out, count as u64);
            put64(&mut out, count as u64);
            put64(&mut out, size);
            put64(&mut out, start);
            put32(&mut out, 0x0706_4b50);
            put32(&mut out, 0); // disk of the ZIP64 end record
            put64(&mut out, record);
            put32(&mut out, 1); // disks
        }
        put32(&mut out, 0x0605_4b50);
        put16(&mut out, 0); // disk
        put16(&mut out, 0); // disk of the central directory
        put16(&mut out, count.min(MAX_16) as u16);
        put16(&mut out, count.min(MAX_16) as u16);
        put32(&mut out, size.min(MAX_32) as u32);
        put32(&mut out, start.min(MAX_32) as u32);
        put16(&mut out, 0); // comment
        out
    }
}

fn put16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// MS-DOS time and date (UTC) of `time`, within the years 1980 to 2107 they can hold.
fn dos_date_time(time: SystemTime) -> (u16, u16) {
    let seconds = time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (days, rest) = ((seconds / 86_400) as i64, seconds % 86_400);
    // Civil date of a day count (H. Hinnant's days_from_civil, inverted).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    if year < 1980 {
        return (0, (1 << 5) | 1);
    }
    if year > 2107 {
        return ((23 << 11) | (59 << 5) | 29, (127 << 9) | (12 << 5) | 31);
    }
    let time = ((rest / 3600) << 11) | ((rest / 60 % 60) << 5) | ((rest % 60) / 2);
    let date = ((year - 1980) << 9) | (month << 5) | day;
    (time as u16, date as u16)
}

enum Data {
    Whole(Bytes),
    /// Key of a file to stream.
    File(String),
}

/// A `200 OK` response streaming `instances` as a ZIP archive named `filename`, with entries
/// `{study}/{series}/{instance}.dcm`.
pub fn response(store: &Arc<Store>, instances: Vec<StoredInstance>, filename: &str) -> Response {
    let name = |i: &StoredInstance| format!("{}/{}/{}.dcm", i.study_uid, i.series_uid, i.sop_uid);
    let entries: Vec<_> = instances.iter().map(|i| (name(i), i.size)).collect();
    let length = ZipWriter::length(entries.iter().map(|(n, s)| (n.as_str(), *s)));

    let owned = store.clone();
    let data = stream::iter(instances)
        .map(move |instance| {
            let store = owned.clone();
            async move {
                if instance.size <= WHOLE_FILE_MAX {
                    Ok::<_, io::Error>(Data::Whole(store.files().read(&instance.path).await?))
                } else {
                    Ok(Data::File(instance.path))
                }
            }
        })
        .buffered(READ_AHEAD);
    let files = store.files().clone();
    let body = try_stream! {
        let mut zip = ZipWriter::new(SystemTime::now());
        futures::pin_mut!(data);
        for (name, size) in entries {
            let Some(data) = data.next().await else { break };
            yield Bytes::from(zip.start(&name, size));
            let mut crc = crc32fast::Hasher::new();
            let mut sent = 0;
            match data? {
                Data::Whole(bytes) => {
                    crc.update(&bytes);
                    sent += bytes.len() as u64;
                    yield bytes;
                }
                Data::File(path) => {
                    let mut chunks = files.stream(&path).await?;
                    while let Some(chunk) = chunks.next().await {
                        let chunk = chunk?;
                        crc.update(&chunk);
                        sent += chunk.len() as u64;
                        yield chunk;
                    }
                }
            }
            // The headers and the announced length depend on the indexed size.
            if sent != size {
                Err::<(), _>(io::Error::other(format!(
                    "{name}: {sent} bytes read, {size} expected"
                )))?;
            }
            yield Bytes::from(zip.end(crc.finalize()));
        }
        yield Bytes::from(zip.finish());
    };
    let body: Pin<Box<dyn Stream<Item = io::Result<Bytes>> + Send>> = Box::pin(body);
    let mut response = Body::from_stream(body).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/zip"),
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    if let Ok(value) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    response
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn u16_at(b: &[u8], at: usize) -> u16 {
        u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
    }

    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
    }

    fn u64_at(b: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
    }

    /// Names and data of the entries of a ZIP archive without ZIP64 records, read through
    /// its central directory, after checking their CRC-32 and data descriptors.
    pub(crate) fn read(zip: &[u8]) -> Vec<(String, Vec<u8>)> {
        let end = zip.len() - 22;
        assert_eq!(u32_at(zip, end), 0x0605_4b50);
        let count = usize::from(u16_at(zip, end + 10));
        let mut at = u32_at(zip, end + 16) as usize;
        assert_eq!(at + u32_at(zip, end + 12) as usize, end);
        let mut entries = Vec::new();
        for _ in 0..count {
            assert_eq!(u32_at(zip, at), 0x0201_4b50);
            let crc = u32_at(zip, at + 16);
            let size = u32_at(zip, at + 24) as usize;
            let name_len = usize::from(u16_at(zip, at + 28));
            let next = at + 46 + name_len + usize::from(u16_at(zip, at + 30));
            let name = String::from_utf8(zip[at + 46..at + 46 + name_len].to_vec()).unwrap();
            let local = u32_at(zip, at + 42) as usize;
            assert_eq!(u32_at(zip, local), 0x0403_4b50);
            assert_eq!(&zip[local + 30..local + 30 + name_len], name.as_bytes());
            let start = local + 30 + name_len + usize::from(u16_at(zip, local + 28));
            let data = zip[start..start + size].to_vec();
            assert_eq!(crc32fast::hash(&data), crc, "{name}");
            let descriptor = start + size;
            assert_eq!(u32_at(zip, descriptor), 0x0807_4b50);
            assert_eq!(u32_at(zip, descriptor + 4), crc);
            assert_eq!(u32_at(zip, descriptor + 8) as usize, size);
            entries.push((name, data));
            at = next;
        }
        entries
    }

    fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut zip = ZipWriter::new(SystemTime::now());
        let mut out = Vec::new();
        for (name, data) in entries {
            out.extend(zip.start(name, data.len() as u64));
            out.extend_from_slice(data);
            out.extend(zip.end(crc32fast::hash(data)));
        }
        out.extend(zip.finish());
        out
    }

    #[test]
    fn entries_are_read_back() {
        let entries: [(&str, &[u8]); 3] = [
            ("a/1.dcm", b"first"),
            ("a/2.dcm", b""),
            ("b.dcm", &[7; 1000]),
        ];
        let zip = archive(&entries);
        assert_eq!(
            ZipWriter::length(entries.iter().map(|(n, d)| (*n, d.len() as u64))),
            zip.len() as u64
        );
        let entries_read = read(&zip);
        assert_eq!(entries_read.len(), 3);
        for ((name, data), (read_name, read_data)) in entries.iter().zip(entries_read) {
            assert_eq!(*name, read_name);
            assert_eq!(*data, read_data);
        }
        assert_eq!(read(&archive(&[])), vec![]);
    }

    #[test]
    fn zip64_records_when_needed() {
        // Records only: the data of the entries is not written.
        let mut zip = ZipWriter::new(UNIX_EPOCH);
        let header = zip.start("big", 5 << 30);
        assert_eq!(u16_at(&header, 4), VERSION_ZIP64);
        assert_eq!(u32_at(&header, 22), u32::MAX);
        assert_eq!(u16_at(&header, 28), 20);
        assert_eq!(zip.end(1).len(), 24);
        assert_eq!(zip.start("after", 3).len(), 30 + 5);
        assert_eq!(zip.end(2).len(), 16);
        let directory = zip.finish();

        // Second entry: 32-bit sizes, ZIP64 offset.
        let second = 46 + 3 + 4 + 16;
        assert_eq!(u32_at(&directory, second), 0x0201_4b50);
        assert_eq!(u32_at(&directory, second + 42), u32::MAX);
        assert_eq!(
            u64_at(&directory, second + 46 + 5 + 4),
            (5 << 30) + 30 + 3 + 20 + 24
        );

        let end = directory.len() - 22;
        let locator = end - 20;
        assert_eq!(u32_at(&directory, locator), 0x0706_4b50);
        let record = locator - 56;
        assert_eq!(u32_at(&directory, record), 0x0606_4b50);
        assert_eq!(u64_at(&directory, record + 32), 2);
        assert_eq!(u64_at(&directory, record + 48), zip.offset);
        assert_eq!(u64_at(&directory, locator + 8), zip.offset + record as u64);
        assert_eq!(u32_at(&directory, end + 16), u32::MAX);
    }

    #[test]
    fn dos_dates() {
        let at = |s| UNIX_EPOCH + std::time::Duration::from_secs(s);
        // 2026-10-29 14:35:07 UTC.
        let (time, date) = dos_date_time(at(1_793_284_507));
        assert_eq!((date >> 9, date >> 5 & 15, date & 31), (46, 10, 29));
        assert_eq!((time >> 11, time >> 5 & 63, time & 31), (14, 35, 3));
        // 2000-02-29.
        let (_, date) = dos_date_time(at(951_782_400));
        assert_eq!((date >> 9, date >> 5 & 15, date & 31), (20, 2, 29));
        assert_eq!(dos_date_time(UNIX_EPOCH), (0, 33));
    }
}
