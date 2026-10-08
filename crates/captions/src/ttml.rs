//! TTML (W3C Timed Text Markup Language): IMSC1 Text profile (`.ttml`, written per TTML2 /
//! IMSC 1.1 Text) and DFXP (`.dfxp`, TTML1's Distribution Format Exchange Profile).
//!
//! **Writing.** One `<p>` per cue inside `body/div`, lines separated by `<br/>`; `<i>`, `<b>` and
//! `<u>` become `<span>`s with `tts:fontStyle="italic"`, `tts:fontWeight="bold"` and
//! `tts:textDecoration="underline"`. IMSC1 files signal `ttp:profile` (the IMSC1 Text profile
//! designator), a bottom region and a default style. With a frame rate, IMSC1 times are frame
//! offsets (`123f`) at `ttp:frameRate` / `ttp:frameRateMultiplier`, which are exact; without
//! one, and always for DFXP (for older players), times are clock times with milliseconds.
//!
//! **Reading** is namespace-agnostic (TTML1 2010, the 2006 `ttaf1` drafts and TTML2 all read):
//! clock times (`HH:MM:SS`, `.fraction`, `:FF` frames and `.subframes`), offset times (`h`, `m`,
//! `s`, `ms`, `f`, `t` with `ttp:tickRate`), `begin` / `end` / `dur`, nested time containers
//! (`body` / `div` begins add up), timed `<span>`s inside an untimed `<p>`, `<br/>`, whitespace
//! collapsing, styles referenced by id (italic / bold / underline) and `ttm:agent` / `xml:id`.

use filmcraft_time::{FrameRate, TICKS_PER_SECOND, Tick};

use crate::{Cue, Document, Error, Result, TICKS_PER_MS};

/// Which TTML flavour to write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavor {
    /// IMSC1 Text profile (TTML2 namespaces).
    Imsc1,
    /// DFXP (TTML1).
    Dfxp,
}

const NS_TT: &str = "http://www.w3.org/ns/ttml";
const NS_TTP: &str = "http://www.w3.org/ns/ttml#parameter";
const NS_TTS: &str = "http://www.w3.org/ns/ttml#styling";
const NS_TTM: &str = "http://www.w3.org/ns/ttml#metadata";
const IMSC1_TEXT: &str = "http://www.w3.org/ns/ttml/profile/imsc1/text";
const DFXP_PROFILE: &str = "http://www.w3.org/ns/ttml/profile/dfxp-presentation";

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            c => o.push(c),
        }
    }
    o
}

fn clock(t: Tick) -> String {
    let ms = (t.0.max(0) + TICKS_PER_MS / 2) / TICKS_PER_MS;
    format!("{:02}:{:02}:{:02}.{:03}", ms / 3_600_000, (ms / 60_000) % 60, (ms / 1000) % 60, ms % 1000)
}

/// Time expression for `t`: frames at `rate` when it is on a frame boundary, else clock time.
fn time_expr(t: Tick, rate: Option<FrameRate>) -> String {
    match rate {
        Some(r) if r.tick_of(r.frame_at(t)) == t => format!("{}f", r.frame_at(t)),
        _ => clock(t),
    }
}

/// Caption text (`\n` lines, `<i>`/`<b>`/`<u>` tags) → TTML inline content.
fn inline(text: &str) -> String {
    let mut out = String::new();
    let mut open: Vec<&str> = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('<')
            && let Some(end) = after.find('>')
        {
            let tag = after[..end].trim().to_ascii_lowercase();
            let (closing, name) = match tag.strip_prefix('/') {
                Some(n) => (true, n.trim().to_string()),
                None => (false, tag.split_whitespace().next().unwrap_or("").to_string()),
            };
            let attr = match name.as_str() {
                "i" => Some("tts:fontStyle=\"italic\""),
                "b" => Some("tts:fontWeight=\"bold\""),
                "u" => Some("tts:textDecoration=\"underline\""),
                _ => None,
            };
            if let Some(a) = attr {
                if closing {
                    if let Some(pos) = open.iter().rposition(|x| *x == a) {
                        // close spans opened after it, then it, then reopen the inner ones
                        let inner: Vec<&str> = open.drain(pos..).skip(1).collect();
                        for _ in 0..=inner.len() {
                            out.push_str("</span>");
                        }
                        for x in inner {
                            out.push_str(&format!("<span {x}>"));
                            open.push(x);
                        }
                    }
                } else {
                    out.push_str(&format!("<span {a}>"));
                    open.push(a);
                }
            }
            rest = &after[end + 1..];
            continue;
        }
        let Some(ch) = rest.chars().next() else { break };
        if ch == '\n' {
            out.push_str("<br/>");
        } else {
            out.push_str(&esc(&ch.to_string()));
        }
        rest = &rest[ch.len_utf8()..];
    }
    for _ in open {
        out.push_str("</span>");
    }
    out
}

fn is_ncname(s: &str) -> bool {
    let mut c = s.chars();
    c.next().is_some_and(|f| f.is_alphabetic() || f == '_') && c.all(|x| x.is_alphanumeric() || matches!(x, '_' | '-' | '.'))
}

/// Write `doc` as TTML. `rate` (IMSC1 only) makes times exact frame offsets; `lang` is the
/// document language (`xml:lang`).
pub fn write(doc: &Document, flavor: Flavor, rate: Option<FrameRate>, lang: &str) -> String {
    let mut o = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    let rate = if flavor == Flavor::Imsc1 { rate } else { None };
    o.push_str(&format!("<tt xmlns=\"{NS_TT}\" xmlns:ttp=\"{NS_TTP}\" xmlns:tts=\"{NS_TTS}\" xmlns:ttm=\"{NS_TTM}\""));
    o.push_str(&format!(" ttp:profile=\"{}\" ttp:timeBase=\"media\"", if flavor == Flavor::Imsc1 { IMSC1_TEXT } else { DFXP_PROFILE }));
    if let Some(r) = rate {
        let base = r.timecode_base();
        o.push_str(&format!(" ttp:frameRate=\"{base}\""));
        // effective rate = base × multiplier
        let (mn, md) = (r.num, r.den * base);
        let g = gcd(mn, md);
        if (mn / g, md / g) != (1, 1) {
            o.push_str(&format!(" ttp:frameRateMultiplier=\"{} {}\"", mn / g, md / g));
        }
    }
    o.push_str(&format!(" xml:lang=\"{}\">\n", esc(if lang.is_empty() { "en" } else { lang })));
    o.push_str("  <head>\n");
    o.push_str("    <styling>\n      <style xml:id=\"s1\" tts:color=\"white\" tts:fontFamily=\"proportionalSansSerif\" tts:fontSize=\"100%\" tts:textAlign=\"center\"/>\n    </styling>\n");
    o.push_str("    <layout>\n      <region xml:id=\"r1\" tts:origin=\"10% 10%\" tts:extent=\"80% 80%\" tts:displayAlign=\"after\"/>\n    </layout>\n");
    o.push_str("  </head>\n  <body region=\"r1\" style=\"s1\">\n    <div>\n");
    for (i, c) in doc.cues.iter().enumerate() {
        let id = c.id.as_deref().filter(|x| is_ncname(x)).map(str::to_string).unwrap_or_else(|| format!("c{}", i + 1));
        let agent = c.speaker.as_deref().map(|s| format!(" ttm:role=\"x-speaker\" ttm:agent=\"{}\"", esc(&agent_id(s)))).unwrap_or_default();
        o.push_str(&format!(
            "      <p xml:id=\"{}\" begin=\"{}\" end=\"{}\"{agent}>{}</p>\n",
            esc(&id),
            time_expr(c.start, rate),
            time_expr(c.end, rate),
            inline(&c.text)
        ));
    }
    o.push_str("    </div>\n  </body>\n</tt>\n");
    o
}

/// `ttm:agent` refers to an agent id; we write the speaker name itself (made an NCName).
fn agent_id(s: &str) -> String {
    let v: String = s.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '.' { c } else { '_' }).collect();
    if v.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_') { v } else { format!("_{v}") }
}

fn gcd(a: i64, b: i64) -> i64 {
    if b == 0 { a.abs().max(1) } else { gcd(b, a % b) }
}

// ------------------------------------------------------------------------------------------------
// Reader
// ------------------------------------------------------------------------------------------------

/// Timing parameters from the root element.
#[derive(Clone, Copy)]
struct Timing {
    /// Effective frame rate (frameRate × multiplier).
    rate: FrameRate,
    /// Nominal frames per second for `HH:MM:SS:FF`.
    base: i64,
    sub_frame_rate: i64,
    tick_rate: i64,
}

fn positive_integer(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<i64>().ok().filter(|v| *v > 0)
}

fn parse_ratio(s: &str) -> Option<(i64, i64)> {
    let mut it = s.split_whitespace().map(positive_integer);
    let a = it.next()??;
    let b = it.next()??;
    it.next().is_none().then_some((a, b))
}

fn attr<'a>(n: roxmltree::Node<'a, '_>, local: &str) -> Option<&'a str> {
    n.attributes().find(|a| a.name() == local).map(|a| a.value())
}

fn timing(root: roxmltree::Node) -> Result<Timing> {
    let invalid =
        |name: &str| Error::Syntax { line: root.document().text_pos_at(root.range().start).row as usize, msg: format!("invalid TTML timing parameter {name}") };
    let integer = |name: &str, default| match attr(root, name) {
        Some(v) => positive_integer(v).ok_or_else(|| invalid(name)),
        None => Ok(default),
    };
    let base = integer("frameRate", 30)?;
    let (mn, md) = match attr(root, "frameRateMultiplier") {
        Some(v) => parse_ratio(v).ok_or_else(|| invalid("frameRateMultiplier"))?,
        None => (1, 1),
    };
    let rate = FrameRate::new(base.checked_mul(mn).ok_or_else(|| invalid("frameRateMultiplier"))?, md);
    let sub = integer("subFrameRate", 1)?;
    let tick_rate = match attr(root, "tickRate") {
        Some(v) => positive_integer(v).ok_or_else(|| invalid("tickRate"))?,
        None if attr(root, "frameRate").is_some() => base.checked_mul(sub).ok_or_else(|| invalid("subFrameRate"))?,
        None => 1,
    };
    Ok(Timing { rate, base, sub_frame_rate: sub, tick_rate })
}

/// Latest time (either way) a TTML time expression or nesting of timed elements yields (about 78
/// hours, as interchange imports allow); a damaged file can name any number.
const MAX_TIME: i64 = 1 << 56;

/// Deepest nesting of `<div>` / `<span>` elements that is read.
const MAX_DEPTH: usize = 256;

/// `t` ticks clamped to ±[`MAX_TIME`].
fn clamp_time(t: i128) -> Tick {
    Tick(t.clamp(-(MAX_TIME as i128), MAX_TIME as i128) as i64)
}

/// `a + b`, saturating and clamped to ±[`MAX_TIME`].
fn add_time(a: Tick, b: Tick) -> Tick {
    clamp_time(a.0 as i128 + b.0 as i128)
}

/// Seconds as an exact decimal string → ticks.
fn decimal_ticks(int: &str, frac: &str, unit_ticks: i128) -> Option<i128> {
    let i: i128 = if int.is_empty() { 0 } else { int.parse().ok()? };
    let mut t = i.checked_mul(unit_ticks)?;
    if !frac.is_empty() {
        if frac.len() > 12 || !frac.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let f: i128 = frac.parse().ok()?;
        t = t.checked_add(f.checked_mul(unit_ticks)? / 10i128.pow(frac.len() as u32))?;
    }
    Some(t)
}

/// Parse a TTML time expression into ticks.
fn parse_time(s: &str, tm: Timing) -> Option<Tick> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let tps = TICKS_PER_SECOND as i128;
    if s.contains(':') {
        let parts: Vec<&str> = s.split(':').collect();
        let num = |p: &str| -> Option<i128> { (!p.is_empty() && p.len() <= 6 && p.bytes().all(|b| b.is_ascii_digit())).then(|| p.parse().ok()).flatten() };
        let (h, m) = (num(parts.first()?)?, num(parts.get(1)?)?);
        let sec_part = parts.get(2)?;
        let (si, sf) = sec_part.split_once('.').unwrap_or((sec_part, ""));
        let mut t = (h * 3600 + m * 60) * tps + decimal_ticks(si, sf, tps)?;
        if let Some(fp) = parts.get(3) {
            if parts.len() > 4 {
                return None;
            }
            let (fi, fs) = fp.split_once('.').unwrap_or((fp, ""));
            let frames = num(fi)?;
            let _ = tm.base;
            let sub: i128 = if fs.is_empty() { 0 } else { num(fs)? };
            // frames at the effective rate; sub-frames divide a frame
            let ft = tm.rate.tick_of(1).0 as i128;
            t = t.checked_add(frames.checked_mul(ft)?)?.checked_add(sub.checked_mul(ft)? / tm.sub_frame_rate.max(1) as i128)?;
        }
        return Some(clamp_time(t));
    }
    let unit_start = s.find(|c: char| c.is_ascii_alphabetic())?;
    let (v, unit) = s.split_at(unit_start);
    let (vi, vf) = v.split_once('.').unwrap_or((v, ""));
    if !vi.bytes().all(|b| b.is_ascii_digit()) || vi.len() > 15 {
        return None;
    }
    let t = match unit {
        "h" => decimal_ticks(vi, vf, tps * 3600)?,
        "m" => decimal_ticks(vi, vf, tps * 60)?,
        "s" => decimal_ticks(vi, vf, tps)?,
        "ms" => decimal_ticks(vi, vf, tps / 1000)?,
        "f" => {
            let n: i128 = vi.parse().ok()?;
            let fd = tm.rate.tick_of(1).0 as i128;
            // FrameRate::tick_of in i128 (it truncates to i64); huge values clamp below
            let r = tm.rate.sane();
            let whole = n.saturating_mul(tps).saturating_mul(r.den as i128).div_euclid(r.num as i128);
            whole.checked_add(decimal_ticks("0", vf, fd)?)?
        }
        "t" => {
            let n: i128 = vi.parse().ok()?;
            n.checked_mul(tps)? / tm.tick_rate.max(1) as i128
        }
        _ => return None,
    };
    Some(clamp_time(t))
}

#[derive(Clone, Copy, Default, PartialEq)]
struct Fmt {
    italic: bool,
    bold: bool,
    underline: bool,
}

fn style_of(n: roxmltree::Node, styles: &std::collections::HashMap<String, Fmt>, parent: Fmt) -> Fmt {
    let mut f = parent;
    if let Some(refs) = attr(n, "style") {
        for r in refs.split_whitespace() {
            if let Some(s) = styles.get(r) {
                f.italic |= s.italic;
                f.bold |= s.bold;
                f.underline |= s.underline;
            }
        }
    }
    own_fmt(n, &mut f);
    f
}

fn own_fmt(n: roxmltree::Node, f: &mut Fmt) {
    if let Some(v) = attr(n, "fontStyle") {
        f.italic = v.trim() == "italic" || v.trim() == "oblique";
    }
    if let Some(v) = attr(n, "fontWeight") {
        f.bold = v.trim() == "bold";
    }
    if let Some(v) = attr(n, "textDecoration") {
        let v = v.trim();
        if v.contains("noUnderline") || v == "none" {
            f.underline = false;
        } else if v.contains("underline") {
            f.underline = true;
        }
    }
}

/// Text being collected from a `<p>`: whitespace collapses to one space, which is written only
/// before the next visible character or opening tag (never at a line start).
struct Out {
    s: String,
    pending: bool,
}

impl Out {
    fn flush(&mut self) {
        if self.pending && !self.s.is_empty() && !self.s.ends_with('\n') {
            self.s.push(' ');
        }
        self.pending = false;
    }
}

/// Collect a `<p>`'s (or `<span>`'s) text with `<i>`/`<b>`/`<u>` tags, collapsing whitespace.
/// Elements nested deeper than [`MAX_DEPTH`] are skipped.
fn collect(n: roxmltree::Node, styles: &std::collections::HashMap<String, Fmt>, fmt: Fmt, out: &mut Out, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    for c in n.children() {
        if c.is_text() {
            for ch in c.text().unwrap_or("").chars() {
                if ch.is_whitespace() {
                    out.pending = true;
                } else {
                    out.flush();
                    out.s.push(ch);
                }
            }
        } else if c.is_element() {
            match c.tag_name().name() {
                "br" => {
                    out.pending = false;
                    out.s.push('\n');
                }
                "span" => {
                    let f = style_of(c, styles, fmt);
                    let tags: Vec<&str> = [(f.italic && !fmt.italic, "i"), (f.bold && !fmt.bold, "b"), (f.underline && !fmt.underline, "u")]
                        .into_iter()
                        .filter(|x| x.0)
                        .map(|x| x.1)
                        .collect();
                    if !tags.is_empty() {
                        out.flush();
                    }
                    for t in &tags {
                        out.s.push_str(&format!("<{t}>"));
                    }
                    collect(c, styles, f, out, depth + 1);
                    for t in tags.iter().rev() {
                        out.s.push_str(&format!("</{t}>"));
                    }
                }
                _ => collect(c, styles, fmt, out, depth + 1),
            }
        }
    }
}

fn clean(s: &str) -> String {
    s.lines().map(str::trim).collect::<Vec<_>>().join("\n").trim().to_string()
}

/// Deepest element nesting a TTML document may have. roxmltree parses nested elements
/// recursively (around 1 KiB of stack per level), so a document nested 100 000 deep overflowed
/// the stack.
const MAX_XML_DEPTH: usize = 512;

/// Index of the first `pat` in `b` at or after `from`.
fn find(b: &[u8], from: usize, pat: &[u8]) -> Option<usize> {
    b.get(from..)?.windows(pat.len()).position(|w| w == pat).map(|i| from + i)
}

/// Whether `text` nests elements at most [`MAX_XML_DEPTH`] deep (checked without recursion,
/// before roxmltree parses it). Comments, CDATA and processing instructions are skipped exactly as
/// XML defines them; a DTD (which TTML parsing refuses anyway) or malformed markup is `false`.
fn nesting_ok(text: &str) -> bool {
    let b = text.as_bytes();
    let mut i = 0;
    let mut depth = 0usize;
    while let Some(off) = b.get(i..).and_then(|r| r.iter().position(|&c| c == b'<')) {
        i += off;
        let rest = b.get(i..).unwrap_or_default();
        i = if rest.starts_with(b"<!--") {
            find(b, i + 4, b"-->").map_or(b.len(), |e| e + 3)
        } else if rest.starts_with(b"<![CDATA[") {
            find(b, i + 9, b"]]>").map_or(b.len(), |e| e + 3)
        } else if rest.starts_with(b"<?") {
            find(b, i + 2, b"?>").map_or(b.len(), |e| e + 2)
        } else if rest.starts_with(b"<!") {
            return false;
        } else {
            // the tag, to its `>` outside quoted attribute values (which can't contain `<`)
            let mut j = i + 1;
            let mut quote = None;
            loop {
                let Some(&c) = b.get(j) else { return false };
                match (quote, c) {
                    (_, b'<') => return false,
                    (Some(q), c) if c == q => quote = None,
                    (Some(_), _) => {}
                    (None, b'"' | b'\'') => quote = Some(c),
                    (None, b'>') => break,
                    (None, _) => {}
                }
                j += 1;
            }
            if rest.get(1) == Some(&b'/') {
                depth = depth.saturating_sub(1);
            } else if b.get(j.saturating_sub(1)) != Some(&b'/') {
                depth += 1;
                if depth > MAX_XML_DEPTH {
                    return false;
                }
            }
            j + 1
        };
    }
    true
}

pub fn parse(text: &str) -> Result<Document> {
    if !nesting_ok(text) {
        return Err(Error::Syntax { line: 1, msg: format!("TTML elements are nested deeper than {MAX_XML_DEPTH} levels, or the markup is malformed") });
    }
    let xml = roxmltree::Document::parse(text).map_err(|_| Error::NotFormat("TTML"))?;
    let root = xml.root_element();
    if root.tag_name().name() != "tt" {
        return Err(Error::NotFormat("TTML"));
    }
    let tm = timing(root)?;
    let mut styles = std::collections::HashMap::new();
    for s in root.descendants().filter(|n| n.is_element() && n.tag_name().name() == "style") {
        if let Some(id) = s.attributes().find(|a| a.name() == "id").map(|a| a.value()) {
            let mut f = Fmt::default();
            own_fmt(s, &mut f);
            styles.insert(id.to_string(), f);
        }
    }
    let mut doc = Document::default();
    let Some(body) = root.children().find(|n| n.is_element() && n.tag_name().name() == "body") else {
        return Ok(doc);
    };
    walk(body, Tick::ZERO, &tm, &styles, Fmt::default(), &mut doc, 0);
    doc.cues.sort_by_key(|c| (c.start, c.end));
    Ok(doc)
}

/// Begin and end of a timed element relative to `parent` (None when it has no timing).
fn times(n: roxmltree::Node, parent: Tick, tm: &Timing, doc: &mut Document) -> (Option<Tick>, Option<Tick>) {
    let p = |k: &str, doc: &mut Document| -> Option<Tick> {
        let v = attr(n, k)?;
        let t = parse_time(v, *tm);
        if t.is_none() {
            doc.warnings.push(format!("bad time expression \"{v}\""));
        }
        t
    };
    let begin = p("begin", doc).map(|b| add_time(parent, b));
    let end = match (p("end", doc), p("dur", doc)) {
        (Some(e), _) => Some(add_time(parent, e)),
        (None, Some(d)) => Some(add_time(begin.unwrap_or(parent), d)),
        _ => None,
    };
    (begin, end)
}

fn walk(n: roxmltree::Node, parent: Tick, tm: &Timing, styles: &std::collections::HashMap<String, Fmt>, fmt: Fmt, doc: &mut Document, depth: usize) {
    for c in n.children().filter(|c| c.is_element()) {
        let f = style_of(c, styles, fmt);
        match c.tag_name().name() {
            "div" if depth >= MAX_DEPTH => {
                doc.warnings.push(format!("<div> elements nested deeper than {MAX_DEPTH} were skipped"));
            }
            "div" => {
                let (b, _) = times(c, parent, tm, doc);
                walk(c, b.unwrap_or(parent), tm, styles, f, doc, depth + 1);
            }
            "p" => {
                let (b, e) = times(c, parent, tm, doc);
                let speaker = attr(c, "agent").map(|a| a.trim().trim_start_matches('_').replace('_', " "));
                let id = c.attributes().find(|a| a.name() == "id").map(|a| a.value().to_string());
                match (b, e) {
                    (Some(b), Some(e)) => {
                        let mut o = Out { s: String::new(), pending: false };
                        let tags: Vec<&str> = [(f.italic, "i"), (f.bold, "b"), (f.underline, "u")].into_iter().filter(|x| x.0).map(|x| x.1).collect();
                        for t in &tags {
                            o.s.push_str(&format!("<{t}>"));
                        }
                        collect(c, styles, f, &mut o, 0);
                        for t in tags.iter().rev() {
                            o.s.push_str(&format!("</{t}>"));
                        }
                        let text = clean(&o.s);
                        if e > b && !text.is_empty() {
                            doc.cues.push(Cue { start: b, end: e, text, speaker, id, ..Default::default() });
                        }
                    }
                    _ => {
                        // untimed paragraph: its timed spans are the cues
                        let base = b.unwrap_or(parent);
                        for sp in c.children().filter(|x| x.is_element() && x.tag_name().name() == "span") {
                            let (sb, se) = times(sp, base, tm, doc);
                            if let (Some(sb), Some(se)) = (sb, se) {
                                let mut o = Out { s: String::new(), pending: false };
                                collect(sp, styles, style_of(sp, styles, f), &mut o, 0);
                                let text = clean(&o.s);
                                if se > sb && !text.is_empty() {
                                    doc.cues.push(Cue { start: sb, end: se, text, speaker: speaker.clone(), ..Default::default() });
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tm(rate: FrameRate) -> Timing {
        Timing { rate, base: rate.timecode_base(), sub_frame_rate: 1, tick_rate: 1 }
    }

    #[test]
    fn invalid_timing_parameters_return_an_error() {
        for params in [
            r#"ttp:frameRate="0""#,
            r#"ttp:frameRate="-25""#,
            r#"ttp:frameRate="""#,
            r#"ttp:subFrameRate="0""#,
            r#"ttp:tickRate="-1""#,
            r#"ttp:frameRateMultiplier="1 0""#,
            r#"ttp:frameRateMultiplier="1""#,
            r#"ttp:frameRateMultiplier="1 2 3""#,
        ] {
            let xml = format!(
                r#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:ttp="http://www.w3.org/ns/ttml#parameter" {params}><body><div><p begin="1s" end="2s">Hello</p></div></body></tt>"#
            );
            assert!(crate::parse(xml.as_bytes(), crate::Format::Ttml).is_err(), "{params}");
        }
    }

    #[test]
    fn missing_timing_parameters_keep_defaults() {
        for (params, begin, end) in [
            ("", "1s", "2s"),
            (r#"ttp:frameRate="25""#, "25f", "50f"),
            (r#"ttp:frameRate="30" ttp:subFrameRate="2""#, "60t", "120t"),
            (r#"ttp:frameRate="30" ttp:subFrameRate="2" ttp:tickRate="10""#, "10t", "20t"),
            (r#"ttp:frameRate="25" ttp:frameRateMultiplier="2 1""#, "50f", "100f"),
        ] {
            let xml = format!(
                r#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:ttp="http://www.w3.org/ns/ttml#parameter" {params}><body><div><p begin="{begin}" end="{end}">Hello</p></div></body></tt>"#
            );
            let doc = crate::parse(xml.as_bytes(), crate::Format::Ttml).unwrap();
            assert_eq!(doc.cues.len(), 1, "{params}");
            assert_eq!(doc.cues[0].start, Tick(TICKS_PER_SECOND), "{params}");
            assert_eq!(doc.cues[0].end, Tick(2 * TICKS_PER_SECOND), "{params}");
        }
    }

    #[test]
    fn time_expressions() {
        let t = tm(FrameRate::FPS_25);
        let s = |v: f64| Tick((v * TICKS_PER_SECOND as f64).round() as i64);
        assert_eq!(parse_time("00:00:01.500", t), Some(s(1.5)));
        assert_eq!(parse_time("01:00:00", t), Some(s(3600.0)));
        assert_eq!(parse_time("00:00:02:12", t), Some(s(2.48)));
        assert_eq!(parse_time("2.5s", t), Some(s(2.5)));
        assert_eq!(parse_time("1500ms", t), Some(s(1.5)));
        assert_eq!(parse_time("0.5m", t), Some(s(30.0)));
        assert_eq!(parse_time("50f", t), Some(s(2.0)));
        let ntsc = tm(FrameRate::FPS_29_97);
        assert_eq!(parse_time("30f", ntsc), Some(FrameRate::FPS_29_97.tick_of(30)));
        let ticks = Timing { tick_rate: 10_000_000, ..t };
        assert_eq!(parse_time("15000000t", ticks), Some(s(1.5)));
        assert_eq!(parse_time("bogus", t), None);
        assert_eq!(parse_time("1:2", t), None);
    }

    #[test]
    fn reads_styles_spans_and_nesting() {
        let x = r#"<?xml version="1.0"?>
<tt xmlns="http://www.w3.org/2006/10/ttaf1" xmlns:tts="http://www.w3.org/2006/10/ttaf1#styling" xmlns:ttp="http://www.w3.org/2006/10/ttaf1#parameter" ttp:frameRate="25">
 <head><styling><style xml:id="it" tts:fontStyle="italic"/></styling></head>
 <body>
  <div begin="10s">
   <p begin="00:00:01.000" end="00:00:02.000">Hello
      <span style="it">big</span>   world<br/>
      second   line</p>
   <p begin="3s" dur="1s"><span tts:fontWeight="bold">Bold</span> &amp; plain</p>
  </div>
  <div><p><span begin="20s" end="21s">span timed</span></p></div>
 </body>
</tt>"#;
        let d = parse(x).unwrap();
        let got: Vec<(f64, f64, &str)> = d.cues.iter().map(|c| (c.start.seconds(), c.end.seconds(), c.text.as_str())).collect();
        assert_eq!(got, vec![(11.0, 12.0, "Hello <i>big</i> world\nsecond line"), (13.0, 14.0, "<b>Bold</b> & plain"), (20.0, 21.0, "span timed")]);
        assert!(parse("<html/>").is_err());
        assert!(parse("not xml").is_err());
    }

    #[test]
    fn writes_imsc1_with_frames_and_spans() {
        let r = FrameRate::FPS_29_97;
        let doc = Document {
            cues: vec![Cue {
                start: r.tick_of(30),
                end: r.tick_of(75),
                text: "A <i>tilted</i> line\nand <b>more</b> & \"quotes\"".into(),
                speaker: Some("Ann Lee".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let x = write(&doc, Flavor::Imsc1, Some(r), "en");
        assert!(x.contains(IMSC1_TEXT) && x.contains("ttp:frameRate=\"30\"") && x.contains("ttp:frameRateMultiplier=\"1000 1001\""), "{x}");
        assert!(x.contains("begin=\"30f\" end=\"75f\""), "{x}");
        assert!(x.contains("<span tts:fontStyle=\"italic\">tilted</span>"), "{x}");
        let back = parse(&x).unwrap();
        assert_eq!(back.cues.len(), 1);
        assert_eq!((back.cues[0].start, back.cues[0].end), (r.tick_of(30), r.tick_of(75)));
        assert_eq!(back.cues[0].text, doc.cues[0].text);
        assert_eq!(back.cues[0].speaker.as_deref(), Some("Ann Lee"));
        let d = write(&doc, Flavor::Dfxp, Some(r), "en");
        assert!(d.contains(DFXP_PROFILE) && d.contains("begin=\"00:00:01.001\""), "{d}");
    }

    /// `<div>`s and `<span>`s nested 100 000 deep overflowed the stack (inside roxmltree and in
    /// the recursive walks): such a document is an error, and what lies deeper than MAX_DEPTH in
    /// a document roxmltree can read is skipped.
    #[test]
    fn deep_nesting_is_bounded() {
        let run = || {
            let n = 100_000;
            let divs = format!(r#"<tt xmlns="http://www.w3.org/ns/ttml"><body>{}{}</body></tt>"#, "<div>".repeat(n), "</div>".repeat(n));
            assert!(crate::parse(divs.as_bytes(), crate::Format::Ttml).is_err());
            let spans = format!(
                r#"<tt xmlns="http://www.w3.org/ns/ttml"><body><div><p begin="1s" end="2s">{}{}</p></div></body></tt>"#,
                "<span a='/>'>".repeat(n),
                "</span>".repeat(n)
            );
            assert!(crate::parse(spans.as_bytes(), crate::Format::Ttml).is_err());
        };
        std::thread::Builder::new().stack_size(256 * 1024).spawn(run).unwrap().join().unwrap();
        let run = || {
            let n = 400;
            let divs = format!(
                r#"<tt xmlns="http://www.w3.org/ns/ttml"><body>{}<p begin="1s" end="2s">deep</p>{}<p begin="3s" end="4s">top</p></body></tt>"#,
                "<div>".repeat(n),
                "</div>".repeat(n)
            );
            let doc = crate::parse(divs.as_bytes(), crate::Format::Ttml).unwrap();
            assert_eq!(doc.cues.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(), ["top"]);
            assert!(!doc.warnings.is_empty());
            let spans = format!(
                r#"<tt xmlns="http://www.w3.org/ns/ttml"><body><div><p begin="1s" end="2s">a{}b{}</p></div></body></tt>"#,
                "<span>".repeat(n),
                "</span>".repeat(n)
            );
            let doc = crate::parse(spans.as_bytes(), crate::Format::Ttml).unwrap();
            assert_eq!(doc.cues.len(), 1);
            assert_eq!(doc.cues[0].text, "a");
        };
        std::thread::Builder::new().stack_size(2 * 1024 * 1024).spawn(run).unwrap().join().unwrap();
    }

    /// Huge times wrapped when truncated to i64 and overflowed when added to a parent's begin
    /// (a panic with overflow checks); they clamp now.
    #[test]
    fn huge_times_clamp() {
        let t = tm(FrameRate::FPS_25);
        let max = Some(Tick(MAX_TIME));
        assert_eq!(parse_time("999999999999999h", t), max);
        assert_eq!(parse_time("999999999999999f", t), max);
        assert_eq!(parse_time("999999999999999t", Timing { tick_rate: 1, ..t }), max);
        assert_eq!(parse_time("999999:999999:99999999999999999999999999999999999", t), None);
        assert_eq!(parse_time("999999:999999:9999999999999999999", t), max);
        let xml = format!(
            r#"<tt xmlns="http://www.w3.org/ns/ttml"><body>{}<p begin="999999999999999h" dur="999999999999999h">x</p>{}</body></tt>"#,
            r#"<div begin="999999999999999h">"#.repeat(200),
            "</div>".repeat(200)
        );
        let doc = crate::parse(xml.as_bytes(), crate::Format::Ttml).unwrap();
        assert!(doc.cues.iter().all(|c| c.start.0 <= MAX_TIME && c.end.0 <= MAX_TIME));
    }
}
