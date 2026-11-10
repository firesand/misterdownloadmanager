//! Decoding yEnc, the encoding Usenet binaries are posted in: one article holds one part of a
//! file, with the part's place in the file in its own headers. The article body comes here
//! after NNTP's dot-unstuffing (see `nntp`).

use crate::Error;

/// One decoded article: part of a file, and where in that file it goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Part {
    /// The file's name, as the poster's `=ybegin` line gives it.
    pub name: String,
    /// Where `data` goes in the file, counted from 0.
    pub begin: u64,
    pub data: Vec<u8>,
    /// The whole file's size.
    pub file_size: u64,
    /// The part's number and the number of parts, when the poster says.
    pub part: Option<u32>,
    pub total: Option<u32>,
    /// `Some(true)` when the trailer's CRC-32 was checked and matched; `None` when the poster
    /// gave none. A mismatch is an error, never `Some(false)`.
    pub crc_ok: Option<bool>,
}

fn bad(what: impl Into<String>) -> Error {
    Error::Yenc(what.into())
}

/// The `key=value` fields of a header line. `name` runs to the end of the line, spaces and all,
/// so it is taken apart from the rest.
fn fields(line: &str) -> (Vec<(&str, &str)>, Option<&str>) {
    let (head, name) = match line.find(" name=") {
        Some(i) => (&line[..i], Some(line[i + 6..].trim())),
        None => (line, None),
    };
    let pairs = head
        .split_whitespace()
        .filter_map(|t| t.split_once('='))
        .collect();
    (pairs, name)
}

fn get<'a>(pairs: &[(&'a str, &'a str)], key: &str) -> Option<&'a str> {
    pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

fn number(pairs: &[(&str, &str)], key: &str) -> Result<u64, Error> {
    get(pairs, key)
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| bad(format!("the yEnc header has no valid {key}")))
}

/// The CRC-32 a trailer gives, when it gives a valid one: some posters leave it out.
fn checksum(value: Option<&str>) -> Option<u32> {
    let v = value?;
    if v.is_empty() || v.len() > 8 {
        return None;
    }
    u32::from_str_radix(v, 16).ok()
}

/// Lines of the article, the line ending (CRLF or LF) taken off.
struct Lines<'a> {
    body: &'a [u8],
    at: usize,
}

impl<'a> Lines<'a> {
    fn next_line(&mut self) -> Option<&'a [u8]> {
        if self.at >= self.body.len() {
            return None;
        }
        let rest = &self.body[self.at..];
        let (line, used) = match rest.iter().position(|b| *b == b'\n') {
            Some(i) => (&rest[..i], i + 1),
            None => (rest, rest.len()),
        };
        self.at += used;
        Some(line.strip_suffix(b"\r").unwrap_or(line))
    }

    fn peek_starts_with(&self, prefix: &[u8]) -> bool {
        self.body[self.at.min(self.body.len())..].starts_with(prefix)
    }
}

/// Decodes one article's body: the `=ybegin` line, `=ypart` for an article that is part of a
/// larger file, the data lines and `=yend`. Anything before `=ybegin` is ignored, as decoders
/// do. The data's length and CRC-32 are checked against the headers and trailer, so a damaged
/// article is never written as if it were good.
pub fn decode(body: &[u8]) -> Result<Part, Error> {
    let mut lines = Lines { body, at: 0 };
    let begin_line = loop {
        match lines.next_line() {
            Some(l) if l.starts_with(b"=ybegin ") => break l,
            Some(_) => {}
            None => return Err(bad("the article is not yEnc-encoded")),
        }
    };
    let begin_text = String::from_utf8_lossy(begin_line).into_owned();
    let (begin, name) = fields(&begin_text);
    let file_size = number(&begin, "size")?;
    let part_no = get(&begin, "part").and_then(|v| v.parse::<u32>().ok());
    let total = get(&begin, "total").and_then(|v| v.parse::<u32>().ok());

    let has_ypart = lines.peek_starts_with(b"=ypart");
    let (offset, expected) = if has_ypart {
        let line = lines.next_line().unwrap_or_default();
        let text = String::from_utf8_lossy(line).into_owned();
        let (part, _) = fields(&text);
        let first = number(&part, "begin")?;
        let last = number(&part, "end")?;
        if first < 1 || last < first || last > file_size {
            return Err(bad("the part lies outside its file"));
        }
        (first - 1, last - first + 1)
    } else if part_no.is_some() {
        return Err(bad("the yEnc part header is missing"));
    } else {
        (0, file_size)
    };

    // The decoded data is never longer than what encodes it.
    let mut data = Vec::with_capacity(expected.min(body.len() as u64) as usize);
    let mut trailer = None;
    while let Some(line) = lines.next_line() {
        if line.starts_with(b"=yend") {
            trailer = Some(String::from_utf8_lossy(line).into_owned());
            break;
        }
        let mut bytes = line.iter().copied();
        while let Some(mut b) = bytes.next() {
            if b == b'=' {
                // An escape ending a line escapes nothing; some encoders do that.
                let Some(next) = bytes.next() else { break };
                b = next.wrapping_sub(64);
            }
            data.push(b.wrapping_sub(42));
        }
        if data.len() as u64 > expected {
            return Err(bad("the article holds more data than it says"));
        }
    }
    let trailer = trailer.ok_or_else(|| bad("the article ends before its yEnc trailer"))?;
    let (end, _) = fields(&trailer);
    if data.len() as u64 != expected {
        return Err(bad(format!(
            "the article holds {} bytes, not the {expected} it says",
            data.len()
        )));
    }
    if let Some(size) = get(&end, "size").and_then(|v| v.parse::<u64>().ok())
        && size != expected
    {
        return Err(bad("the trailer's size differs from the part's"));
    }
    // A part's own checksum is `pcrc32`; a single-part file has only `crc32`.
    let wanted = if has_ypart || part_no.is_some() {
        checksum(get(&end, "pcrc32"))
    } else {
        checksum(get(&end, "crc32")).or_else(|| checksum(get(&end, "pcrc32")))
    };
    let crc_ok = match wanted {
        Some(crc) if crc != crc32fast::hash(&data) => {
            return Err(bad("the article's data does not match its checksum"));
        }
        Some(_) => Some(true),
        None => None,
    };
    Ok(Part {
        name: name.unwrap_or_default().to_owned(),
        begin: offset,
        data,
        file_size,
        part: part_no,
        total,
        crc_ok,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An encoder for the tests: 128 columns, CRLF, escaping what yEnc must (and a leading
    /// dot, as many posters do not).
    fn encode(
        name: &str,
        file: &[u8],
        begin: usize,
        end: usize,
        multi: Option<(u32, u32)>,
        escape_dots: bool,
    ) -> Vec<u8> {
        let part = &file[begin..end];
        let mut out = Vec::new();
        match multi {
            Some((n, t)) => out.extend(
                format!(
                    "=ybegin part={n} total={t} line=128 size={} name={name}\r\n",
                    file.len()
                )
                .bytes(),
            ),
            None => {
                out.extend(format!("=ybegin line=128 size={} name={name}\r\n", file.len()).bytes())
            }
        }
        if multi.is_some() {
            out.extend(format!("=ypart begin={} end={end}\r\n", begin + 1).bytes());
        }
        let mut col = 0;
        for (i, &b) in part.iter().enumerate() {
            let o = b.wrapping_add(42);
            let edge = col == 0 || i == part.len() - 1;
            let escape = matches!(o, 0 | 10 | 13 | b'=')
                || (edge && matches!(o, b'\t' | b' '))
                || (col == 0 && o == b'.' && escape_dots);
            if escape {
                out.push(b'=');
                out.push(o.wrapping_add(64));
                col += 2;
            } else {
                out.push(o);
                col += 1;
            }
            if col >= 128 {
                out.extend(b"\r\n");
                col = 0;
            }
        }
        if col > 0 {
            out.extend(b"\r\n");
        }
        let crc = crc32fast::hash(part);
        if multi.is_some() {
            out.extend(format!("=yend size={} part=1 pcrc32={crc:08x}\r\n", part.len()).bytes());
        } else {
            out.extend(format!("=yend size={} crc32={crc:08x}\r\n", part.len()).bytes());
        }
        out
    }

    fn sample(len: usize) -> Vec<u8> {
        // Every byte value appears, so every escape does.
        (0..len).map(|i| (i * 7 + i / 251) as u8).collect()
    }

    #[test]
    fn single_part_round_trip() {
        let file = sample(5000);
        let part = decode(&encode("my file.bin", &file, 0, file.len(), None, true)).unwrap();
        assert_eq!(part.name, "my file.bin");
        assert_eq!(part.begin, 0);
        assert_eq!(part.file_size, 5000);
        assert_eq!(part.data, file);
        assert_eq!(part.crc_ok, Some(true));
    }

    #[test]
    fn every_byte_value_survives() {
        let file: Vec<u8> = (0..=255u8).cycle().take(2000).collect();
        let part = decode(&encode("all.bin", &file, 0, file.len(), None, true)).unwrap();
        assert_eq!(part.data, file);
    }

    #[test]
    fn parts_land_at_their_offsets() {
        let file = sample(3000);
        let a = decode(&encode("f.bin", &file, 0, 1000, Some((1, 3)), true)).unwrap();
        let b = decode(&encode("f.bin", &file, 1000, 2000, Some((2, 3)), true)).unwrap();
        let c = decode(&encode("f.bin", &file, 2000, 3000, Some((3, 3)), true)).unwrap();
        assert_eq!((a.begin, b.begin, c.begin), (0, 1000, 2000));
        assert_eq!((b.part, b.total), (Some(2), Some(3)));
        assert_eq!(b.file_size, 3000);
        let mut joined = vec![0; 3000];
        for p in [&a, &b, &c] {
            joined[p.begin as usize..p.begin as usize + p.data.len()].copy_from_slice(&p.data);
        }
        assert_eq!(joined, file);
    }

    #[test]
    fn lines_starting_with_a_dot_decode_whether_or_not_they_are_escaped() {
        // 0x2e - 42 = 4: the byte that encodes to a dot.
        let file = vec![4u8; 400];
        for escape_dots in [true, false] {
            let article = encode("dotsbin", &file, 0, 400, None, escape_dots);
            let first = article.split(|b| *b == b'\n').nth(1).unwrap();
            assert_eq!(first.starts_with(b"."), !escape_dots);
            assert_eq!(decode(&article).unwrap().data, file);
        }
    }

    #[test]
    fn lf_only_line_endings_and_leading_text_are_accepted() {
        let file = sample(700);
        let crlf = encode("a.bin", &file, 0, 700, None, true);
        // The encoded bytes are not all UTF-8, so build the LF form on bytes.
        let mut raw = b"junk before\n".to_vec();
        let mut i = 0;
        while i < crlf.len() {
            if crlf[i] == b'\r' && crlf.get(i + 1) == Some(&b'\n') {
                i += 1;
                continue;
            }
            raw.push(crlf[i]);
            i += 1;
        }
        assert_eq!(decode(&raw).unwrap().data, file);
    }

    #[test]
    fn a_damaged_checksum_is_an_error() {
        let file = sample(1000);
        let mut article = encode("c.bin", &file, 0, 1000, None, true);
        // Flip one data byte after the header line, keeping the line ending sane.
        let at = article.iter().position(|b| *b == b'\n').unwrap() + 5;
        article[at] = if article[at] == b'A' { b'B' } else { b'A' };
        let err = decode(&article).unwrap_err();
        assert!(
            matches!(err, Error::Yenc(ref m) if m.contains("checksum")),
            "{err}"
        );
    }

    #[test]
    fn a_missing_checksum_is_not_an_error() {
        let article = b"=ybegin line=128 size=3 name=x\r\nKLM\r\n=yend size=3\r\n";
        let part = decode(article).unwrap();
        assert_eq!(part.data, [b'K' - 42, b'L' - 42, b'M' - 42]);
        assert_eq!(part.crc_ok, None);
    }

    #[test]
    fn malformed_articles_are_errors() {
        assert!(decode(b"").is_err());
        assert!(decode(b"hello\r\nworld\r\n").is_err());
        // No trailer.
        assert!(decode(b"=ybegin line=128 size=3 name=x\r\nKLM\r\n").is_err());
        // No size.
        assert!(decode(b"=ybegin line=128 name=x\r\nKLM\r\n=yend size=3\r\n").is_err());
        // Shorter than the header says.
        assert!(decode(b"=ybegin line=128 size=5 name=x\r\nKLM\r\n=yend size=5\r\n").is_err());
        // Longer than the header says.
        assert!(decode(b"=ybegin line=128 size=2 name=x\r\nKLM\r\n=yend size=2\r\n").is_err());
        // A part that says it is one but has no =ypart.
        assert!(
            decode(b"=ybegin part=1 total=2 size=3 name=x\r\nKLM\r\n=yend size=3\r\n").is_err()
        );
        // A part outside its file.
        let out = b"=ybegin part=1 total=1 size=3 name=x\r\n=ypart begin=3 end=9\r\nKLM\r\n=yend size=3\r\n";
        assert!(decode(out).is_err());
    }
}
