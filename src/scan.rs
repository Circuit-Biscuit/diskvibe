//! Parallel directory scanner producing a flat arena tree.
use rayon::prelude::*;
use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Default)]
pub struct Node {
    pub name: String,
    pub parent: u32, // u32::MAX for the root
    pub children: Vec<u32>, // sorted by size, largest first
    pub end: u32,    // subtree occupies ids [self, end) — nodes are stored in DFS preorder
    pub is_dir: bool,
    pub hidden: bool,
    pub readonly: bool,
    pub dead: bool, // removed after the scan (trashed / moved)
    pub size: u64,  // aggregate for dirs
    pub alloc: u64,
    pub files: u64,
    pub dirs: u64,
    pub modified: i64,
    pub created: i64,
    pub accessed: i64,
    pub depth: u16,
}

pub struct Tree {
    pub root: PathBuf,
    pub nodes: Vec<Node>,
    pub scanned: i64, // unix time the scan finished
}

#[derive(Default)]
pub struct Progress {
    pub files: AtomicU64,
    pub bytes: AtomicU64,
    pub cancel: AtomicBool,
}

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

impl Tree {
    pub fn path(&self, id: u32) -> PathBuf {
        let mut parts = vec![];
        let mut i = id;
        while self.nodes[i as usize].parent != u32::MAX {
            parts.push(self.nodes[i as usize].name.as_str());
            i = self.nodes[i as usize].parent;
        }
        let mut p = self.root.clone();
        p.extend(parts.iter().rev());
        p
    }

    /// Lowercase extension, empty for dirs / no extension.
    pub fn ext(&self, id: u32) -> String {
        let n = &self.nodes[id as usize];
        match n.name.rfind('.') {
            Some(i) if i > 0 && !n.is_dir => n.name[i + 1..].to_lowercase(),
            _ => String::new(),
        }
    }

    /// Live descendants of `id` (excluding `id` itself).
    pub fn descendants(&self, id: u32) -> impl Iterator<Item = u32> + '_ {
        (id + 1..self.nodes[id as usize].end).filter(|&i| !self.nodes[i as usize].dead)
    }

    /// Forget a node (after it was deleted/moved on disk) and fix ancestor totals.
    pub fn remove(&mut self, id: u32) {
        let n = &self.nodes[id as usize];
        if n.dead || n.parent == u32::MAX {
            return;
        }
        let (size, alloc, end) = (n.size, n.alloc, n.end);
        let (files, dirs) = (n.files + !n.is_dir as u64, n.dirs + n.is_dir as u64);
        let parent = n.parent;
        self.nodes[parent as usize].children.retain(|&c| c != id);
        let mut p = parent;
        while p != u32::MAX {
            let a = &mut self.nodes[p as usize];
            a.size -= size;
            a.alloc -= alloc;
            a.files -= files;
            a.dirs -= dirs;
            p = a.parent;
        }
        for i in id..end {
            self.nodes[i as usize].dead = true;
        }
    }
}

struct Raw {
    name: String,
    is_dir: bool,
    hidden: bool,
    readonly: bool,
    size: u64,
    alloc: u64,
    times: [i64; 3],
    kids: Vec<Raw>,
}

fn secs(t: std::io::Result<SystemTime>) -> i64 {
    t.ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs() as i64)
}

// ponytail: allocated size on Windows is "rounded up to 4K clusters", not GetCompressedFileSize —
// compressed/sparse/cloud-placeholder files read too big. Swap in the real API if that matters.
#[cfg(windows)]
fn platform(_name: &str, md: &fs::Metadata) -> (bool, u64) {
    use std::os::windows::fs::MetadataExt;
    (md.file_attributes() & 2 != 0, md.len().div_ceil(4096) * 4096)
}
#[cfg(unix)]
fn platform(name: &str, md: &fs::Metadata) -> (bool, u64) {
    use std::os::unix::fs::MetadataExt;
    (name.starts_with('.'), md.blocks() * 512)
}

fn walk(dir: &Path, p: &Progress) -> Vec<Raw> {
    if p.cancel.load(Relaxed) {
        return vec![];
    }
    let entries: Vec<_> = fs::read_dir(dir).into_iter().flatten().flatten().collect();
    entries
        .into_par_iter()
        .filter_map(|e| {
            // DirEntry::metadata does not follow symlinks/junctions, so no cycles and no double counting.
            let md = e.metadata().ok()?;
            let name = e.file_name().to_string_lossy().into_owned();
            let is_dir = md.is_dir();
            let (hidden, alloc) = platform(&name, &md);
            let size = if is_dir { 0 } else { md.len() };
            if !is_dir {
                p.files.fetch_add(1, Relaxed);
                p.bytes.fetch_add(size, Relaxed);
            }
            Some(Raw {
                name,
                is_dir,
                hidden,
                readonly: md.permissions().readonly(),
                size,
                alloc: if is_dir { 0 } else { alloc },
                times: [secs(md.modified()), secs(md.created()), secs(md.accessed())],
                kids: if is_dir { walk(&e.path(), p) } else { vec![] },
            })
        })
        .collect()
}

fn flatten(nodes: &mut Vec<Node>, r: Raw, parent: u32, depth: u16) -> u32 {
    let id = nodes.len() as u32;
    nodes.push(Node {
        name: r.name,
        parent,
        is_dir: r.is_dir,
        hidden: r.hidden,
        readonly: r.readonly,
        size: r.size,
        alloc: r.alloc,
        modified: r.times[0],
        created: r.times[1],
        accessed: r.times[2],
        depth,
        ..Default::default()
    });
    let mut kids = Vec::with_capacity(r.kids.len());
    let (mut size, mut alloc, mut files, mut dirs) = (r.size, r.alloc, 0, 0);
    for k in r.kids {
        let c = flatten(nodes, k, id, depth + 1);
        let n = &nodes[c as usize];
        size += n.size;
        alloc += n.alloc;
        files += n.files + !n.is_dir as u64;
        dirs += n.dirs + n.is_dir as u64;
        kids.push(c);
    }
    kids.sort_by_key(|&c| std::cmp::Reverse(nodes[c as usize].size));
    let end = nodes.len() as u32;
    let n = &mut nodes[id as usize];
    (n.children, n.end, n.size, n.alloc, n.files, n.dirs) = (kids, end, size, alloc, files, dirs);
    id
}

pub fn scan(root: &Path, p: &Progress) -> Tree {
    let md = fs::metadata(root).ok();
    let raw = Raw {
        name: root.display().to_string(),
        is_dir: true,
        hidden: false,
        readonly: false,
        size: 0,
        alloc: 0,
        times: md.map_or([0; 3], |m| [secs(m.modified()), secs(m.created()), secs(m.accessed())]),
        kids: walk(root, p),
    };
    let mut nodes = Vec::new();
    flatten(&mut nodes, raw, u32::MAX, 0);
    Tree { root: root.to_path_buf(), nodes, scanned: now() }
}

// ───────────── saved scans ─────────────
// One file per scanned root, so the app can reopen where you left off without rescanning.
// Format: "DVS1", scanned i64, root string, node count u32, then per node:
// name, parent u32, flags u8, own size u64, own alloc u64, modified/created/accessed i64.
// Folder totals, depth, child lists and subtree ranges are rebuilt on load.

const MAGIC: &[u8; 4] = b"DVS1";

pub fn cache_dir() -> PathBuf {
    let base = ["LOCALAPPDATA", "XDG_CACHE_HOME"].iter().find_map(std::env::var_os).map(PathBuf::from);
    let base = base.or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")));
    base.unwrap_or_else(std::env::temp_dir).join("DiskVibe").join("scans")
}

fn put_str(w: &mut impl Write, s: &str) -> io::Result<()> {
    w.write_all(&(s.len() as u32).to_le_bytes())?;
    w.write_all(s.as_bytes())
}
fn get<const N: usize>(r: &mut impl Read) -> io::Result<[u8; N]> {
    let mut b = [0; N];
    r.read_exact(&mut b)?;
    Ok(b)
}
fn get_str(r: &mut impl Read) -> io::Result<String> {
    let len = u32::from_le_bytes(get(r)?) as usize;
    if len > 1 << 16 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut b = vec![0; len];
    r.read_exact(&mut b)?;
    String::from_utf8(b).map_err(|_| io::ErrorKind::InvalidData.into())
}
fn header(r: &mut impl Read) -> io::Result<(i64, PathBuf)> {
    if &get::<4>(r)? != MAGIC {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok((i64::from_le_bytes(get(r)?), get_str(r)?.into()))
}

/// Saved scans as (file, scanned root, scan time), newest first.
pub fn saved() -> Vec<(PathBuf, PathBuf, i64)> {
    let mut v: Vec<_> = fs::read_dir(cache_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let (at, root) = header(&mut fs::File::open(e.path()).ok()?).ok()?;
            Some((e.path(), root, at))
        })
        .collect();
    v.sort_by_key(|s| std::cmp::Reverse(s.2));
    v
}

impl Tree {
    pub fn save(&self) -> io::Result<()> {
        fs::create_dir_all(cache_dir())?;
        // ponytail: file name is the root path with punctuation flattened — "C:\a b" and "C:\a_b" would
        // share a file (last one wins). Hash the path if that ever bites.
        let name: String = self.root.to_string_lossy().chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
        let mut w = io::BufWriter::new(fs::File::create(cache_dir().join(name + ".dvscan"))?);
        w.write_all(MAGIC)?;
        w.write_all(&self.scanned.to_le_bytes())?;
        put_str(&mut w, &self.root.to_string_lossy())?;
        w.write_all(&(self.nodes.len() as u32).to_le_bytes())?;
        for n in &self.nodes {
            put_str(&mut w, &n.name)?;
            w.write_all(&n.parent.to_le_bytes())?;
            w.write_all(&[n.is_dir as u8 | (n.hidden as u8) << 1 | (n.readonly as u8) << 2 | (n.dead as u8) << 3])?;
            for v in if n.is_dir { [0, 0] } else { [n.size, n.alloc] } {
                w.write_all(&v.to_le_bytes())?;
            }
            for v in [n.modified, n.created, n.accessed] {
                w.write_all(&v.to_le_bytes())?;
            }
        }
        w.flush()
    }

    pub fn load(file: &Path) -> io::Result<Tree> {
        let bad = || io::Error::from(io::ErrorKind::InvalidData);
        let mut r = io::BufReader::new(fs::File::open(file)?);
        let (scanned, root) = header(&mut r)?;
        let count = u32::from_le_bytes(get(&mut r)?) as usize;
        let mut nodes = Vec::with_capacity(count.min(1 << 24));
        for id in 0..count {
            let name = get_str(&mut r)?;
            let parent = u32::from_le_bytes(get(&mut r)?);
            let [flags] = get(&mut r)?;
            // parents always precede their children; anything else is a corrupt file
            if (id == 0) != (parent == u32::MAX) || (id > 0 && parent as usize >= id) {
                return Err(bad());
            }
            nodes.push(Node {
                name,
                parent,
                end: id as u32 + 1,
                is_dir: flags & 1 != 0,
                hidden: flags & 2 != 0,
                readonly: flags & 4 != 0,
                dead: flags & 8 != 0,
                size: u64::from_le_bytes(get(&mut r)?),
                alloc: u64::from_le_bytes(get(&mut r)?),
                modified: i64::from_le_bytes(get(&mut r)?),
                created: i64::from_le_bytes(get(&mut r)?),
                accessed: i64::from_le_bytes(get(&mut r)?),
                ..Default::default()
            });
        }
        if nodes.is_empty() {
            return Err(bad());
        }
        for id in 1..count {
            let p = nodes[id].parent as usize;
            nodes[id].depth = nodes[p].depth + 1;
        }
        // children have larger ids than their parent, so one reverse pass rebuilds every folder total
        for id in (1..count).rev() {
            let (n, p) = (&nodes[id], nodes[id].parent as usize);
            let (end, dead) = (n.end, n.dead);
            let (size, alloc, files, dirs) = (n.size, n.alloc, n.files + !n.is_dir as u64, n.dirs + n.is_dir as u64);
            let pn = &mut nodes[p];
            pn.end = pn.end.max(end);
            if !dead {
                pn.children.push(id as u32);
                (pn.size, pn.alloc, pn.files, pn.dirs) = (pn.size + size, pn.alloc + alloc, pn.files + files, pn.dirs + dirs);
            }
        }
        for id in 0..count {
            let mut kids = std::mem::take(&mut nodes[id].children);
            kids.sort_by_key(|&c| std::cmp::Reverse(nodes[c as usize].size));
            nodes[id].children = kids;
        }
        Ok(Tree { root, nodes, scanned })
    }
}

pub const CATS: [&str; 10] =
    ["Videos", "Audio", "Images", "Documents", "Archives", "Installers", "Disk Images", "Code", "Fonts", "Other"];

/// File category from a lowercase extension. Also the folder name `organize` sorts into.
pub fn category(ext: &str) -> &'static str {
    match ext {
        "mp4" | "mkv" | "avi" | "mov" | "wmv" | "webm" | "flv" | "m4v" | "mpg" | "mpeg" | "m2ts" => "Videos",
        "mp3" | "wav" | "flac" | "aac" | "ogg" | "m4a" | "wma" | "opus" | "aiff" => "Audio",
        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "tiff" | "tif" | "svg" | "heic" | "raw" | "psd" | "ico"
        | "cr2" | "nef" | "arw" => "Images",
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "md" | "rtf" | "odt" | "ods" | "odp"
        | "csv" | "epub" | "mobi" => "Documents",
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" | "zst" | "tgz" | "cab" => "Archives",
        "exe" | "msi" | "msix" | "appx" | "dmg" | "pkg" | "deb" | "rpm" | "appimage" | "apk" => "Installers",
        "iso" | "img" | "vhd" | "vhdx" | "vmdk" | "vdi" | "qcow2" | "wim" | "esd" => "Disk Images",
        "rs" | "py" | "js" | "ts" | "tsx" | "jsx" | "c" | "h" | "cpp" | "hpp" | "cs" | "java" | "go" | "rb" | "php"
        | "html" | "css" | "json" | "xml" | "yaml" | "yml" | "toml" | "sh" | "ps1" | "bat" | "sql" | "kt"
        | "swift" | "lua" => "Code",
        "ttf" | "otf" | "woff" | "woff2" => "Fonts",
        _ => "Other",
    }
}
