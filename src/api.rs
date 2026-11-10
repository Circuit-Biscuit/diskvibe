//! Tabular results ("sections"), report rendering, and the tool surface shared by CLI and MCP.
use crate::ops::{self, DupGroup};
use crate::query::{self, Expr, SortKey};
use crate::scan::{self, category, now, Progress, Tree};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

pub struct Section {
    pub title: String,
    pub cols: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

fn sec(title: impl Into<String>, cols: &[&str], rows: Vec<Vec<String>>) -> Section {
    Section { title: title.into(), cols: cols.iter().map(|c| c.to_string()).collect(), rows }
}

pub fn human(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < 4 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{b} B") } else { format!("{v:.*} {}", if v < 10.0 { 2 } else { 1 }, U[i]) }
}

pub fn commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn date(ts: i64) -> String {
    match chrono::DateTime::from_timestamp(ts, 0) {
        Some(d) if ts > 0 => d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string(),
        _ => String::new(),
    }
}

// ───────────── stats (charts + reports) ─────────────

pub struct Bucket {
    pub label: String,
    pub bytes: u64,
    pub count: u64,
    pub query: String, // filter that selects exactly this bucket — used for chart drill-down
}

pub struct Stats {
    pub cats: Vec<Bucket>,
    pub exts: Vec<Bucket>,
    pub ages: Vec<Bucket>,
    pub sizes: Vec<Bucket>,
}

pub fn stats(t: &Tree, root: u32, f: Option<&Expr>) -> Stats {
    const AGES: [(&str, i64); 6] =
        [("< 1 week", 7), ("1 week – 1 month", 30), ("1 – 6 months", 182), ("6 – 12 months", 365), ("1 – 2 years", 730), ("> 2 years", i64::MAX)];
    const SIZES: [(&str, u64, &str); 6] = [
        ("< 1 MB", 1 << 20, "1mb"),
        ("1 – 10 MB", 10 << 20, "10mb"),
        ("10 – 100 MB", 100 << 20, "100mb"),
        ("100 MB – 1 GB", 1 << 30, "1gb"),
        ("1 – 10 GB", 10 << 30, "10gb"),
        ("> 10 GB", u64::MAX, ""),
    ];
    let mut exts: HashMap<String, (u64, u64)> = HashMap::new();
    let mut cats: HashMap<&str, (u64, u64)> = HashMap::new();
    let (mut ages, mut sizes) = ([(0u64, 0u64); 6], [(0u64, 0u64); 6]);
    let now = now();
    for id in t.descendants(root) {
        let n = &t.nodes[id as usize];
        if n.is_dir || !f.map_or(true, |f| f.eval(t, id)) {
            continue;
        }
        let ext = t.ext(id);
        let add = |e: &mut (u64, u64)| (e.0 += n.size, e.1 += 1);
        add(cats.entry(category(&ext)).or_default());
        add(exts.entry(ext).or_default());
        let days = (now - n.modified) / 86400;
        add(&mut ages[AGES.iter().position(|a| days < a.1).unwrap_or(5)]);
        add(&mut sizes[SIZES.iter().position(|s| n.size < s.1).unwrap_or(5)]);
    }
    let sorted = |mut v: Vec<Bucket>| {
        v.sort_by_key(|b| std::cmp::Reverse(b.bytes));
        v
    };
    let range = |field: &str, lo: String, hi: String| match (lo.is_empty(), hi.is_empty()) {
        (true, _) => format!("file and {field} < {hi}"),
        (_, true) => format!("file and {field} >= {lo}"),
        _ => format!("file and {field} >= {lo} and {field} < {hi}"),
    };
    Stats {
        cats: sorted(cats.into_iter().map(|(k, v)| Bucket { label: k.into(), bytes: v.0, count: v.1, query: format!("category = \"{}\"", k.to_lowercase()) }).collect()),
        exts: sorted(exts.into_iter().map(|(k, v)| Bucket { query: format!("file and ext = \"{k}\""), label: if k.is_empty() { "(none)".into() } else { k }, bytes: v.0, count: v.1 }).collect()),
        ages: (0..6).map(|i| Bucket {
            label: AGES[i].0.into(), bytes: ages[i].0, count: ages[i].1,
            query: range("age", if i == 0 { String::new() } else { AGES[i - 1].1.to_string() }, if i == 5 { String::new() } else { AGES[i].1.to_string() }),
        }).collect(),
        sizes: (0..6).map(|i| Bucket {
            label: SIZES[i].0.into(), bytes: sizes[i].0, count: sizes[i].1,
            query: range("size", if i == 0 { String::new() } else { SIZES[i - 1].2.into() }, SIZES[i].2.into()),
        }).collect(),
    }
}

// ───────────── sections ─────────────

pub fn entries(t: &Tree, ids: &[u32], title: impl Into<String>) -> Section {
    let rows = ids.iter().map(|&id| {
        let n = &t.nodes[id as usize];
        vec![
            t.path(id).display().to_string(),
            if n.is_dir { "dir".into() } else { t.ext(id) },
            human(n.size),
            n.size.to_string(),
            if n.is_dir { n.files.to_string() } else { String::new() },
            date(n.modified),
        ]
    });
    sec(title, &["Path", "Type", "Size", "Bytes", "Files", "Modified"], rows.collect())
}

fn buckets(title: &str, v: &[Bucket], top: usize) -> Section {
    let total: u64 = v.iter().map(|b| b.bytes).sum::<u64>().max(1);
    let rows = v.iter().take(top).map(|b| {
        vec![b.label.clone(), human(b.bytes), b.bytes.to_string(), b.count.to_string(), format!("{:.1}%", b.bytes as f64 / total as f64 * 100.0)]
    });
    sec(title, &["Group", "Size", "Bytes", "Files", "Share"], rows.collect())
}

pub fn dup_section(t: &Tree, groups: &[DupGroup], top: usize) -> Section {
    let wasted: u64 = groups.iter().map(|g| g.size * (g.files.len() as u64 - 1)).sum();
    let mut rows = vec![];
    for (i, g) in groups.iter().take(top).enumerate() {
        for &f in &g.files {
            rows.push(vec![(i + 1).to_string(), human(g.size), g.size.to_string(), t.path(f).display().to_string(), date(t.nodes[f as usize].modified)]);
        }
    }
    sec(format!("Duplicates — {} groups, {} reclaimable", groups.len(), human(wasted)), &["Group", "Size", "Bytes", "Path", "Modified"], rows)
}

fn junk_section(t: &Tree, root: u32) -> Section {
    let j = ops::junk(t, root);
    let total: u64 = j.iter().map(|j| t.nodes[j.id as usize].size).sum();
    let rows = j.iter().map(|j| {
        let n = &t.nodes[j.id as usize];
        vec![t.path(j.id).display().to_string(), j.why.into(), human(n.size), n.size.to_string()]
    });
    sec(format!("Caches & junk — {} items, {}", j.len(), human(total)), &["Path", "Reason", "Size", "Bytes"], rows.collect())
}

fn disk_section() -> Section {
    let disks = ops::disks();
    let rows = disks.iter().map(|d| {
        vec![d.mount.display().to_string(), d.name.clone(), d.kind.into(), d.fs.clone(), human(d.total), human(d.free), format!("{:.0}%", d.free as f64 / d.total as f64 * 100.0), d.removable.to_string()]
    });
    sec("Disks", &["Mount", "Name", "Kind", "FS", "Total", "Free", "Free %", "Removable"], rows.collect())
}

fn tip_section(tips: &[ops::Tip]) -> Section {
    let rows = tips.iter().map(|t| vec![t.title.clone(), t.detail.clone(), if t.bytes > 0 { human(t.bytes) } else { String::new() }]);
    sec("Suggestions", &["Suggestion", "Detail", "Space at stake"], rows.collect())
}

fn all_dupes(t: &Tree, root: u32, f: Option<&Expr>, min: u64) -> Vec<DupGroup> {
    let files = t
        .descendants(root)
        .filter(|&i| {
            let n = &t.nodes[i as usize];
            !n.is_dir && n.size >= min.max(1) && f.map_or(true, |f| f.eval(t, i))
        })
        .map(|i| (i, t.path(i), t.nodes[i as usize].size))
        .collect();
    ops::dupes(files, &Default::default(), &Default::default())
}

/// (id, label, on by default)
pub const SECTIONS: [(&str, &str, bool); 14] = [
    ("summary", "Summary", true),
    ("top_files", "Largest files", true),
    ("top_folders", "Largest folders", true),
    ("categories", "By category", true),
    ("extensions", "By extension", true),
    ("ages", "By file age", true),
    ("sizes", "By file size", true),
    ("old_large", "Old & large files (>100 MB, untouched for a year)", true),
    ("empty_folders", "Empty folders", true),
    ("junk", "Caches & junk", true),
    ("tree", "Folder tree (3 levels)", true),
    ("disks", "Disks", true),
    ("tips", "Suggestions", true),
    ("duplicates", "Duplicates (slow: hashes files)", false),
];

/// Build report sections for the subtree at `root`. Empty `which` = all default sections.
pub fn build(t: &Tree, root: u32, f: Option<&Expr>, which: &[String], top: usize, dups: Option<&[DupGroup]>) -> Vec<Section> {
    let has = |s: &str| if which.is_empty() { SECTIONS.iter().any(|x| x.0 == s && x.2) } else { which.iter().any(|w| w == s) };
    let all: Vec<u32> = t.descendants(root).filter(|&i| f.map_or(true, |f| f.eval(t, i))).collect();
    let node = |i: u32| &t.nodes[i as usize];
    let biggest = |dirs: bool, keep: &dyn Fn(u32) -> bool| {
        let mut v: Vec<u32> = all.iter().copied().filter(|&i| node(i).is_dir == dirs && keep(i)).collect();
        v.sort_by_key(|&i| std::cmp::Reverse(node(i).size));
        v.truncate(top);
        v
    };
    let st = stats(t, root, f);
    let mut out = vec![];
    if has("summary") {
        let r = node(root);
        let files: Vec<u32> = all.iter().copied().filter(|&i| !node(i).is_dir).collect();
        let dated = || files.iter().copied().filter(|&i| node(i).modified > 0);
        let show = |i: Option<u32>| i.map_or(String::new(), |i| format!("{} ({}, {})", t.path(i).display(), human(node(i).size), date(node(i).modified)));
        let kv = |k: &str, v: String| vec![k.to_string(), v];
        out.push(sec("Summary", &["Metric", "Value"], vec![
            kv("Path", t.path(root).display().to_string()),
            kv("Generated", date(now())),
            kv("Total size", format!("{} ({} bytes)", human(r.size), commas(r.size))),
            kv("Allocated on disk", human(r.alloc)),
            kv("Files", commas(r.files)),
            kv("Folders", commas(r.dirs)),
            kv("Filter", if f.is_some() { "active".into() } else { "none".into() }),
            kv("Files matching filter", format!("{} ({})", commas(files.len() as u64), human(files.iter().map(|&i| node(i).size).sum()))),
            kv("Average file size", human(files.iter().map(|&i| node(i).size).sum::<u64>() / files.len().max(1) as u64)),
            kv("Largest file", show(files.iter().copied().max_by_key(|&i| node(i).size))),
            kv("Newest file", show(dated().max_by_key(|&i| node(i).modified))),
            kv("Oldest file", show(dated().min_by_key(|&i| node(i).modified))),
            kv("Deepest nesting", all.iter().map(|&i| node(i).depth - r.depth).max().unwrap_or(0).to_string()),
        ]));
    }
    if has("top_files") {
        out.push(entries(t, &biggest(false, &|_| true), "Largest files"));
    }
    if has("top_folders") {
        out.push(entries(t, &biggest(true, &|_| true), "Largest folders"));
    }
    for (id, title, v) in [("categories", "By category", &st.cats), ("extensions", "By extension", &st.exts), ("ages", "By file age", &st.ages), ("sizes", "By file size", &st.sizes)] {
        if has(id) {
            out.push(buckets(title, v, top.max(6)));
        }
    }
    if has("old_large") {
        let old = now() - 365 * 86400;
        out.push(entries(t, &biggest(false, &|i| node(i).size > 100 << 20 && node(i).modified < old), "Old & large files"));
    }
    if has("empty_folders") {
        let v: Vec<u32> = all.iter().copied().filter(|&i| node(i).is_dir && node(i).files == 0).take(top).collect();
        out.push(entries(t, &v, "Empty folders"));
    }
    if has("junk") {
        out.push(junk_section(t, root));
    }
    if has("tree") {
        fn walk(t: &Tree, id: u32, depth: usize, total: u64, rows: &mut Vec<Vec<String>>) {
            let n = &t.nodes[id as usize];
            rows.push(vec![format!("{}{}", "    ".repeat(depth), n.name), human(n.size), n.size.to_string(), n.files.to_string(), format!("{:.1}%", n.size as f64 / total as f64 * 100.0)]);
            if depth < 3 {
                // ponytail: 15 biggest subfolders per level keeps the tree readable; raise if someone wants it all
                for &c in n.children.iter().filter(|&&c| t.nodes[c as usize].is_dir).take(15) {
                    walk(t, c, depth + 1, total, rows);
                }
            }
        }
        let mut rows = vec![];
        walk(t, root, 0, node(root).size.max(1), &mut rows);
        out.push(sec("Folder tree", &["Folder", "Size", "Bytes", "Files", "Share"], rows));
    }
    if has("disks") {
        out.push(disk_section());
    }
    if has("tips") {
        let d = ops::disks();
        let mut tips = ops::system_tips(&d);
        tips.extend(ops::offload_tips(t, &d));
        out.push(tip_section(&tips));
    }
    if has("duplicates") {
        let owned;
        let groups = match dups {
            Some(d) => d,
            None => {
                owned = all_dupes(t, root, f, 1 << 20);
                &owned
            }
        };
        out.push(dup_section(t, groups, top));
    }
    out
}

// ───────────── rendering ─────────────

pub const FORMATS: [&str; 5] = ["txt", "md", "html", "csv", "json"];

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

pub fn render(secs: &[Section], fmt: &str) -> Result<String, String> {
    let mut o = String::new();
    match fmt {
        "json" => {
            let v: Vec<Value> = secs
                .iter()
                .map(|s| json!({ "title": s.title, "rows": s.rows.iter().map(|r| s.cols.iter().cloned().zip(r.iter().map(|c| json!(c))).collect::<serde_json::Map<_, _>>()).collect::<Vec<_>>() }))
                .collect();
            o = serde_json::to_string_pretty(&v).unwrap();
        }
        "csv" => {
            let q = |c: &String| if c.contains([',', '"', '\n']) { format!("\"{}\"", c.replace('"', "\"\"")) } else { c.clone() };
            for s in secs {
                o += &format!("# {}\n{}\n", s.title, s.cols.iter().map(q).collect::<Vec<_>>().join(","));
                for r in &s.rows {
                    o += &(r.iter().map(q).collect::<Vec<_>>().join(",") + "\n");
                }
                o.push('\n');
            }
        }
        "md" => {
            for s in secs {
                o += &format!("## {}\n\n| {} |\n|{}\n", s.title, s.cols.join(" | "), "---|".repeat(s.cols.len()));
                for r in &s.rows {
                    o += &format!("| {} |\n", r.iter().map(|c| c.replace('|', "\\|")).collect::<Vec<_>>().join(" | "));
                }
                o.push('\n');
            }
        }
        "txt" => {
            for s in secs {
                let w: Vec<usize> = (0..s.cols.len())
                    .map(|i| s.rows.iter().map(|r| r[i].chars().count()).chain([s.cols[i].len()]).max().unwrap_or(0))
                    .collect();
                let line = |r: &[String]| r.iter().enumerate().map(|(i, c)| format!("{c:<0$}", w[i])).collect::<Vec<_>>().join("  ").trim_end().to_string() + "\n";
                o += &format!("== {} ==\n{}", s.title, line(&s.cols));
                o += &(w.iter().map(|&n| "-".repeat(n)).collect::<Vec<_>>().join("  ") + "\n");
                for r in &s.rows {
                    o += &line(r);
                }
                o.push('\n');
            }
        }
        "html" => {
            o += "<!doctype html><meta charset=utf-8><title>DiskVibe report</title><style>\
body{font:14px system-ui,sans-serif;background:#0f1117;color:#e6e8f0;max-width:1200px;margin:2em auto;padding:0 1em}\
h1{color:#a78bfa}h2{margin-top:2em;color:#22d3ee;font-size:1.1em}table{border-collapse:collapse;width:100%}\
th{text-align:left;color:#8a90a6;font-weight:600}td,th{padding:5px 10px;border-bottom:1px solid #232838;vertical-align:top;white-space:pre-wrap;word-break:break-word}\
.bar{background:linear-gradient(90deg,#7c5cff55 var(--p),transparent var(--p))}tr:hover{background:#1c2030}</style><h1>DiskVibe report</h1>";
            for s in secs {
                // rows get a proportional bar when the section carries raw bytes
                let bytes = s.cols.iter().position(|c| c == "Bytes");
                let max = bytes.map(|b| s.rows.iter().filter_map(|r| r[b].parse::<f64>().ok()).fold(1.0, f64::max));
                o += &format!("<h2>{}</h2><table><tr>{}</tr>", esc(&s.title), s.cols.iter().map(|c| format!("<th>{}</th>", esc(c))).collect::<String>());
                for r in &s.rows {
                    let style = match (bytes, max) {
                        (Some(b), Some(m)) => format!(" class=bar style=\"--p:{:.1}%\"", r[b].parse::<f64>().unwrap_or(0.0) / m * 100.0),
                        _ => String::new(),
                    };
                    o += &format!("<tr{style}>{}</tr>", r.iter().map(|c| format!("<td>{}</td>", esc(c))).collect::<String>());
                }
                o += "</table>";
            }
        }
        _ => return Err(format!("unknown format '{fmt}' (use {})", FORMATS.join(", "))),
    }
    Ok(o)
}

// ───────────── tools (CLI + MCP) ─────────────

/// (name, description, [(param, json type, required, description)])
pub type ToolDef = (&'static str, &'static str, &'static [(&'static str, &'static str, bool, &'static str)]);

const FILTER: (&str, &str, bool, &str) = ("filter", "string", false, "Filter expression, e.g. `size > 100mb and ext in (mp4, mkv) and not hidden`. Run the `help` tool for full syntax.");
const SORT: (&str, &str, bool, &str) = ("sort", "string", false, "Multi-key sort, e.g. `size desc, name`. Default: size desc.");
const LIMIT: (&str, &str, bool, &str) = ("limit", "integer", false, "Max rows (default 100).");
const PATH: (&str, &str, bool, &str) = ("path", "string", true, "Folder or drive to scan (scan results are cached per path until something changes).");

pub const TOOLS: &[ToolDef] = &[
    ("ls", "Sizes of the direct children of a folder (files and subfolders, aggregate sizes).", &[PATH, FILTER, SORT, LIMIT]),
    ("find", "Deep search: every file/folder below a path matching a filter.", &[PATH, FILTER, SORT, LIMIT]),
    ("stats", "Space breakdown by category, extension, file age and file size.", &[PATH, FILTER, LIMIT]),
    ("dupes", "Find duplicate files (size, then BLAKE3 content hash).", &[PATH, FILTER, ("min_size", "string", false, "Ignore files smaller than this (default 1mb)."), LIMIT]),
    ("junk", "Detect cache folders, build outputs, temp files, dumps and other regenerable junk below a path.", &[PATH]),
    ("temp", "List system temp locations and their sizes; with clear=true permanently delete their contents (entries used in the last 24h are kept).", &[("clear", "boolean", false, "Actually delete. Default false = just report.")]),
    ("disks", "List disks: type (SSD/HDD), filesystem, total and free space.", &[]),
    ("advise", "Suggestions: pagefile/hibernation sizing and placement, nearly full disks, and (with path) cold data on an SSD that could move to a roomier HDD.", &[("path", "string", false, "Optional folder to analyse for offload candidates.")]),
    ("organize", "Sort the files directly inside a folder into Videos/, Archives/, Installers/, Documents/… subfolders by extension. Dry run unless apply=true.", &[PATH, ("apply", "boolean", false, "Actually move files. Default false = preview only.")]),
    ("report", "Detailed report of a folder. Pick sections and format; use `out` to write a file.", &[PATH, FILTER, ("sections", "array", false, "Subset of: summary, top_files, top_folders, categories, extensions, ages, sizes, old_large, empty_folders, junk, tree, disks, tips, duplicates. Default: all but duplicates."), ("top", "integer", false, "Rows per section (default 50).")]),
    ("trash", "Move files/folders to the recycle bin (recoverable).", &[("paths", "array", true, "Paths to trash.")]),
    ("bench", "Quick sequential write/read speed test of the disk holding a folder (writes a 256 MB temp file).", &[PATH]),
    ("help", "Filter and sort syntax reference.", &[]),
];

static CACHE: Mutex<Option<(PathBuf, Arc<Tree>)>> = Mutex::new(None);

fn load(path: &str) -> Result<Arc<Tree>, String> {
    let p = PathBuf::from(path);
    if !p.is_dir() {
        return Err(format!("'{path}' is not a folder"));
    }
    let mut c = CACHE.lock().unwrap();
    if let Some((cp, t)) = &*c {
        if *cp == p {
            return Ok(t.clone());
        }
    }
    let t = Arc::new(scan::scan(&p, &Progress::default()));
    *c = Some((p, t.clone()));
    Ok(t)
}

fn call(name: &str, a: &Value) -> Result<Vec<Section>, String> {
    let s = |k: &str| a.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
    let n = |k: &str, d: usize| a.get(k).and_then(Value::as_u64).map_or(d, |v| v as usize);
    let flag = |k: &str| a.get(k).and_then(Value::as_bool).unwrap_or(false);
    let list = |k: &str| -> Vec<String> { a.get(k).and_then(Value::as_array).map_or(vec![], |v| v.iter().filter_map(|x| x.as_str().map(String::from)).collect()) };
    let tree = || load(s("path").ok_or("missing required argument 'path'")?);
    let filter = s("filter").map(query::parse).transpose()?;
    let f = filter.as_ref();
    let stale = || *CACHE.lock().unwrap() = None;
    let msg = |title: String| vec![sec(title, &[], vec![])];

    Ok(match name {
        "ls" | "find" => {
            let t = tree()?;
            let mut ids: Vec<u32> = if name == "ls" {
                t.nodes[0].children.iter().copied().filter(|&i| f.map_or(true, |f| f.eval(&t, i))).collect()
            } else {
                t.descendants(0).filter(|&i| f.map_or(true, |f| f.eval(&t, i))).collect()
            };
            let keys = match s("sort") {
                Some(k) => query::parse_sort(k)?,
                None => vec![SortKey { field: query::Field::Size, desc: true }],
            };
            query::sort(&t, &mut ids, &keys);
            let (count, bytes) = (ids.len(), ids.iter().filter(|&&i| !t.nodes[i as usize].is_dir).map(|&i| t.nodes[i as usize].size).sum());
            ids.truncate(n("limit", 100));
            let title = format!("{} — {} matches ({} in files), showing {} · folder total {}", t.root.display(), count, human(bytes), ids.len(), human(t.nodes[0].size));
            vec![entries(&t, &ids, title)]
        }
        "stats" => {
            let t = tree()?;
            let (st, top) = (stats(&t, 0, f), n("limit", 100));
            vec![buckets("By category", &st.cats, top), buckets("By extension", &st.exts, top), buckets("By file age", &st.ages, top), buckets("By file size", &st.sizes, top)]
        }
        "dupes" => {
            let t = tree()?;
            let min = query::parse_size(s("min_size").unwrap_or("1mb"))? as u64;
            vec![dup_section(&t, &all_dupes(&t, 0, f, min), n("limit", 100))]
        }
        "junk" => {
            let t = tree()?;
            vec![junk_section(&t, 0)]
        }
        "temp" => {
            let dirs = ops::temp_dirs();
            let rows = dirs.iter().map(|d| {
                let (bytes, files) = ops::dir_size(d);
                let mut r = vec![d.display().to_string(), human(bytes), bytes.to_string(), files.to_string()];
                if flag("clear") {
                    let (ok, kept) = ops::clear_dir(d);
                    r.push(format!("deleted {ok} entries, kept {kept} (recent or in use), freed {}", human(bytes.saturating_sub(ops::dir_size(d).0))));
                }
                r
            });
            let mut cols = vec!["Path", "Size", "Bytes", "Files"];
            if flag("clear") {
                cols.push("Result");
            }
            vec![sec("Temp locations", &cols, rows.collect())]
        }
        "disks" => vec![disk_section()],
        "advise" => {
            let d = ops::disks();
            let mut tips = ops::system_tips(&d);
            if s("path").is_some() {
                let t = tree()?;
                tips.extend(ops::offload_tips(&t, &d));
            }
            vec![tip_section(&tips)]
        }
        "organize" => {
            let dir = PathBuf::from(s("path").ok_or("missing required argument 'path'")?);
            if !dir.is_dir() {
                return Err(format!("'{}' is not a folder", dir.display()));
            }
            let plan = ops::organize_plan(&dir);
            let rows = plan.iter().map(|m| vec![m.from.display().to_string(), m.to.display().to_string()]).collect();
            let title = if flag("apply") {
                stale();
                let (ok, errs) = ops::apply_moves(&plan, false);
                format!("Organized: moved {ok} files{}", if errs.is_empty() { String::new() } else { format!(", {} failed: {}", errs.len(), errs.join("; ")) })
            } else {
                format!("Dry run — {} files would move (pass apply=true to do it)", plan.len())
            };
            vec![sec(title, &["From", "To"], rows)]
        }
        "report" => {
            let t = tree()?;
            build(&t, 0, f, &list("sections"), n("top", 50), None)
        }
        "trash" => {
            stale();
            let rows = list("paths").into_iter().map(|p| {
                let res = trash::delete(&p).map_or_else(|e| format!("failed: {e}"), |_| "moved to recycle bin".into());
                vec![p, res]
            });
            vec![sec("Trash", &["Path", "Result"], rows.collect())]
        }
        "bench" => {
            let p = s("path").ok_or("missing required argument 'path'")?;
            let (w, r) = ops::bench(p.as_ref())?;
            vec![sec("Disk speed (sequential, 256 MB)", &["Path", "Write MB/s", "Read MB/s (cached, upper bound)"], vec![vec![p.into(), format!("{w:.0}"), format!("{r:.0}")]])]
        }
        "help" => msg(query::HELP.into()),
        _ => return Err(format!("unknown tool '{name}'")),
    })
}

/// Run a tool and render it. `format` and `out` are accepted by every tool.
pub fn run(name: &str, args: &Value, default_fmt: &str) -> Result<String, String> {
    let secs = call(name, args)?;
    let text = render(&secs, args.get("format").and_then(Value::as_str).unwrap_or(default_fmt))?;
    match args.get("out").and_then(Value::as_str) {
        Some(out) => std::fs::write(out, &text).map(|_| format!("written {} bytes to {out}", text.len())).map_err(|e| format!("{out}: {e}")),
        None => Ok(text),
    }
}
