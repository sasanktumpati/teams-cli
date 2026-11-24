use serde::ser::{SerializeMap, SerializeSeq};
use serde::Serialize;
use serde_json::ser::{Formatter, Serializer};
use serde_json::Value;
use std::io;

#[derive(Clone, Copy)]
enum Style {
    Compact,
    Spaced,
    Pretty,
}

struct CanonicalFormatter {
    style: Style,
    depth: usize,
    has_value: bool,
}

impl CanonicalFormatter {
    fn newline<W: ?Sized + io::Write>(&self, w: &mut W) -> io::Result<()> {
        w.write_all(b"\n")?;
        for _ in 0..self.depth {
            w.write_all(b"  ")?;
        }
        Ok(())
    }

    fn item_sep<W: ?Sized + io::Write>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
        match self.style {
            Style::Compact if !first => w.write_all(b","),
            Style::Spaced if !first => w.write_all(b", "),
            Style::Pretty => {
                if !first {
                    w.write_all(b",")?;
                }
                self.newline(w)
            }
            _ => Ok(()),
        }
    }

    fn open<W: ?Sized + io::Write>(&mut self, w: &mut W, c: &[u8]) -> io::Result<()> {
        self.depth += 1;
        self.has_value = false;
        w.write_all(c)
    }

    fn close<W: ?Sized + io::Write>(&mut self, w: &mut W, c: &[u8]) -> io::Result<()> {
        self.depth -= 1;
        if matches!(self.style, Style::Pretty) && self.has_value {
            self.newline(w)?;
        }
        self.has_value = true;
        w.write_all(c)
    }
}

impl Formatter for CanonicalFormatter {
    fn begin_array<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.open(w, b"[")
    }
    fn end_array<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.close(w, b"]")
    }
    fn begin_array_value<W: ?Sized + io::Write>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
        self.item_sep(w, first)
    }
    fn end_array_value<W: ?Sized + io::Write>(&mut self, _w: &mut W) -> io::Result<()> {
        self.has_value = true;
        Ok(())
    }
    fn begin_object<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.open(w, b"{")
    }
    fn end_object<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.close(w, b"}")
    }
    fn begin_object_key<W: ?Sized + io::Write>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
        self.item_sep(w, first)
    }
    fn begin_object_value<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        w.write_all(if matches!(self.style, Style::Compact) {
            b":"
        } else {
            b": "
        })
    }
    fn end_object_value<W: ?Sized + io::Write>(&mut self, _w: &mut W) -> io::Result<()> {
        self.has_value = true;
        Ok(())
    }
    fn write_f64<W: ?Sized + io::Write>(&mut self, w: &mut W, v: f64) -> io::Result<()> {
        w.write_all(format_float(v).as_bytes())
    }
    fn write_f32<W: ?Sized + io::Write>(&mut self, w: &mut W, v: f32) -> io::Result<()> {
        w.write_all(format_float(v as f64).as_bytes())
    }
}

fn serialize_into<W: io::Write>(w: &mut W, v: &impl Serialize, style: Style) -> io::Result<()> {
    let fmt = CanonicalFormatter {
        style,
        depth: 0,
        has_value: false,
    };
    let mut ser = Serializer::with_formatter(w, fmt);
    v.serialize(&mut ser).map_err(io::Error::other)
}

fn serialize_string(v: &impl Serialize, style: Style) -> String {
    let mut buf = Vec::with_capacity(128);
    if serialize_into(&mut buf, v, style).is_err() {
        return "null".to_string();
    }
    String::from_utf8(buf).unwrap_or_default()
}

pub fn to_string(v: &impl Serialize) -> String {
    serialize_string(v, Style::Spaced)
}

pub fn to_compact(v: &impl Serialize) -> String {
    serialize_string(v, Style::Compact)
}

pub fn to_pretty(v: &impl Serialize) -> String {
    serialize_string(v, Style::Pretty)
}

pub fn to_sorted(v: &Value) -> String {
    to_string(&SortedRef(v))
}

pub fn write_sorted_filtered<W: io::Write>(w: &mut W, v: &Value, volatile: &[&str]) -> io::Result<()> {
    if volatile.is_empty() {
        serialize_into(w, &SortedRef(v), Style::Spaced)
    } else {
        serialize_into(w, &FilteredRootRef { value: v, volatile }, Style::Spaced)
    }
}

struct SortedRef<'a>(&'a Value);

impl Serialize for SortedRef<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            Value::Object(map) => {
                let mut entries: Vec<(&str, &Value)> = map.iter().map(|(k, v)| (k.as_str(), v)).collect();
                entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
                let mut m = s.serialize_map(Some(entries.len()))?;
                for (k, v) in entries {
                    m.serialize_entry(k, &SortedRef(v))?;
                }
                m.end()
            }
            Value::Array(a) => {
                let mut seq = s.serialize_seq(Some(a.len()))?;
                for item in a {
                    seq.serialize_element(&SortedRef(item))?;
                }
                seq.end()
            }
            other => other.serialize(s),
        }
    }
}

struct FilteredRootRef<'a> {
    value: &'a Value,
    volatile: &'a [&'a str],
}

fn has_nested_volatile(volatile: &[&str], parent: &str) -> bool {
    volatile
        .iter()
        .any(|f| f.strip_prefix(parent).is_some_and(|rest| rest.starts_with('.')))
}

fn is_nested_volatile(volatile: &[&str], parent: &str, child: &str) -> bool {
    volatile.iter().any(|f| {
        f.strip_prefix(parent)
            .and_then(|rest| rest.strip_prefix('.'))
            .is_some_and(|c| c == child)
    })
}

struct FilteredSubObjRef<'a> {
    parent: &'a str,
    map: &'a serde_json::Map<String, Value>,
    volatile: &'a [&'a str],
}

impl Serialize for FilteredSubObjRef<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut entries: Vec<(&str, &Value)> = self
            .map
            .iter()
            .filter(|(kk, _)| !is_nested_volatile(self.volatile, self.parent, kk.as_str()))
            .map(|(kk, vv)| (kk.as_str(), vv))
            .collect();
        entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
        let mut m = s.serialize_map(Some(entries.len()))?;
        for (kk, vv) in entries {
            m.serialize_entry(kk, &SortedRef(vv))?;
        }
        m.end()
    }
}

impl Serialize for FilteredRootRef<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let Value::Object(map) = self.value else {
            return SortedRef(self.value).serialize(s);
        };
        let mut entries: Vec<(&str, &Value)> = map
            .iter()
            .filter(|(k, _)| !self.volatile.contains(&k.as_str()))
            .map(|(k, v)| (k.as_str(), v))
            .collect();
        entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
        let mut m = s.serialize_map(Some(entries.len()))?;
        for (k, v) in entries {
            if let Value::Object(sub) = v {
                if has_nested_volatile(self.volatile, k) {
                    m.serialize_entry(
                        k,
                        &FilteredSubObjRef {
                            parent: k,
                            map: sub,
                            volatile: self.volatile,
                        },
                    )?;
                    continue;
                }
            }
            m.serialize_entry(k, &SortedRef(v))?;
        }
        m.end()
    }
}

pub fn format_float(v: f64) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    let sci = format!("{v:e}");
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let (neg, mantissa) = match mantissa.strip_prefix('-') {
        Some(m) => (true, m),
        None => (false, mantissa),
    };
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let mut out = String::with_capacity(digits.len() + 8);
    if neg {
        out.push('-');
    }
    if (-4..16).contains(&exp) {
        if exp >= 0 {
            let int_len = exp as usize + 1;
            if digits.len() <= int_len {
                out.push_str(&digits);
                out.extend(std::iter::repeat_n('0', int_len - digits.len()));
                out.push_str(".0");
            } else {
                out.push_str(&digits[..int_len]);
                out.push('.');
                out.push_str(&digits[int_len..]);
            }
        } else {
            out.push_str("0.");
            out.extend(std::iter::repeat_n('0', (-exp - 1) as usize));
            out.push_str(&digits);
        }
    } else {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push_str(&format!("e{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_float_formatting() {
        for (v, want) in [
            (1795472573801.0, "1795472573801.0"),
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (1.5, "1.5"),
            (0.1, "0.1"),
            (1e15, "1000000000000000.0"),
            (1e16, "1e+16"),
            (1.5e16, "1.5e+16"),
            (123456789012345680.0, "1.2345678901234568e+17"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (-2.5e-7, "-2.5e-07"),
            (1e100, "1e+100"),
            (5e-324, "5e-324"),
            (1.7976931348623157e308, "1.7976931348623157e+308"),
        ] {
            assert_eq!(format_float(v), want, "{v}");
        }
    }

    #[test]
    fn canonical_styles_and_filtered_streaming() {
        let v = json!({"b": [1, 2.0, {"x": null}], "a": "é\n\"", "e": [], "o": {}});
        assert_eq!(
            to_string(&v),
            r#"{"b": [1, 2.0, {"x": null}], "a": "é\n\"", "e": [], "o": {}}"#
        );
        assert_eq!(to_compact(&v), r#"{"b":[1,2.0,{"x":null}],"a":"é\n\"","e":[],"o":{}}"#);
        assert_eq!(
            to_sorted(&v),
            r#"{"a": "é\n\"", "b": [1, 2.0, {"x": null}], "e": [], "o": {}}"#
        );
        assert_eq!(
            to_pretty(&v),
            "{\n  \"b\": [\n    1,\n    2.0,\n    {\n      \"x\": null\n    }\n  ],\n  \"a\": \"é\\n\\\"\",\n  \"e\": [],\n  \"o\": {}\n}"
        );
        assert_eq!(to_string(&json!([])), "[]");
        assert_eq!(to_pretty(&json!("s")), "\"s\"");

        let with_volatile = json!({
            "z": 1,
            "drop_me": 99,
            "properties": {"keep": "yes", "isread": true},
            "a": "ok"
        });
        let mut streamed = Vec::new();
        write_sorted_filtered(&mut streamed, &with_volatile, &["drop_me", "properties.isread"]).unwrap();
        assert_eq!(
            String::from_utf8(streamed).unwrap(),
            r#"{"a": "ok", "properties": {"keep": "yes"}, "z": 1}"#
        );
    }
}
