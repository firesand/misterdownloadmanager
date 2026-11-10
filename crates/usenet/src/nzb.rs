//! Reading an NZB: the list of articles (message ids) that make up each file of a Usenet post,
//! and the file names to give them.
//!
//! An NZB is XML, written by indexers and posting tools of every quality, so the reader is
//! forgiving where it can be (a stray `&`, a missing size) and strict where a wrong guess would
//! hurt (a message id that would break the NNTP command, a file name naming another folder).
//! `roxmltree` never fetches external entities, and the usual `<!DOCTYPE nzb ...>` line is
//! allowed.

use std::collections::HashSet;

use mdm_engine::naming;
use serde::{Deserialize, Serialize};

use crate::Error;

/// One article as the NZB lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    /// Its place in its file, from 1.
    pub number: u32,
    /// Its size as posted: the encoded article, a little more than the data it holds.
    pub bytes: u64,
    /// Without the angle brackets.
    pub message_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NzbFile {
    pub subject: String,
    /// What to save it as: from the subject, safe in any folder on any system, and different
    /// from the other files' names of this NZB.
    pub name: String,
    pub poster: String,
    /// Seconds since 1970, as posted.
    pub date: Option<u64>,
    pub groups: Vec<String>,
    /// By number, without duplicates.
    pub segments: Vec<Segment>,
}

impl NzbFile {
    /// The posted size of its articles (the file itself is a little smaller).
    pub fn size(&self) -> u64 {
        self.segments.iter().map(|s| s.bytes).sum()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Nzb {
    pub files: Vec<NzbFile>,
    /// The archive password the poster put in the NZB's head, if any.
    pub password: Option<String>,
    /// What to call the download: the NZB's `name` or `title` meta, or what the caller sets (it
    /// also names the resume file, so give each NZB in one folder its own).
    pub name: Option<String>,
}

impl Nzb {
    pub fn size(&self) -> u64 {
        self.files.iter().map(NzbFile::size).sum()
    }
}

/// Reads an NZB. Fails when the text is not one or lists nothing to download.
pub fn parse(text: &str) -> Result<Nzb, Error> {
    let text = text.trim_start_matches('\u{feff}');
    let options = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..Default::default()
    };
    let fixed: String;
    let doc = match roxmltree::Document::parse_with_options(text, options) {
        Ok(doc) => doc,
        Err(first) => {
            // Posting tools sometimes leave a bare `&` in a subject.
            fixed = escape_bare_ampersands(text);
            roxmltree::Document::parse_with_options(&fixed, options)
                .map_err(|_| Error::Nzb(first.to_string()))?
        }
    };
    let root = doc.root_element();
    if !root.tag_name().name().eq_ignore_ascii_case("nzb") {
        return Err(Error::Nzb("the document is not an <nzb>".into()));
    }
    let text_of = |n: roxmltree::Node| -> String {
        n.children()
            .filter_map(|c| c.text())
            .collect::<String>()
            .trim()
            .to_owned()
    };
    let named = |n: &roxmltree::Node, name: &str| {
        n.is_element() && n.tag_name().name().eq_ignore_ascii_case(name)
    };

    let mut nzb = Nzb::default();
    for meta in root.descendants().filter(|n| named(n, "meta")) {
        let value = text_of(meta);
        if value.is_empty() {
            continue;
        }
        match meta
            .attribute("type")
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("password") => nzb.password = Some(value),
            Some("name" | "title") if nzb.name.is_none() => nzb.name = Some(value),
            _ => {}
        }
    }

    let mut index = 0;
    for file in root.descendants().filter(|n| named(n, "file")) {
        index += 1;
        let mut segments: Vec<Segment> = Vec::new();
        for seg in file.descendants().filter(|n| named(n, "segment")) {
            let Some(number) = seg.attribute("number").and_then(|v| v.trim().parse().ok()) else {
                continue;
            };
            let id = text_of(seg);
            let id = id.trim_start_matches('<').trim_end_matches('>');
            // A message id holding whitespace or brackets would break the command.
            if number == 0
                || id.is_empty()
                || id.contains(|c: char| c.is_whitespace() || "<>".contains(c))
            {
                continue;
            }
            if segments.iter().any(|s| s.number == number) {
                continue;
            }
            // Every article is at least a byte: a missing or bad size counts as that.
            let bytes = seg
                .attribute("bytes")
                .and_then(|v| v.trim().parse::<u64>().ok())
                .filter(|b| *b > 0)
                .unwrap_or(1);
            segments.push(Segment {
                number,
                bytes,
                message_id: id.to_owned(),
            });
        }
        if segments.is_empty() {
            continue;
        }
        segments.sort_by_key(|s| s.number);
        let subject = file.attribute("subject").unwrap_or_default().to_owned();
        let groups = file
            .descendants()
            .filter(|n| named(n, "group"))
            .map(text_of)
            .filter(|g| !g.is_empty())
            .collect();
        nzb.files.push(NzbFile {
            name: safe_name(&name_from_subject(&subject), index),
            subject,
            poster: file.attribute("poster").unwrap_or_default().to_owned(),
            date: file.attribute("date").and_then(|d| d.trim().parse().ok()),
            groups,
            segments,
        });
    }
    if nzb.files.is_empty() {
        return Err(Error::Nzb("it lists no files to download".into()));
    }
    let names = unique_names(nzb.files.iter().map(|f| f.name.clone()).collect());
    for (file, name) in nzb.files.iter_mut().zip(names) {
        file.name = name;
    }
    Ok(nzb)
}

/// `&` that starts no entity, written as `&amp;`.
fn escape_bare_ampersands(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    let mut rest = text;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let is_entity = tail.find(';').is_some_and(|end| {
            let name = &tail[1..end];
            end <= 12
                && !name.is_empty()
                && (name.strip_prefix('#').is_some_and(|d| {
                    !d.is_empty()
                        && d.trim_start_matches(['x', 'X'])
                            .chars()
                            .all(|c| c.is_ascii_hexdigit())
                }) || name.chars().all(|c| c.is_ascii_alphanumeric()))
        });
        out.push_str(if is_entity { "&" } else { "&amp;" });
        rest = &tail[1..];
    }
    out.push_str(rest);
    out
}

/// A file's name from its subject. Posters quote it, as in
/// `[01/20] - "Name.part01.rar" yEnc (1/50)`; without quotes it is a guess at what is left once
/// the article count and the `yEnc` marker are taken off.
fn name_from_subject(subject: &str) -> String {
    if let Some(start) = subject.find('"')
        && let Some(len) = subject[start + 1..].find('"')
    {
        let quoted = subject[start + 1..start + 1 + len].trim();
        if !quoted.is_empty() {
            return quoted.to_owned();
        }
    }
    let tokens: Vec<&str> = subject.split_whitespace().collect();
    for (i, token) in tokens.iter().enumerate() {
        let candidate = strip_count(token);
        let counted_here = candidate.len() != token.len();
        let next = tokens.get(i + 1);
        let ends_name = counted_here
            || next.is_none_or(|n| n.starts_with("yEnc") || n.starts_with("yenc") || is_count(n));
        if ends_name && !candidate.contains('"') && has_extension(candidate) {
            return candidate.to_owned();
        }
    }
    let mut name = subject;
    if let Some(i) = name.find(" yEnc").or_else(|| name.find(" yenc")) {
        name = &name[..i];
    }
    strip_count(name.trim()).trim().to_owned()
}

/// `(12/50)` at the end of `s`, removed.
fn strip_count(s: &str) -> &str {
    if let Some(open) = s.rfind('(')
        && is_count(&s[open..])
    {
        return s[..open].trim_end();
    }
    s
}

/// `(12/50)`.
fn is_count(s: &str) -> bool {
    let Some(inner) = s.strip_prefix('(').and_then(|s| s.strip_suffix(')')) else {
        return false;
    };
    inner.split_once('/').is_some_and(|(a, b)| {
        !a.is_empty()
            && !b.is_empty()
            && a.bytes().all(|c| c.is_ascii_digit())
            && b.bytes().all(|c| c.is_ascii_digit())
    })
}

/// Something before a dot, then two to five letters or digits.
fn has_extension(s: &str) -> bool {
    s.rfind('.').is_some_and(|dot| {
        let ext = &s[dot + 1..];
        dot > 0 && (2..=5).contains(&ext.len()) && ext.bytes().all(|c| c.is_ascii_alphanumeric())
    })
}

/// `raw` as a name to save under in the download's folder: nothing that names another folder,
/// no character Windows refuses, no reserved name, not too long (the engine's rules for every
/// file name). `index` names a file the subject says nothing about.
fn safe_name(raw: &str, index: usize) -> String {
    let raw = raw.trim();
    if raw.trim_matches(['.', ' ']).is_empty() {
        return format!("file-{index}");
    }
    let mut name = naming::sanitize(raw);
    // Separators are gone; a `..` inside a name is harmless but never wanted.
    while name.contains("..") {
        name = name.replace("..", ".");
    }
    if name.is_empty() || name == "." {
        return format!("file-{index}");
    }
    name
}

/// "Name (2).ext" for a second file of the same name, compared without letter case as macOS and
/// Windows compare.
fn unique_names(names: Vec<String>) -> Vec<String> {
    let mut taken = HashSet::new();
    names
        .into_iter()
        .map(|name| {
            let mut candidate = name.clone();
            let mut n = 2;
            while !taken.insert(candidate.to_lowercase()) {
                candidate = match name.rfind('.').filter(|d| *d > 0) {
                    Some(dot) => format!("{} ({n}){}", &name[..dot], &name[dot..]),
                    None => format!("{name} ({n})"),
                };
                n += 1;
            }
            candidate
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(files: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE nzb PUBLIC "-//newzBin//DTD NZB 1.1//EN" "http://www.newzbin.com/DTD/nzb/nzb-1.1.dtd">
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
<head><meta type="password">s3cret</meta><meta type="title">My Post</meta></head>
{files}
</nzb>"#
        )
    }

    fn file(subject: &str, segs: &str) -> String {
        format!(
            r#"<file poster="me@x.y" date="1700000000" subject="{subject}"><groups><group>alt.binaries.test</group></groups><segments>{segs}</segments></file>"#
        )
    }

    #[test]
    fn reads_files_password_and_name() {
        let nzb = parse(&doc(&file(
            "[1/2] - &quot;a.rar&quot; yEnc (1/2)",
            r#"<segment bytes="100" number="1">one@x</segment><segment bytes="50" number="2">&lt;two@x&gt;</segment>"#,
        )))
        .unwrap();
        assert_eq!(nzb.password.as_deref(), Some("s3cret"));
        assert_eq!(nzb.name.as_deref(), Some("My Post"));
        let f = &nzb.files[0];
        assert_eq!(f.name, "a.rar");
        assert_eq!(f.poster, "me@x.y");
        assert_eq!(f.date, Some(1_700_000_000));
        assert_eq!(f.groups, ["alt.binaries.test"]);
        assert_eq!(f.size(), 150);
        assert_eq!(nzb.size(), 150);
        assert_eq!(
            f.segments[1].message_id, "two@x",
            "brackets are not part of the id"
        );
    }

    #[test]
    fn names_from_subjects() {
        let n = |s| name_from_subject(s);
        assert_eq!(
            n(r#"[01/20] - "Name.part01.rar" yEnc (1/50)"#),
            "Name.part01.rar"
        );
        assert_eq!(n(r#"x "  spaced name.mkv " y"#), "spaced name.mkv");
        assert_eq!(n("Some.Release.mkv yEnc (1/9)"), "Some.Release.mkv");
        assert_eq!(n("[3/9] Some.Release.r01 (1/9)"), "Some.Release.r01");
        assert_eq!(n("Some.Release.mkv"), "Some.Release.mkv");
        assert_eq!(n("plain words yEnc (1/2)"), "plain words");
        assert_eq!(n(""), "");
    }

    #[test]
    fn a_file_name_never_names_another_folder() {
        for raw in [
            "../../etc/passwd",
            r"..\..\evil.exe",
            "a/b/c.txt",
            "..",
            ".",
            "x:y*z?.bin",
            "CON.txt",
        ] {
            let name = safe_name(raw, 7);
            assert!(
                !name.contains(['/', '\\', ':', '*', '?']),
                "{raw} -> {name}"
            );
            assert!(!name.contains(".."), "{raw} -> {name}");
            assert!(!name.is_empty() && name != ".", "{raw} -> {name}");
        }
        assert_eq!(safe_name("..", 7), "file-7");
        assert_eq!(safe_name("  ", 3), "file-3");
        assert_eq!(safe_name("CON.txt", 1), "_CON.txt");
        assert_eq!(safe_name("a/b.txt", 1), "a_b.txt");
    }

    #[test]
    fn segments_are_sorted_and_deduplicated_and_bad_ones_dropped() {
        let segs = r#"
            <segment bytes="3" number="3">c@x</segment>
            <segment bytes="1" number="1">a@x</segment>
            <segment bytes="2" number="2">b@x</segment>
            <segment bytes="9" number="2">again@x</segment>
            <segment bytes="9" number="0">zero@x</segment>
            <segment bytes="9" number="4">has space@x</segment>
            <segment bytes="9" number="5">  </segment>
            <segment bytes="x" number="6">six@x</segment>"#;
        let nzb = parse(&doc(&file("&quot;f.bin&quot;", segs))).unwrap();
        let got: Vec<_> = nzb.files[0]
            .segments
            .iter()
            .map(|s| (s.number, s.bytes, s.message_id.as_str()))
            .collect();
        assert_eq!(
            got,
            [(1, 1, "a@x"), (2, 2, "b@x"), (3, 3, "c@x"), (6, 1, "six@x")]
        );
    }

    #[test]
    fn same_names_are_numbered() {
        let one = r#"<segment bytes="1" number="1">a@x</segment>"#;
        let files = [
            file("&quot;a.rar&quot;", one),
            file("&quot;A.RAR&quot;", one),
            file("&quot;noext&quot;", one),
            file("&quot;noext&quot;", one),
        ]
        .concat();
        let nzb = parse(&doc(&files)).unwrap();
        let names: Vec<_> = nzb.files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["a.rar", "A (2).RAR", "noext", "noext (2)"]);
    }

    #[test]
    fn a_subject_without_a_name_gets_a_numbered_one() {
        let one = r#"<segment bytes="1" number="1">a@x</segment>"#;
        let nzb = parse(&doc(
            &[file("", one), file("&quot;b.bin&quot;", one)].concat()
        ))
        .unwrap();
        assert_eq!(nzb.files[0].name, "file-1");
    }

    #[test]
    fn a_bare_ampersand_is_forgiven() {
        let one = r#"<segment bytes="1" number="1">a@x</segment>"#;
        let nzb = parse(&doc(&file("&quot;Tom & Jerry &amp; co.avi&quot;", one))).unwrap();
        assert_eq!(nzb.files[0].name, "Tom & Jerry & co.avi");
    }

    #[test]
    fn rejects_what_is_not_an_nzb() {
        assert!(parse("").is_err());
        assert!(parse("just some text").is_err());
        assert!(parse("<html><body>sign in</body></html>").is_err());
        assert!(parse("<nzb></nzb>").is_err());
        let no_segments = r#"<nzb><file subject="&quot;a.bin&quot;"><segments/></file></nzb>"#;
        assert!(matches!(parse(no_segments), Err(Error::Nzb(_))));
    }

    #[test]
    fn a_document_type_with_entities_is_not_expanded_into_files() {
        // External entities are never read: the reference stays unresolved and the file's name
        // is just what the subject says.
        let xml = r#"<?xml version="1.0"?>
<!DOCTYPE nzb [<!ENTITY x SYSTEM "file:///etc/passwd">]>
<nzb><file subject="&quot;a.bin&quot;"><segments><segment bytes="1" number="1">a@x</segment></segments></file></nzb>"#;
        let nzb = parse(xml).unwrap();
        assert_eq!(nzb.files[0].name, "a.bin");
    }
}
