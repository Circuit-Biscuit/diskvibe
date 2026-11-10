//! Everything that isn't scanning or querying: duplicates, junk/temp, organize, disks & advice.
use crate::api::human;
use crate::scan::{self, category, now, Progress, Tree};
use rayon::prelude::*;
use std::{
    collections::HashMap,
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
    time::Instant,
};

// ───────────── duplicates ─────────────

pub struct DupGroup {
    pub size: u64,
    pub files: Vec<u32>,
}

type Cand = (u32, PathBuf, u64);

fn hash(p: &Path, head_only: bool) -> Option<[u8; 32]> {
    let f = fs::File::open(p).ok()?;
    let mut h = blake3::Hasher::new();
    io::copy(&mut f.take(if head_only { 65536 } else { u64::MAX }), &mut h).ok()?;
    Some(*h.finalize().as_bytes())
}

fn refine(groups: Vec<Vec<Cand>>, head_only: bool, done: &AtomicU64, cancel: &AtomicBool) -> Vec<Vec<Cand>> {
    groups
        .into_par_iter()
        .flat_map_iter(|g| {
            let mut m: HashMap<[u8; 32], Vec<Cand>> = HashMap::new();
            for f in g {
                if cancel.load(Relaxed) {
                    break;
                }
                if let Some(h) = hash(&f.1, head_only) {
                    m.entry(h).or_default().push(f);
                }
                if !head_only {
                    done.fetch_add(1, Relaxed);
                }
            }
            m.into_values().filter(|v| v.len() > 1)
        })
        .collect()
}

/// Same size → same first 64 KB → same full BLAKE3. `done` counts fully hashed files.
pub fn dupes(files: Vec<Cand>, done: &AtomicU64, cancel: &AtomicBool) -> Vec<DupGroup> {
    let mut by_size: HashMap<u64, Vec<Cand>> = HashMap::new();
    for f in files {
        by_size.entry(f.2).or_default().push(f);
    }
    let groups = by_size.into_values().filter(|v| v.len() > 1).collect();
    let mut out: Vec<DupGroup> = refine(refine(groups, true, done, cancel), false, done, cancel)
        .into_iter()
        .map(|g| DupGroup { size: g[0].2, files: g.into_iter().map(|f| f.0).collect() })
        .collect();
    out.sort_by_key(|g| std::cmp::Reverse(g.size * (g.files.len() as u64 - 1)));
    out
}

// ───────────── junk (caches / temp files inside a scan) ─────────────

pub struct Junk {
    pub id: u32,
    pub why: &'static str,
}

pub fn junk(t: &Tree, root: u32) -> Vec<Junk> {
    let sibling = |id: u32, name: &str| {
        let p = t.nodes[id as usize].parent;
        t.nodes[p as usize].children.iter().any(|&c| t.nodes[c as usize].name.eq_ignore_ascii_case(name))
    };
    let mut out = vec![];
    let (mut i, end) = (root + 1, t.nodes[root as usize].end);
    while i < end {
        let n = &t.nodes[i as usize];
        let name = n.name.to_lowercase();
        let why = if n.dead || n.size == 0 {
            None
        } else if n.is_dir {
            match name.as_str() {
                "node_modules" => Some("Node.js dependencies (npm install restores them)"),
                "__pycache__" | ".pytest_cache" | ".mypy_cache" | ".ruff_cache" | ".tox" => Some("Python cache"),
                ".cache" | "cache" | "caches" | "code cache" | "gpucache" | "shadercache" | "cacheddata"
                | "cache_data" | "dawncache" | "grshadercache" | "inetcache" | "d3dscache" => Some("Application cache"),
                ".gradle" | ".parcel-cache" | ".next" | ".nuxt" | ".turbo" | ".angular" => Some("Build tool cache"),
                "deriveddata" => Some("Xcode build cache"),
                "temp" | "tmp" | ".tmp" => Some("Temporary folder"),
                "crashdumps" | "crashpad" | "crash reports" | "crashes" => Some("Crash dumps"),
                "thumbnails" | ".thumbnails" => Some("Thumbnail cache"),
                "$recycle.bin" | ".trash" => Some("Recycle bin"),
                "windows.old" => Some("Previous Windows installation"),
                "target" if sibling(i, "Cargo.toml") => Some("Rust build output (cargo build restores it)"),
                "library" if sibling(i, "Assets") && sibling(i, "ProjectSettings") => {
                    Some("Unity Library cache (regenerated on project open)")
                }
                _ => None,
            }
        } else {
            match (name.as_str(), t.ext(i).as_str()) {
                ("thumbs.db" | ".ds_store", _) => Some("Thumbnail/metadata litter"),
                (_, "tmp" | "temp") => Some("Temporary file"),
                (_, "dmp" | "mdmp") => Some("Crash dump"),
                (_, "bak" | "old") => Some("Backup leftover"),
                (_, "crdownload" | "part") => Some("Unfinished download"),
                (_, "log") if n.size > 10 << 20 => Some("Large log file"),
                _ => None,
            }
        };
        match why {
            Some(why) => {
                out.push(Junk { id: i, why });
                i = n.end.max(i + 1); // don't report things inside something already reported
            }
            None => i += 1,
        }
    }
    out.sort_by_key(|j| std::cmp::Reverse(t.nodes[j.id as usize].size));
    out
}

// ───────────── system temp locations ─────────────

pub fn temp_dirs() -> Vec<PathBuf> {
    let env = |k: &str, sub: &str| std::env::var_os(k).map(|v| PathBuf::from(v).join(sub));
    let mut v = vec![Some(std::env::temp_dir())];
    if cfg!(windows) {
        v.extend([
            env("WINDIR", "Temp"),
            env("LOCALAPPDATA", "CrashDumps"),
            env("LOCALAPPDATA", "Microsoft\\Windows\\INetCache"),
            env("WINDIR", "SoftwareDistribution\\Download"),
        ]);
    } else if cfg!(target_os = "macos") {
        v.extend([env("HOME", "Library/Caches"), env("HOME", "Library/Logs")]);
    } else {
        v.extend([Some("/var/tmp".into()), env("HOME", ".cache")]);
    }
    let mut out: Vec<PathBuf> = v.into_iter().flatten().filter(|p| p.is_dir()).collect();
    out.dedup();
    out
}

pub fn dir_size(p: &Path) -> (u64, u64) {
    let t = scan::scan(p, &Progress::default());
    (t.nodes[0].size, t.nodes[0].files)
}

/// Permanently delete the contents of a temp dir. Entries touched in the last 24h are kept
/// (likely in use); locked files just fail and are counted. Returns (deleted, kept).
pub fn clear_dir(p: &Path) -> (u64, u64) {
    let (mut ok, mut kept) = (0, 0);
    for e in fs::read_dir(p).into_iter().flatten().flatten() {
        let Ok(md) = e.metadata() else { continue };
        let fresh = md.modified().ok().and_then(|m| m.elapsed().ok()).map_or(true, |a| a.as_secs() < 86400);
        let res = if fresh {
            Err(io::ErrorKind::Other.into())
        } else if md.is_dir() {
            fs::remove_dir_all(e.path())
        } else {
            fs::remove_file(e.path())
        };
        if res.is_ok() { ok += 1 } else { kept += 1 }
    }
    (ok, kept)
}

// ───────────── organize ─────────────

pub struct Move {
    pub from: PathBuf,
    pub to: PathBuf,
}

/// Plan: every known-type file directly inside `dir` goes to `dir/<Category>/`. Nothing is touched.
pub fn organize_plan(dir: &Path) -> Vec<Move> {
    let mut out: Vec<Move> = vec![];
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !e.metadata().is_ok_and(|m| m.is_file()) || name.starts_with('.') || name.eq_ignore_ascii_case("desktop.ini") {
            continue;
        }
        let from = e.path();
        let ext = from.extension().map(|x| x.to_string_lossy().to_lowercase()).unwrap_or_default();
        let cat = category(&ext);
        if cat == "Other" {
            continue;
        }
        let stem = from.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let mut to = dir.join(cat).join(&name);
        let mut n = 1;
        while to.exists() || out.iter().any(|m| m.to == to) {
            to = dir.join(cat).join(format!("{stem} ({n}).{ext}"));
            n += 1;
        }
        out.push(Move { from, to });
    }
    out.sort_by(|a, b| a.to.cmp(&b.to));
    out
}

/// Apply (or with `undo`, reverse) a plan. Never overwrites. Returns (moved, errors).
pub fn apply_moves(moves: &[Move], undo: bool) -> (usize, Vec<String>) {
    let (mut ok, mut errs) = (0, vec![]);
    for m in moves {
        let (from, to) = if undo { (&m.to, &m.from) } else { (&m.from, &m.to) };
        let res = if to.exists() {
            Err(io::Error::new(io::ErrorKind::AlreadyExists, "target already exists"))
        } else {
            to.parent().map_or(Ok(()), fs::create_dir_all).and_then(|_| fs::rename(from, to))
        };
        match res {
            Ok(()) => ok += 1,
            Err(e) => errs.push(format!("{}: {e}", from.display())),
        }
        if undo {
            let _ = from.parent().map(fs::remove_dir); // only succeeds when empty
        }
    }
    (ok, errs)
}

// ───────────── disks & advice ─────────────

#[derive(Clone)]
pub struct Disk {
    pub name: String,
    pub mount: PathBuf,
    pub fs: String,
    pub kind: &'static str, // "SSD" | "HDD" | "?"
    pub total: u64,
    pub free: u64,
    pub removable: bool,
}

pub fn disks() -> Vec<Disk> {
    sysinfo::Disks::new_with_refreshed_list()
        .list()
        .iter()
        .filter(|d| d.total_space() > 0)
        .map(|d| Disk {
            name: d.name().to_string_lossy().into_owned(),
            mount: d.mount_point().to_path_buf(),
            fs: d.file_system().to_string_lossy().into_owned(),
            kind: match d.kind() {
                sysinfo::DiskKind::SSD => "SSD",
                sysinfo::DiskKind::HDD => "HDD",
                _ => "?",
            },
            total: d.total_space(),
            free: d.available_space(),
            removable: d.is_removable(),
        })
        .collect()
}

fn disk_of<'a>(p: &Path, disks: &'a [Disk]) -> Option<&'a Disk> {
    let p = p.to_string_lossy().to_lowercase();
    disks
        .iter()
        .filter(|d| p.starts_with(&d.mount.to_string_lossy().to_lowercase()))
        .max_by_key(|d| d.mount.as_os_str().len())
}

pub struct Tip {
    pub title: String,
    pub detail: String,
    pub bytes: u64,        // space at stake, 0 if informational
    pub dir: Option<u32>,  // tree node this is about
    pub dest: Option<PathBuf>, // offload target folder, when the tip can be executed
}

fn tip(title: String, detail: String, bytes: u64) -> Tip {
    Tip { title, detail, bytes, dir: None, dest: None }
}

/// Pagefile / hibernation / low-space advice. Only suggests — never changes system settings.
pub fn system_tips(disks: &[Disk]) -> Vec<Tip> {
    let mut out = vec![];
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    let (ram, swap) = (sys.total_memory(), sys.total_swap());
    let pct_free = |d: &Disk| d.free as f64 / d.total as f64 * 100.0;
    let fixed: Vec<&Disk> = disks.iter().filter(|d| !d.removable).collect();

    for d in disks.iter().filter(|d| pct_free(d) < 10.0) {
        out.push(tip(
            format!("{} is almost full ({:.0}% free)", d.mount.display(), pct_free(d)),
            format!("Only {} of {} left. Scan it and check Cleanup, Duplicates and the suggestions below.", human(d.free), human(d.total)),
            0,
        ));
    }

    if cfg!(windows) {
        const HOW: &str = "Change it in: Win+R > sysdm.cpl > Advanced > Performance Settings > Advanced > Virtual memory > Change.";
        let size = |d: &Disk, f: &str| fs::metadata(d.mount.join(f)).map_or(0, |m| m.len());
        let pagefiles: Vec<(&Disk, u64)> = fixed.iter().map(|d| (*d, size(d, "pagefile.sys"))).filter(|p| p.1 > 0).collect();
        let list: Vec<String> = pagefiles.iter().map(|(d, s)| format!("{}pagefile.sys {}", d.mount.display(), human(*s))).collect();
        let total: u64 = pagefiles.iter().map(|p| p.1).sum();
        if total > ram + ram / 2 || (ram >= 30 << 30 && total > 16 << 30) {
            out.push(tip(
                format!("Pagefile space is {} — more than this machine needs", human(total)),
                format!("{}. You have {} of RAM; a fixed 4–8 GB pagefile is plenty with this much memory (keep it ≥ RAM only if you need full kernel crash dumps). {HOW}", list.join(", "), human(ram)),
                total.saturating_sub(8 << 30),
            ));
        }
        for &(d, pf) in &pagefiles {
            // best other home for the pagefile: a disk that itself has room; prefer non-HDD, then most free space
            let alt = fixed
                .iter()
                .filter(|o| o.mount != d.mount && o.free > pf * 2 && pct_free(o) >= 20.0)
                .max_by_key(|o| (o.kind != "HDD", o.free));
            if let Some(o) = alt {
                if pct_free(d) < 15.0 {
                    out.push(tip(
                        format!("Move most of the pagefile from {} to {}", d.mount.display(), o.mount.display()),
                        format!("{} has only {:.0}% free while {} ({}) has {} free. Keep a small 1–2 GB pagefile on {} for crash dumps and put the rest on {}.{} {HOW}",
                            d.mount.display(), pct_free(d), o.mount.display(), o.kind, human(o.free), d.mount.display(), o.mount.display(),
                            if o.kind == "HDD" { " It is a hard disk, so paging will be slower — fine if you rarely run out of RAM." } else { "" }),
                        pf.saturating_sub(2 << 30),
                    ));
                } else if d.kind == "HDD" && o.kind == "SSD" {
                    out.push(tip(
                        format!("Pagefile lives on a hard disk ({})", d.mount.display()),
                        format!("{} is an SSD with {} free — paging there is far faster. {HOW}", o.mount.display(), human(o.free)),
                        0,
                    ));
                }
            }
        }
        out.push(tip(
            "Virtual memory overview".into(),
            format!("RAM {} · pagefile(s): {}", human(ram), if list.is_empty() { "none found".into() } else { list.join(", ") }),
            0,
        ));
        for d in &fixed {
            let hib = size(d, "hiberfil.sys");
            if hib > 1 << 30 {
                out.push(tip(
                    format!("Hibernation file uses {} on {}", human(hib), d.mount.display()),
                    "If you never hibernate: run `powercfg /h off` in an admin terminal (also disables Fast Startup). To just shrink it: `powercfg /h /type reduced`.".into(),
                    hib,
                ));
            }
        }
    } else if swap > 0 {
        out.push(tip("Swap overview".into(), format!("RAM {} · swap {} ({} used)", human(ram), human(swap), human(sys.used_swap())), 0));
    }
    out
}

const COLD: [&str; 4] = ["Videos", "Archives", "Disk Images", "Installers"];

/// Direct children of `dir` that don't need fast storage (videos, archives, images, installers).
pub fn cold_files(t: &Tree, dir: u32) -> Vec<u32> {
    let n = &t.nodes[dir as usize];
    n.children.iter().copied().filter(|&c| !t.nodes[c as usize].is_dir && COLD.contains(&category(&t.ext(c)))).collect()
}

/// "You have 40 GB of movies on your SSD, disk J: is an HDD with room" suggestions.
pub fn offload_tips(t: &Tree, disks: &[Disk]) -> Vec<Tip> {
    let Some(src) = disk_of(&t.root, disks) else { return vec![] };
    let src_tight = (src.free as f64) < src.total as f64 * 0.15;
    let mut out = vec![];
    for id in std::iter::once(0).chain(t.descendants(0)).filter(|&i| t.nodes[i as usize].is_dir) {
        let cold = cold_files(t, id);
        let bytes: u64 = cold.iter().map(|&c| t.nodes[c as usize].size).sum();
        if bytes < 1 << 30 {
            continue;
        }
        // target: another fixed disk with comfortable room; a hard disk is the natural home for cold data
        let target = disks
            .iter()
            .filter(|d| !d.removable && d.mount != src.mount && d.free > bytes * 2)
            .filter(|d| (src.kind == "SSD" && d.kind == "HDD") || src_tight)
            .max_by_key(|d| (d.kind == "HDD", d.free));
        let Some(dst) = target else { continue };
        let mut cats: HashMap<&str, u64> = HashMap::new();
        for &c in &cold {
            *cats.entry(category(&t.ext(c))).or_default() += t.nodes[c as usize].size;
        }
        let what = cats.iter().max_by_key(|c| *c.1).map_or("files", |c| c.0).to_lowercase();
        let path = t.path(id);
        let folder = path.file_name().map_or("Offloaded".into(), |f| f.to_string_lossy().into_owned());
        out.push(Tip {
            title: format!("Move {} of {} out of {}", human(bytes), what, path.display()),
            detail: format!(
                "Hey, {} files ({}) in this folder are clogging {} ({}, {} free). They'd work just fine from {} ({}, {} free).",
                cold.len(), what, src.mount.display(), src.kind, human(src.free), dst.mount.display(), dst.kind, human(dst.free)
            ),
            bytes,
            dir: Some(id),
            dest: Some(dst.mount.join("DiskVibe Offload").join(folder)),
        });
    }
    out.sort_by_key(|t| std::cmp::Reverse(t.bytes));
    out.truncate(12);
    out
}

/// Cross-disk move: copy, verify size, then delete the source. Never overwrites. Returns moved ids + errors.
pub fn offload(files: Vec<(u32, PathBuf)>, dest: &Path) -> (Vec<u32>, Vec<String>) {
    let (mut moved, mut errs) = (vec![], vec![]);
    if let Err(e) = fs::create_dir_all(dest) {
        return (moved, vec![format!("{}: {e}", dest.display())]);
    }
    for (id, from) in files {
        let to = dest.join(from.file_name().unwrap_or_default());
        let res = if to.exists() {
            Err(io::Error::new(io::ErrorKind::AlreadyExists, "already exists at destination"))
        } else {
            fs::copy(&from, &to).and_then(|n| {
                if n == fs::metadata(&from)?.len() {
                    fs::remove_file(&from)
                } else {
                    let _ = fs::remove_file(&to);
                    Err(io::Error::new(io::ErrorKind::Other, "copy size mismatch, source kept"))
                }
            })
        };
        match res {
            Ok(()) => moved.push(id),
            Err(e) => errs.push(format!("{}: {e}", from.display())),
        }
    }
    (moved, errs)
}

/// Quick sequential speed test: 256 MB write (fsynced) then read. Returns (write, read) in MB/s.
// ponytail: the read-back is served largely from the OS cache, so treat "read" as an upper bound.
// Real numbers need unbuffered I/O (FILE_FLAG_NO_BUFFERING / O_DIRECT) — add if anyone relies on it.
pub fn bench(dir: &Path) -> Result<(f64, f64), String> {
    const MB: usize = 256;
    let path = dir.join(format!(".diskvibe-bench-{}.tmp", now()));
    let run = || -> io::Result<(f64, f64)> {
        let chunk = vec![0xA5u8; 1 << 20];
        let t = Instant::now();
        let mut f = fs::File::create(&path)?;
        for _ in 0..MB {
            f.write_all(&chunk)?;
        }
        f.sync_all()?;
        let w = MB as f64 / t.elapsed().as_secs_f64();
        let t = Instant::now();
        io::copy(&mut fs::File::open(&path)?, &mut io::sink())?;
        Ok((w, MB as f64 / t.elapsed().as_secs_f64()))
    };
    let res = run().map_err(|e| format!("{}: {e}", dir.display()));
    let _ = fs::remove_file(&path);
    res
}
