//! UM982 ASCII protocol. CRC starts at zero, with no final XOR (not IEEE CRC32).
use chrono::{NaiveDate, NaiveDateTime};

#[derive(Clone, Debug)]
pub struct Position {
    pub status: String,
    pub kind: String,
    pub lat: f64,
    pub lon: f64,
    pub height: f64,
    pub std: [f64; 3],
    pub tracked: u32,
    pub used: u32,
    pub speed: f64,
    pub course: f64,
    pub up: f64,
}
impl Position {
    pub fn valid(&self) -> bool {
        self.status == "SOL_COMPUTED" && self.kind != "NONE" && self.used > 0
    }
}
#[derive(Clone, Debug)]
pub struct Heading {
    pub status: String,
    pub kind: String,
    pub baseline: f64,
    pub angle: f64,
    pub pitch: f64,
    pub std: f64,
    pub dual: u32,
}
impl Heading {
    pub fn valid(&self) -> bool {
        self.status == "SOL_COMPUTED"
            && matches!(
                self.kind.as_str(),
                "NARROW_INT" | "L1_INT" | "NARROW_FLOAT" | "L1_FLOAT"
            )
            && self.std < 1.0
            && self.dual > 0
    }
}
#[derive(Clone, Debug)]
pub enum Message {
    Position(Position),
    Heading(Heading),
    Time(Option<NaiveDateTime>),
}
#[derive(Clone, Debug)]
pub struct Record {
    pub week: u32,
    pub tow_ms: u32,
    pub message: Message,
    pub raw: String,
}
impl Record {
    /// Heap allocations plus the record's slot in the session archive.
    pub fn retained_bytes(&self) -> usize {
        let strings = match &self.message {
            Message::Position(p) => p.status.capacity() + p.kind.capacity(),
            Message::Heading(h) => h.status.capacity() + h.kind.capacity(),
            Message::Time(_) => 0,
        };
        std::mem::size_of::<Self>() + self.raw.capacity() + strings
    }
}

#[derive(Default, Clone, Debug, PartialEq)]
pub struct Stats {
    pub valid: usize,
    pub crc_errors: usize,
    pub malformed: usize,
    pub ignored: usize,
}

pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0u32;
    for &byte in bytes {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ if crc & 1 != 0 { 0xedb88320 } else { 0 };
        }
    }
    crc
}
#[derive(Debug)]
enum Error {
    Crc,
    Malformed,
    Unsupported,
}
fn parse(raw: &[u8]) -> Result<Record, Error> {
    let text = std::str::from_utf8(raw)
        .map_err(|_| Error::Malformed)?
        .trim_end_matches(['\r', '\n']);
    let (body, checksum) = text
        .strip_prefix('#')
        .ok_or(Error::Malformed)?
        .rsplit_once('*')
        .ok_or(Error::Malformed)?;
    if checksum.len() != 8 {
        return Err(Error::Malformed);
    }
    let expected = u32::from_str_radix(checksum, 16).map_err(|_| Error::Malformed)?;
    if crc32(body.as_bytes()) != expected {
        return Err(Error::Crc);
    }
    let (header, payload) = body.split_once(';').ok_or(Error::Malformed)?;
    let h: Vec<_> = header.split(',').collect();
    let p: Vec<_> = payload.split(',').collect();
    let s = |i: usize| p.get(i).copied().ok_or(Error::Malformed);
    let n = |i: usize| -> Result<f64, Error> {
        let v: f64 = s(i)?.parse().map_err(|_| Error::Malformed)?;
        if v.is_finite() {
            Ok(v)
        } else {
            Err(Error::Malformed)
        }
    };
    let u = |i: usize| -> Result<u32, Error> { s(i)?.parse().map_err(|_| Error::Malformed) };
    let message = match h.first().copied() {
        Some("BESTNAVA") => {
            let pos = Position {
                status: s(0)?.into(),
                kind: s(1)?.into(),
                lat: n(2)?,
                lon: n(3)?,
                height: n(4)?,
                std: [n(7)?, n(8)?, n(9)?],
                tracked: u(13)?,
                used: u(14)?,
                speed: n(25)?,
                course: n(26)?,
                up: n(27)?,
            };
            if pos.lat.abs() > 90.0
                || pos.lon.abs() > 180.0
                || pos.speed < 0.0
                || pos.std.iter().any(|x| *x < 0.0)
            {
                return Err(Error::Malformed);
            }
            Message::Position(pos)
        }
        Some("UNIHEADINGA") => {
            let v = Heading {
                status: s(0)?.into(),
                kind: s(1)?.into(),
                baseline: n(2)?,
                angle: n(3)?,
                pitch: n(4)?,
                std: n(6)?,
                dual: u(12)?,
            };
            if !(0.0..=360.0).contains(&v.angle) || v.std < 0.0 {
                return Err(Error::Malformed);
            }
            Message::Heading(v)
        }
        Some("RECTIMEA") => {
            let time = if s(10)? == "VALID" {
                Some(
                    NaiveDate::from_ymd_opt(u(4)? as i32, u(5)?, u(6)?)
                        .and_then(|d| {
                            d.and_hms_milli_opt(
                                u(7).ok()?,
                                u(8).ok()?,
                                u(9).ok()? / 1000,
                                u(9).ok()? % 1000,
                            )
                        })
                        .ok_or(Error::Malformed)?,
                )
            } else {
                None
            };
            Message::Time(time)
        }
        _ => return Err(Error::Unsupported),
    };
    Ok(Record {
        week: h
            .get(4)
            .ok_or(Error::Malformed)?
            .parse()
            .map_err(|_| Error::Malformed)?,
        tow_ms: h
            .get(5)
            .ok_or(Error::Malformed)?
            .parse()
            .map_err(|_| Error::Malformed)?,
        message,
        raw: text.into(),
    })
}

/// DAT files contain one independent record per line. Never join broken lines
/// or salvage a frame from the middle of a line (unlike the serial stream).
pub fn decode_log(bytes: &[u8]) -> (Vec<Record>, Stats) {
    let mut records = Vec::new();
    let mut stats = Stats::default();
    for line in bytes.split(|b| *b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        match parse(line) {
            Ok(record) => {
                stats.valid += 1;
                records.push(record);
            }
            Err(Error::Crc) => stats.crc_errors += 1,
            Err(Error::Malformed) => stats.malformed += 1,
            Err(Error::Unsupported) => stats.ignored += 1,
        }
    }
    (records, stats)
}

#[derive(Default)]
pub struct Decoder {
    buffer: Vec<u8>,
    pub stats: Stats,
}
impl Decoder {
    /// Byte-wise framing bounds memory and resynchronizes on a new '#'.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Record> {
        let mut out = Vec::new();
        for &b in bytes {
            if b == b'#' {
                if !self.buffer.is_empty() {
                    self.stats.malformed += 1;
                }
                self.buffer.clear();
                self.buffer.push(b);
            } else if !self.buffer.is_empty() {
                self.buffer.push(b);
                if self.buffer.ends_with(b"\r\n") {
                    match parse(&self.buffer) {
                        Ok(r) => {
                            self.stats.valid += 1;
                            out.push(r);
                        }
                        Err(Error::Crc) => self.stats.crc_errors += 1,
                        Err(Error::Malformed) => self.stats.malformed += 1,
                        Err(Error::Unsupported) => self.stats.ignored += 1,
                    }
                    self.buffer.clear();
                } else if self.buffer.len() > 4096 {
                    self.stats.malformed += 1;
                    self.buffer.clear();
                }
            }
        }
        out
    }
    pub fn finish(&mut self) {
        if !self.buffer.is_empty() {
            self.stats.malformed += 1;
            self.buffer.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const LOG: &[u8] = crate::test_support::LOG;
    #[test]
    fn arbitrary_chunks_match() {
        let mut d = Decoder::default();
        let expected = d.feed(LOG);
        d.finish();
        let stats = d.stats;
        assert_eq!(expected.len(), 3);
        for size in [1, 7, 256, 400, 512, 1024] {
            let mut d = Decoder::default();
            let mut records = Vec::new();
            for chunk in LOG.chunks(size) {
                records.extend(d.feed(chunk));
            }
            d.finish();
            assert_eq!(records.len(), expected.len());
            assert_eq!(d.stats, stats);
        }
        assert_eq!(stats.crc_errors, 0);
    }
    #[test]
    fn corruption_and_resync() {
        let line = LOG
            .split_inclusive(|b| *b == b'\n')
            .find(|l| l.starts_with(b"#"))
            .unwrap();
        let mut bad = line.to_vec();
        bad[20] ^= 1;
        let mut d = Decoder::default();
        assert!(d.feed(&bad).is_empty());
        assert_eq!(d.stats.crc_errors, 1);
        d.feed(b"#broken");
        assert_eq!(d.feed(line).len(), 1);
        assert_eq!(d.stats.malformed, 1);
    }
    #[test]
    fn log_lines_filter_incomplete_content() {
        let first = LOG.split(|b| *b == b'\n').next().unwrap();
        assert!(!first.starts_with(b"#"));
        let (records, stats) = decode_log(LOG);
        assert_eq!(records.len(), 3);
        assert_eq!(stats.malformed, 1); // First line is only a previous frame's tail.
        assert!(records[0].raw.starts_with("#UNIHEADINGA,"));

        let good = LOG
            .split_inclusive(|b| *b == b'\n')
            .find(|l| l.starts_with(b"#"))
            .unwrap();
        let mut input = b"#BESTNAVA,incomplete\n".to_vec();
        input.extend_from_slice(&good[..good.len() / 2]);
        input.push(b'\n');
        input.extend_from_slice(&good[good.len() / 2..]);
        input.extend_from_slice(b"noise");
        input.extend_from_slice(good);
        input.extend_from_slice(good);
        input.extend_from_slice(b"#UNIHEADINGA,truncated");
        let (records, stats) = decode_log(&input);
        assert_eq!(records.len(), 1); // Only the standalone, complete line survives.
        assert_eq!(stats.malformed, 5);
    }
    #[test]
    fn all_logs_parse() {
        // Original captures are optional, local-only data; the fixture tests always run.
        for entry in std::fs::read_dir("data").into_iter().flatten().flatten() {
            if entry.path().extension().is_some_and(|s| s == "DAT") {
                let (records, stats) = decode_log(&std::fs::read(entry.path()).unwrap());
                assert!(!records.is_empty());
                assert_eq!(stats.crc_errors, 0, "{:?}", entry.path());
                println!("{:?}: {:?}", entry.file_name(), stats);
            }
        }
    }
}
