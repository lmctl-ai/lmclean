//! Client-side views over a cached scan: filtering, sorting, paging and the
//! summary breakdowns. Everything here works on the cached matches only, so a
//! client can re-slice a scan freely before deciding to clean.
use crate::host::write_json_string;
use crate::json::Json;
use crate::walk;
use std::collections::BTreeMap;
use std::path::PathBuf;

pub struct Match {
    pub path: PathBuf,
    pub root: PathBuf,
    pub size: i64,
    pub kind: &'static str,
    pub modified: i64,
}

impl Match {
    fn lossy_path(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }
    fn lossy_name(&self) -> String {
        self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }
    /// Lowercased extension without the dot; "" when there is none.
    fn ext(&self) -> String {
        self.path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
    }
    fn age_days(&self, now: i64) -> i64 {
        (now - self.modified).max(0) / 86400
    }
}

/// Narrowing criteria; every field that is set must hold (AND).
#[derive(Default)]
pub struct Filter {
    min_size: Option<i64>,
    max_size: Option<i64>,
    exts: Vec<String>,
    kind: Option<String>,
    name: Option<String>,
    path: Option<String>,
    older_than_days: Option<i64>,
    newer_than_days: Option<i64>,
    exclude: Vec<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum SortKey {
    Path,
    Size,
    Modified,
    Name,
}

pub struct Page {
    pub summary_only: bool,
    sort: SortKey,
    desc: Option<bool>,
    limit: usize,
    offset: usize,
}

impl Default for Page {
    fn default() -> Self {
        Page { summary_only: false, sort: SortKey::Path, desc: None, limit: 100, offset: 0 }
    }
}

fn int(key: &str, v: &Json) -> Result<i64, String> {
    match v {
        Json::Num(n) if n.fract() == 0.0 && n.abs() < 9.0e15 => Ok(*n as i64),
        Json::Str(s) => s.trim().parse().map_err(|_| format!("\"{key}\" must be an integer")),
        _ => Err(format!("\"{key}\" must be an integer")),
    }
}

fn text(key: &str, v: &Json) -> Result<String, String> {
    match v {
        Json::Str(s) => Ok(s.clone()),
        _ => Err(format!("\"{key}\" must be a string")),
    }
}

impl Filter {
    /// Parses one filter key; returns false when `key` is not a filter key.
    fn set(&mut self, key: &str, v: &Json) -> Result<bool, String> {
        match key {
            "min_size" => self.min_size = Some(int(key, v)?),
            "max_size" => self.max_size = Some(int(key, v)?),
            "older_than_days" => self.older_than_days = Some(int(key, v)?),
            "newer_than_days" => self.newer_than_days = Some(int(key, v)?),
            "name" => self.name = Some(text(key, v)?),
            "path" => self.path = Some(text(key, v)?),
            "kind" => {
                let k = text(key, v)?;
                if !["file", "dir", "symlink"].contains(&k.as_str()) {
                    return Err("\"kind\" must be file, dir or symlink".into());
                }
                self.kind = Some(k);
            }
            "ext" => match v {
                Json::Arr(items) => {
                    for item in items {
                        self.exts.push(text(key, item)?.trim_start_matches('.').to_lowercase());
                    }
                }
                _ => {
                    for part in text(key, v)?.split(',') {
                        self.exts.push(part.trim().trim_start_matches('.').to_lowercase());
                    }
                }
            },
            "exclude" => match v {
                Json::Arr(items) => {
                    for item in items {
                        self.exclude.push(text(key, item)?);
                    }
                }
                _ => self.exclude.push(text(key, v)?),
            },
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// A body object holding only filter keys (strict: unknown keys fail).
    pub fn from_json(obj: &Json) -> Result<Filter, String> {
        let Json::Obj(items) = obj else {
            return Err("\"filter\" must be an object".into());
        };
        let mut f = Filter::default();
        for (k, v) in items {
            if !f.set(k, v)? {
                return Err(format!("unknown filter key \"{k}\""));
            }
        }
        Ok(f)
    }

    fn passes(&self, m: &Match, now: i64) -> bool {
        self.min_size.is_none_or(|n| m.size >= n)
            && self.max_size.is_none_or(|n| m.size <= n)
            && self.kind.as_deref().is_none_or(|k| m.kind == k)
            && (self.exts.is_empty() || self.exts.contains(&m.ext()))
            && self.name.as_deref().is_none_or(|n| m.lossy_name().contains(n))
            && self.path.as_deref().is_none_or(|p| m.lossy_path().contains(p))
            && self.older_than_days.is_none_or(|d| m.age_days(now) >= d)
            && self.newer_than_days.is_none_or(|d| m.age_days(now) < d)
            && !self.exclude.iter().any(|e| *e == m.lossy_path())
    }
}

/// Filter + paging from query-string / flat fields (`GET /results?...`).
pub fn from_fields(fields: &[(String, Json)]) -> Result<(Filter, Page), String> {
    let (mut f, mut p) = (Filter::default(), Page::default());
    for (k, v) in fields {
        if f.set(k, v)? {
            continue;
        }
        match k.as_str() {
            "view" => match text(k, v)?.as_str() {
                "summary" => p.summary_only = true,
                "detail" => p.summary_only = false,
                _ => return Err("\"view\" must be summary or detail".into()),
            },
            "sort" => {
                p.sort = match text(k, v)?.as_str() {
                    "path" => SortKey::Path,
                    "size" => SortKey::Size,
                    "modified" => SortKey::Modified,
                    "name" => SortKey::Name,
                    _ => return Err("\"sort\" must be path, size, modified or name".into()),
                }
            }
            "order" => {
                p.desc = Some(match text(k, v)?.as_str() {
                    "asc" => false,
                    "desc" => true,
                    _ => return Err("\"order\" must be asc or desc".into()),
                })
            }
            "limit" => p.limit = int(k, v)?.clamp(0, 5000) as usize,
            "offset" => p.offset = int(k, v)?.max(0) as usize,
            other => return Err(format!("unknown parameter \"{other}\"")),
        }
    }
    Ok((f, p))
}

/// `a=1&b=x%20y` -> fields (values as strings; repeated keys kept).
pub fn parse_query(query: &str) -> Vec<(String, Json)> {
    fn decode(s: &str) -> String {
        let b = s.as_bytes();
        let (mut out, mut i) = (Vec::new(), 0);
        while i < b.len() {
            match b[i] {
                b'+' => out.push(b' '),
                b'%' if b.get(i + 1..i + 3).is_some() => {
                    match std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                        Some(v) => {
                            out.push(v);
                            i += 2;
                        }
                        None => out.push(b'%'),
                    }
                }
                c => out.push(c),
            }
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(k), Json::Str(decode(v)))
        })
        .collect()
}

pub fn select<'a>(matches: &'a [Match], f: &Filter, now: i64) -> Vec<&'a Match> {
    matches.iter().filter(|m| f.passes(m, now)).collect()
}

fn jstr(out: &mut Vec<u8>, bytes: &[u8]) {
    let _ = write_json_string(out, bytes);
}

/// `"matched":N,"bytes":B,"by_kind":{...},"by_ext":[...],"by_root":[...]`
pub fn summary_fields(sel: &[&Match], out: &mut Vec<u8>) {
    let bytes: i64 = sel.iter().map(|m| m.size).sum();
    out.extend_from_slice(format!("\"matched\":{},\"bytes\":{bytes},\"by_kind\":{{", sel.len()).as_bytes());
    let mut kinds: BTreeMap<&str, (u64, i64)> = BTreeMap::new();
    let mut exts: BTreeMap<String, (u64, i64)> = BTreeMap::new();
    let mut roots: BTreeMap<Vec<u8>, (u64, i64)> = BTreeMap::new();
    for m in sel {
        let k = kinds.entry(m.kind).or_default();
        (k.0, k.1) = (k.0 + 1, k.1 + m.size);
        if m.kind == "file" {
            let e = exts.entry(m.ext()).or_default();
            (e.0, e.1) = (e.0 + 1, e.1 + m.size);
        }
        let r = roots.entry(walk::path_bytes(&m.root)).or_default();
        (r.0, r.1) = (r.0 + 1, r.1 + m.size);
    }
    for (i, (kind, (n, b))) in kinds.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.extend_from_slice(format!("\"{kind}\":{{\"matched\":{n},\"bytes\":{b}}}").as_bytes());
    }
    out.extend_from_slice(b"},\"by_ext\":[");
    let mut ranked: Vec<_> = exts.into_iter().collect();
    ranked.sort_by(|a, b| b.1 .1.cmp(&a.1 .1).then(a.0.cmp(&b.0)));
    for (i, (ext, (n, b))) in ranked.iter().take(20).enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.extend_from_slice(b"{\"ext\":");
        jstr(out, ext.as_bytes());
        out.extend_from_slice(format!(",\"matched\":{n},\"bytes\":{b}}}").as_bytes());
    }
    out.extend_from_slice(b"],\"by_root\":[");
    for (i, (root, (n, b))) in roots.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.extend_from_slice(b"{\"root\":");
        jstr(out, root);
        out.extend_from_slice(format!(",\"matched\":{n},\"bytes\":{b}}}").as_bytes());
    }
    out.push(b']');
}

/// `"offset":N,"limit":N,"returned":N,"matches":[...]` in the requested order.
pub fn detail_fields(sel: &[&Match], page: &Page, now: i64, out: &mut Vec<u8>) {
    let mut order: Vec<&Match> = sel.to_vec();
    match page.sort {
        SortKey::Path => order.sort_by_key(|m| walk::path_bytes(&m.path)),
        SortKey::Name => order.sort_by(|a, b| a.lossy_name().cmp(&b.lossy_name()).then(a.path.cmp(&b.path))),
        SortKey::Size => order.sort_by(|a, b| a.size.cmp(&b.size).then(a.path.cmp(&b.path))),
        SortKey::Modified => order.sort_by(|a, b| a.modified.cmp(&b.modified).then(a.path.cmp(&b.path))),
    }
    // Size and modified default to "biggest / newest first"; path and name ascend.
    let desc = page.desc.unwrap_or(matches!(page.sort, SortKey::Size | SortKey::Modified));
    if desc {
        order.reverse();
    }
    let window: Vec<&&Match> = order.iter().skip(page.offset).take(page.limit).collect();
    out.extend_from_slice(
        format!("\"offset\":{},\"limit\":{},\"returned\":{},\"matches\":[", page.offset, page.limit, window.len())
            .as_bytes(),
    );
    for (i, m) in window.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.extend_from_slice(b"{\"path\":");
        jstr(out, &walk::path_bytes(&m.path));
        out.extend_from_slice(
            format!(
                ",\"size\":{},\"kind\":\"{}\",\"modified\":{},\"age_days\":{}}}",
                m.size,
                m.kind,
                m.modified,
                m.age_days(now)
            )
            .as_bytes(),
        );
    }
    out.push(b']');
}
