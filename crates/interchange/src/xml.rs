//! Minimal XML writer and read helpers over `roxmltree`.

use std::fmt::Write as _;

use roxmltree::Node;

pub(crate) fn escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            c if (c as u32) < 0x20 && !matches!(c, '\n' | '\r' | '\t') => {}
            c => o.push(c),
        }
    }
    o
}

/// Pretty-printing XML writer (two-space indent, one element per line).
pub(crate) struct XmlWriter {
    out: String,
    stack: Vec<String>,
}

impl XmlWriter {
    pub fn new(doctype: Option<&str>) -> Self {
        let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        if let Some(d) = doctype {
            let _ = writeln!(out, "<!DOCTYPE {d}>");
        }
        Self { out, stack: Vec::new() }
    }

    fn indent(&mut self) {
        for _ in 0..self.stack.len() {
            self.out.push_str("  ");
        }
    }

    fn start_tag(&mut self, name: &str, attrs: &[(&str, &str)]) {
        self.indent();
        self.out.push('<');
        self.out.push_str(name);
        for (k, v) in attrs {
            let _ = write!(self.out, " {k}=\"{}\"", escape(v));
        }
    }

    pub fn open(&mut self, name: &str, attrs: &[(&str, &str)]) {
        self.start_tag(name, attrs);
        self.out.push_str(">\n");
        self.stack.push(name.to_string());
    }

    pub fn close(&mut self) {
        let Some(name) = self.stack.pop() else { return };
        self.indent();
        let _ = writeln!(self.out, "</{name}>");
    }

    pub fn empty(&mut self, name: &str, attrs: &[(&str, &str)]) {
        self.start_tag(name, attrs);
        self.out.push_str("/>\n");
    }

    /// `<name attrs>value</name>`.
    pub fn text_attrs(&mut self, name: &str, attrs: &[(&str, &str)], value: &str) {
        self.start_tag(name, attrs);
        let _ = writeln!(self.out, ">{}</{name}>", escape(value));
    }

    pub fn text(&mut self, name: &str, value: impl std::fmt::Display) {
        self.text_attrs(name, &[], &value.to_string());
    }

    pub fn finish(mut self) -> String {
        while !self.stack.is_empty() {
            self.close();
        }
        self.out
    }
}

pub(crate) fn child<'a, 'i>(n: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    n.children().find(|c| c.is_element() && c.tag_name().name() == name)
}

pub(crate) fn children<'a, 'i>(n: Node<'a, 'i>, name: &'static str) -> impl Iterator<Item = Node<'a, 'i>> {
    n.children().filter(move |c| c.is_element() && c.tag_name().name() == name)
}

pub(crate) fn elements<'a, 'i>(n: Node<'a, 'i>) -> impl Iterator<Item = Node<'a, 'i>> {
    n.children().filter(|c| c.is_element())
}

/// Trimmed text of a node's direct text content.
pub(crate) fn text<'a>(n: Node<'a, '_>) -> &'a str {
    n.text().map(str::trim).unwrap_or("")
}

/// Text of the element at `path` below `n`.
pub(crate) fn path_text<'a>(n: Node<'a, '_>, path: &[&str]) -> Option<&'a str> {
    let mut cur = n;
    for p in path {
        cur = child(cur, p)?;
    }
    Some(text(cur))
}

pub(crate) fn child_text<'a>(n: Node<'a, '_>, name: &str) -> Option<&'a str> {
    child(n, name).map(text)
}

pub(crate) fn child_i64(n: Node<'_, '_>, name: &str) -> Option<i64> {
    child_text(n, name).and_then(|t| t.parse::<i64>().ok().or_else(|| t.parse::<f64>().ok().map(|f| f.round() as i64)))
}

pub(crate) fn child_f64(n: Node<'_, '_>, name: &str) -> Option<f64> {
    child_text(n, name).and_then(|t| t.parse::<f64>().ok())
}

pub(crate) fn child_bool(n: Node<'_, '_>, name: &str) -> Option<bool> {
    child_text(n, name).map(|t| t.eq_ignore_ascii_case("true") || t == "1")
}

pub(crate) fn bool_str(b: bool) -> &'static str {
    if b { "TRUE" } else { "FALSE" }
}

/// Format an f64 compactly (no trailing zeros, no exponent for normal ranges).
pub(crate) fn num(v: f64) -> String {
    if v == v.round() && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    if !v.is_finite() {
        return "0".into();
    }
    // Shortest representation that parses back to the same f64.
    format!("{v}")
}

/// Deepest element nesting a document may have. roxmltree parses nested elements recursively
/// (around 1 KiB of stack per level), so a document nested 100 000 deep overflowed the stack.
pub(crate) const MAX_XML_DEPTH: usize = 512;

/// Most nodes (elements, text, attributes' owners…) a parsed document may have: a cap on memory.
const MAX_XML_NODES: u32 = 4_000_000;

pub(crate) fn parse(text: &str) -> Result<roxmltree::Document<'_>, String> {
    check_nesting(text)?;
    // DTDs are allowed (FCP7 files start with `<!DOCTYPE xmeml>`); roxmltree bounds entity
    // expansion (billion laughs) and the node limit bounds the rest.
    let opts = roxmltree::ParsingOptions { allow_dtd: true, nodes_limit: MAX_XML_NODES, ..Default::default() };
    roxmltree::Document::parse_with_options(text, opts).map_err(|e| e.to_string())
}

/// Index of the first `pat` in `b` at or after `from`.
fn find(b: &[u8], from: usize, pat: &[u8]) -> Option<usize> {
    b.get(from..)?.windows(pat.len()).position(|w| w == pat).map(|i| from + i)
}

/// Check, without recursion, that `text` nests elements at most [`MAX_XML_DEPTH`] deep, before
/// roxmltree (which recurses per level) parses it. Regions skipped without counting (comments,
/// CDATA, processing instructions, the DTD) follow the XML grammar exactly, and a DTD literal may
/// not contain markup (so entities expand to text only); anything else malformed is an error
/// here, as it would be for roxmltree.
pub(crate) fn check_nesting(text: &str) -> Result<(), String> {
    let b = text.as_bytes();
    let malformed = || "malformed XML markup".to_string();
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
            skip_declaration(b, i + 2)?
        } else {
            // the tag, to its `>` outside quoted attribute values (which can't contain `<`)
            let mut j = i + 1;
            let mut quote = None;
            loop {
                let Some(&c) = b.get(j) else { return Err(malformed()) };
                match (quote, c) {
                    (_, b'<') => return Err(malformed()),
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
                    return Err(format!("XML elements are nested deeper than {MAX_XML_DEPTH} levels"));
                }
            }
            j + 1
        };
    }
    Ok(())
}

/// Whether `b` starts with a character reference to `<` (`&#60;`, `&#x3C;`, with any zeros).
fn is_lt_reference(b: &[u8]) -> bool {
    let Some(r) = b.strip_prefix(b"&#") else { return false };
    let (digits, radix) = match r.first() {
        Some(b'x' | b'X') => (r.get(1..).unwrap_or_default(), 16),
        _ => (r, 10),
    };
    let n = digits.iter().take_while(|c| c.is_ascii_hexdigit()).count();
    let num = digits.get(..n).and_then(|d| std::str::from_utf8(d).ok()).map(|d| d.trim_start_matches('0'));
    // more digits than any code point needs is not a valid reference anyway
    num.is_some_and(|d| d.len() <= 8 && u32::from_str_radix(if d.is_empty() { "0" } else { d }, radix) == Ok(60))
}

/// Skip a `<!…>` declaration (a DOCTYPE, with its internal subset) starting after `<!`; the
/// index after it.
fn skip_declaration(b: &[u8], start: usize) -> Result<usize, String> {
    let malformed = || "malformed XML document type declaration".to_string();
    let markup = || "XML entities that contain markup are not supported".to_string();
    let mut j = start;
    let mut quote = None;
    let mut subset = false;
    while let Some(&c) = b.get(j) {
        let rest = b.get(j..).unwrap_or_default();
        match quote {
            // literals (entity values, system ids) never hold markup here; a character
            // reference could spell `<` once expanded
            Some(_) if c == b'<' || is_lt_reference(rest) => return Err(markup()),
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == b'"' || c == b'\'' => quote = Some(c),
            None if subset && rest.starts_with(b"<!--") => {
                j = find(b, j + 4, b"-->").ok_or_else(malformed)? + 3;
                continue;
            }
            None if subset && rest.starts_with(b"<?") => {
                j = find(b, j + 2, b"?>").ok_or_else(malformed)? + 2;
                continue;
            }
            // markup declarations of the subset (`<!ENTITY …>`, `<!ELEMENT …>`…)
            None if subset && rest.starts_with(b"<!") => {}
            None if c == b'<' => return Err(malformed()),
            None if c == b'[' && !subset => subset = true,
            None if c == b']' && subset => subset = false,
            None if c == b'>' && !subset => return Ok(j + 1),
            None => {}
        }
        j += 1;
    }
    Err(malformed())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_roundtrip() {
        let mut w = XmlWriter::new(Some("xmeml"));
        w.open("a", &[("x", "1 & 2")]);
        w.text("b", "<hi>");
        w.empty("c", &[]);
        let s = w.finish();
        let d = parse(&s).unwrap();
        let a = d.root_element();
        assert_eq!(a.attribute("x"), Some("1 & 2"));
        assert_eq!(child_text(a, "b"), Some("<hi>"));
        assert!(child(a, "c").is_some());
        assert_eq!(num(1.5), "1.5");
        assert_eq!(num(100.0), "100");
        assert_eq!(num(-0.0), "0");
        assert_eq!(num(0.1 + 0.2).parse::<f64>().unwrap(), 0.1 + 0.2);
    }

    /// Elements nested 100 000 deep overflowed the stack inside roxmltree; they are an error now.
    #[test]
    fn deep_nesting_is_refused_before_parsing() {
        let run = || {
            let deep = format!("<!DOCTYPE xmeml>\n<a>{}{}</a>", "<b x='/>'>".repeat(100_000), "</b>".repeat(100_000));
            assert!(parse(&deep).unwrap_err().contains("nested deeper"));
            // a quoted `/>` or a self-closing tag doesn't fool the count
            let ok = format!("<a>{}{}{}</a>", "<b x='/>'>".repeat(MAX_XML_DEPTH - 1), "<c/>".repeat(100_000), "</b>".repeat(MAX_XML_DEPTH - 1));
            assert!(parse(&ok).is_ok());
            let one_more = format!("<a>{}{}</a>", "<b>".repeat(MAX_XML_DEPTH), "</b>".repeat(MAX_XML_DEPTH));
            assert!(parse(&one_more).is_err());
            // markup hidden from the count is refused
            for bad in [
                "<!DOCTYPE a [ <!ENTITY e \"<b><b>\"> ]><a>&e;</a>",
                "<!DOCTYPE a [ <!ENTITY e \"&#60;b>\"> ]><a>&e;</a>",
                "<!DOCTYPE a [ <!ENTITY e \"&#x003c;b>\"> ]><a>&e;</a>",
                "<!DOCTYPE a \"<a>\"><a/>",
                "<a x=\"<b>\"/>",
                "<a",
            ] {
                assert!(parse(bad).is_err(), "{bad}");
            }
            assert!(parse("<!DOCTYPE a [ <?pi \"?> <!-- ' --> <!ELEMENT a ANY> ]><a/>").is_ok());
            assert!(parse("<!DOCTYPE a [ <!ENTITY c \"&#169; &#x3E;\"> ]><a>&c;</a>").is_ok());
        };
        std::thread::Builder::new().stack_size(1024 * 1024).spawn(run).unwrap().join().unwrap();
    }

    /// Billion laughs: entity expansion is bounded (roxmltree refuses it), never exponential.
    #[test]
    fn billion_laughs_is_refused() {
        let mut dtd = String::from("<!ENTITY l0 \"lol\">");
        for i in 1..10 {
            dtd.push_str(&format!("<!ENTITY l{i} \"{}\">", format!("&l{};", i - 1).repeat(10)));
        }
        let doc = format!("<?xml version=\"1.0\"?><!DOCTYPE lolz [{dtd}]><lolz>&l9;</lolz>");
        let started = std::time::Instant::now();
        assert!(parse(&doc).is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        // a small entity still expands
        let d = parse("<!DOCTYPE a [<!ENTITY e \"x &amp; y\">]><a>&e;</a>").unwrap();
        assert_eq!(d.root_element().text(), Some("x & y"));
    }
}
