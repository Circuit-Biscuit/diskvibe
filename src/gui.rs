//! The desktop app. One global "current folder + filter" drives Explorer, Treemap, Charts,
//! Duplicates and Reports, so every view filters and drills down the same way.
use crate::api::{self, commas, date, human, Stats};
use crate::ops::{self, Disk, DupGroup, Junk, Move, Tip};
use crate::query::{self, Expr, Field, SortKey};
use crate::scan::{self, category, Progress, Tree, CATS};
use eframe::egui::*;
use egui_extras::{Column, TableBuilder};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
        mpsc::{channel, Receiver, Sender},
        Arc,
    },
    time::{Duration, Instant},
};

const BG: Color32 = Color32::from_rgb(13, 15, 22);
const PANEL: Color32 = Color32::from_rgb(19, 22, 33);
const CARD: Color32 = Color32::from_rgb(27, 31, 46);
const TRACK: Color32 = Color32::from_rgb(40, 45, 66);
const ACCENT: Color32 = Color32::from_rgb(124, 92, 255);
const ACCENT2: Color32 = Color32::from_rgb(34, 211, 238);
const TEXT: Color32 = Color32::from_rgb(228, 231, 240);
const DIM: Color32 = Color32::from_rgb(138, 144, 166);
const WARN: Color32 = Color32::from_rgb(251, 191, 36);
const BAD: Color32 = Color32::from_rgb(248, 113, 113);
const GOOD: Color32 = Color32::from_rgb(52, 211, 153);
const PALETTE: [Color32; 10] = [
    Color32::from_rgb(244, 114, 182), // Videos
    Color32::from_rgb(52, 211, 153),  // Audio
    Color32::from_rgb(251, 191, 36),  // Images
    Color32::from_rgb(96, 165, 250),  // Documents
    Color32::from_rgb(251, 146, 60),  // Archives
    Color32::from_rgb(248, 113, 113), // Installers
    Color32::from_rgb(167, 139, 250), // Disk Images
    Color32::from_rgb(34, 211, 238),  // Code
    Color32::from_rgb(163, 230, 53),  // Fonts
    Color32::from_rgb(100, 116, 139), // Other
];

fn cat_color(cat: &str) -> Color32 {
    PALETTE[CATS.iter().position(|c| *c == cat).unwrap_or(9)]
}

/// Category colour; uncategorised files get a muted hue per extension so treemaps aren't one grey slab.
fn file_color(ext: &str) -> Color32 {
    match category(ext) {
        "Other" => ecolor::Hsva::new(ext.bytes().fold(7u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32)) as f32 % 360.0 / 360.0, 0.3, 0.62, 1.0).into(),
        cat => cat_color(cat),
    }
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Explorer,
    Treemap,
    Charts,
    Dupes,
    Cleanup,
    Advisor,
    Organize,
    Reports,
}

const TABS: [(Tab, &str, &str); 8] = [
    (Tab::Explorer, "🗀", "Explorer"),
    (Tab::Treemap, "🗖", "Treemap"),
    (Tab::Charts, "📊", "Charts"),
    (Tab::Dupes, "🗐", "Duplicates"),
    (Tab::Cleanup, "🗑", "Cleanup"),
    (Tab::Advisor, "💡", "Advisor"),
    (Tab::Organize, "📁", "Organize"),
    (Tab::Reports, "📄", "Reports"),
];

#[derive(Clone, Copy, PartialEq)]
enum Col {
    Name,
    Folder,
    Size,
    Pct,
    Matched,
    Alloc,
    Files,
    Dirs,
    Modified,
    Created,
    Accessed,
    Cat,
    Attr,
}

impl Col {
    fn title(self) -> &'static str {
        match self {
            Col::Name => "Name",
            Col::Folder => "In folder",
            Col::Size => "Size",
            Col::Pct => "% of folder",
            Col::Matched => "Matched",
            Col::Alloc => "On disk",
            Col::Files => "Files",
            Col::Dirs => "Folders",
            Col::Modified => "Modified",
            Col::Created => "Created",
            Col::Accessed => "Accessed",
            Col::Cat => "Category",
            Col::Attr => "Attr",
        }
    }
    fn field(self) -> Option<Field> {
        Some(match self {
            Col::Name => Field::Name,
            Col::Folder => Field::Path,
            Col::Size | Col::Pct => Field::Size,
            Col::Alloc => Field::Alloc,
            Col::Files => Field::Files,
            Col::Dirs => Field::Dirs,
            Col::Modified => Field::Modified,
            Col::Created => Field::Created,
            Col::Accessed => Field::Accessed,
            Col::Cat => Field::Category,
            Col::Matched | Col::Attr => return None,
        })
    }
    fn column(self) -> Column {
        match self {
            Col::Name => Column::initial(330.0).at_least(120.0),
            Col::Folder => Column::initial(280.0).at_least(80.0),
            Col::Pct => Column::initial(130.0).at_least(60.0),
            Col::Modified | Col::Created | Col::Accessed => Column::initial(125.0),
            Col::Attr => Column::initial(44.0),
            _ => Column::initial(84.0),
        }
        .clip(true)
    }
}

enum Confirm {
    Trash(Vec<u32>),
    ClearTemp(Vec<PathBuf>),
    Offload(u32, PathBuf),
    Organize,
}

enum Act {
    Scan(PathBuf),
    Go(u32),
    Back,
    Fwd,
    Open(u32),
    Reveal(u32),
    Copy(u32),
    Query(String),
    Show(u32),
    Ask(Confirm),
    Do(Confirm),
}

enum Bg {
    Scanned(Tree),
    Loaded(Result<Tree, String>),
    Dups(Vec<DupGroup>),
    TempSize(usize, u64, u64),
    TempCleared(String),
    Bench(PathBuf, String),
    Offloaded(Vec<u32>, String),
}

struct Tile {
    id: u32,
    rect: Rect,
    leaf: bool,
    color: Color32,
}

struct App {
    tab: Tab,
    modified: bool, // tree changed since it was last saved (items recycled / moved away)
    tx: Sender<Bg>,
    rx: Receiver<Bg>,
    disks: Vec<Disk>,
    tree: Option<Tree>,
    scan: Option<(Arc<Progress>, PathBuf, Instant)>,
    scan_took: f32,
    busy: Option<String>,
    toast: Option<(String, Instant)>,
    confirm: Option<Confirm>,
    // navigation + query (shared by all views)
    cur: u32,
    back: Vec<u32>,
    fwd: Vec<u32>,
    qtext: String,
    filter: Option<Expr>,
    qerr: Option<String>,
    deep: bool,
    show_builder: bool,
    builder: (usize, usize, usize, String),
    sort: Vec<SortKey>,
    dirty: bool,  // rows need recomputing
    fdirty: bool, // filtered sizes need recomputing (filter or tree changed)
    gen: u64,
    rows: Vec<u32>,
    fsize: Vec<u64>, // per node: bytes of matching files in its subtree; empty when no filter
    sel: HashSet<u32>,
    anchor: Option<usize>,
    stats: Option<Stats>,
    tiles: (u64, Rect, Vec<Tile>),
    tm_ctx: Option<u32>,
    // duplicates
    dup_job: Option<(Arc<AtomicU64>, Arc<AtomicBool>)>,
    dups: Vec<DupGroup>,
    dup_sel: HashSet<u32>,
    dup_min: String,
    // cleanup
    temps: Vec<(PathBuf, bool, Option<(u64, u64)>)>,
    temps_measured: bool,
    junk: Vec<Junk>,
    junk_sel: HashSet<u32>,
    // advisor
    sys_tips: Vec<Tip>,
    tips: Vec<Tip>,
    bench: HashMap<PathBuf, String>,
    // organize
    org_dir: Option<PathBuf>,
    org_plan: Vec<Move>,
    org_done: Vec<Move>,
    // reports
    rep_on: Vec<bool>,
    rep_fmt: usize,
    rep_top: usize,
    rep_text: String,
}

pub fn run(initial: Option<PathBuf>) {
    // Same binary serves CLI and GUI, so it is a console app; drop the console window in GUI mode.
    #[cfg(windows)]
    {
        extern "system" {
            fn FreeConsole() -> i32;
        }
        unsafe { FreeConsole() };
    }
    let opts = eframe::NativeOptions {
        viewport: ViewportBuilder::default()
            .with_icon(eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")).expect("bundled icon is a valid PNG"))
            .with_inner_size([1360.0, 860.0]).with_min_inner_size([900.0, 560.0]).with_title("DiskVibe"),
        ..Default::default()
    };
    let res = eframe::run_native("DiskVibe", opts, Box::new(move |cc| Ok(Box::new(App::new(&cc.egui_ctx, initial)))));
    if let Err(e) = res {
        eprintln!("could not start the UI: {e}");
    }
}

fn theme(ctx: &Context) {
    let mut v = Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = CARD;
    v.extreme_bg_color = Color32::from_rgb(9, 10, 16);
    v.faint_bg_color = Color32::from_rgb(20, 23, 35);
    v.override_text_color = Some(TEXT);
    v.window_rounding = 12.0.into();
    v.menu_rounding = 8.0.into();
    v.selection.bg_fill = ACCENT.linear_multiply(0.45);
    v.selection.stroke = Stroke::new(1.0_f32, Color32::WHITE);
    v.hyperlink_color = ACCENT2;
    for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
        w.rounding = 7.0.into();
    }
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, Color32::from_rgb(38, 43, 62));
    v.widgets.inactive.weak_bg_fill = Color32::from_rgb(38, 43, 64);
    v.widgets.inactive.bg_fill = Color32::from_rgb(38, 43, 64);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(54, 60, 90);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    v.widgets.active.weak_bg_fill = ACCENT;
    let mut s = (*ctx.style()).clone();
    s.visuals = v;
    s.spacing.item_spacing = vec2(8.0, 7.0);
    s.spacing.button_padding = vec2(10.0, 5.0);
    s.interaction.selectable_labels = false; // labels must not swallow row clicks
    for (ts, size) in [(TextStyle::Body, 14.0), (TextStyle::Button, 14.0), (TextStyle::Heading, 22.0), (TextStyle::Small, 11.5), (TextStyle::Monospace, 12.5)] {
        let fam = if ts == TextStyle::Monospace { FontFamily::Monospace } else { FontFamily::Proportional };
        s.text_styles.insert(ts, FontId::new(size, fam));
    }
    ctx.set_style(s);
}

// ───────────── small widgets ─────────────

fn card<R>(ui: &mut Ui, title: &str, body: impl FnOnce(&mut Ui) -> R) -> R {
    Frame::none()
        .fill(CARD)
        .rounding(12.0)
        .inner_margin(14.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            if !title.is_empty() {
                ui.label(RichText::new(title).strong().size(15.0));
                ui.add_space(4.0);
            }
            body(ui)
        })
        .inner
}

fn primary(text: &str) -> Button<'static> {
    Button::new(RichText::new(text.to_owned()).color(Color32::WHITE).strong()).fill(ACCENT)
}

fn meter(ui: &mut Ui, frac: f32, color: Color32, height: f32, text: Option<String>) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, height / 2.0, TRACK);
    let mut fill = rect;
    fill.set_width((rect.width() * frac.clamp(0.0, 1.0)).max(if frac > 0.0 { 3.0 } else { 0.0 }));
    p.rect_filled(fill, height / 2.0, color);
    if let Some(t) = text {
        p.text(rect.center(), Align2::CENTER_CENTER, t, FontId::proportional(11.5), TEXT);
    }
}

fn badge(ui: &mut Ui, text: &str, color: Color32) {
    Frame::none().fill(color.linear_multiply(0.25)).rounding(6.0).inner_margin(Margin::symmetric(6.0, 1.0)).show(ui, |ui| {
        ui.label(RichText::new(text).small().strong().color(color));
    });
}

fn disk_card(ui: &mut Ui, d: &Disk, button: &str, extra: Option<&String>) -> bool {
    Frame::none()
        .fill(CARD)
        .rounding(12.0)
        .inner_margin(14.0)
        .show(ui, |ui| {
            ui.set_width(215.0);
            ui.vertical(|ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("🖴 {}", d.mount.display())).strong().size(17.0));
                badge(ui, d.kind, match d.kind {
                    "SSD" => ACCENT2,
                    "HDD" => WARN,
                    _ => DIM,
                });
                if d.removable {
                    badge(ui, "removable", DIM);
                }
            });
            ui.label(RichText::new(format!("{} · {}", if d.name.is_empty() { "Local disk" } else { &d.name }, d.fs)).color(DIM).small());
            let used = 1.0 - d.free as f32 / d.total as f32;
            meter(ui, used, if used > 0.9 { BAD } else if used > 0.75 { WARN } else { ACCENT }, 8.0, None);
            ui.label(format!("{} free of {}", human(d.free), human(d.total)));
            if let Some(e) = extra {
                ui.label(RichText::new(e).color(ACCENT2).small());
            }
            ui.button(button).clicked()
            })
            .inner
        })
        .inner
}

/// Wrapping grid of disk cards. Returns the mount whose button was clicked.
fn disk_grid(ui: &mut Ui, disks: &[Disk], button: &str, extra: &HashMap<PathBuf, String>) -> Option<PathBuf> {
    let mut hit = None;
    let per_row = ((ui.available_width() / 256.0) as usize).max(1);
    for row in disks.chunks(per_row) {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            for d in row {
                if disk_card(ui, d, button, extra.get(&d.mount)) {
                    hit = Some(d.mount.clone());
                }
            }
        });
        ui.add_space(6.0);
    }
    hit
}

/// Horizontal bar list. Returns the clicked row.
fn hbars(ui: &mut Ui, items: &[(String, u64, String, Color32)]) -> Option<usize> {
    let max = items.iter().map(|i| i.1).max().unwrap_or(0).max(1);
    let mut clicked = None;
    for (i, (label, bytes, sub, col)) in items.iter().enumerate() {
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::click());
        let p = ui.painter_at(rect);
        if resp.hovered() {
            p.rect_filled(rect, 5.0, Color32::from_white_alpha(10));
        }
        let (lw, vw) = ((rect.width() * 0.26).min(170.0), 150.0);
        ui.painter_at(Rect::from_min_size(rect.min, vec2(lw - 6.0, rect.height()))).text(rect.left_center() + vec2(5.0, 0.0), Align2::LEFT_CENTER, label, FontId::proportional(13.0), TEXT);
        let bar = Rect::from_min_max(pos2(rect.left() + lw, rect.top() + 7.0), pos2(rect.right() - vw, rect.bottom() - 7.0));
        p.rect_filled(bar, 5.0, TRACK);
        let mut fill = bar;
        fill.set_width(bar.width() * (*bytes as f32 / max as f32));
        p.rect_filled(fill, 5.0, *col);
        p.text(rect.right_center() - vec2(5.0, 0.0), Align2::RIGHT_CENTER, format!("{} · {sub}", human(*bytes)), FontId::proportional(12.0), DIM);
        if resp.on_hover_cursor(CursorIcon::PointingHand).clicked() {
            clicked = Some(i);
        }
    }
    if items.is_empty() {
        ui.label(RichText::new("Nothing here").color(DIM));
    }
    clicked
}

/// Donut chart. Returns the clicked slice.
fn donut(ui: &mut Ui, items: &[(String, u64, String, Color32)], size: f32) -> Option<usize> {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    let total: u64 = items.iter().map(|i| i.1).sum();
    if total == 0 {
        return None;
    }
    let (c, r0, r1) = (rect.center(), size * 0.29, size * 0.47);
    let tau = std::f32::consts::TAU;
    let hover = resp.hover_pos().and_then(|p| {
        let d = p - c;
        (d.length() >= r0 && d.length() <= r1 + 4.0).then(|| d.x.atan2(-d.y).rem_euclid(tau) / tau)
    });
    let (mut acc, mut hit, mut mesh) = (0.0, None, Mesh::default());
    for (i, (_, b, _, col)) in items.iter().enumerate() {
        let (f0, f1) = (acc, acc + *b as f32 / total as f32);
        acc = f1;
        let hov = hover.is_some_and(|h| h >= f0 && h < f1);
        if hov {
            hit = Some(i);
        }
        let (col, grow) = if hov { (col.linear_multiply(1.0).gamma_multiply(1.0), 5.0) } else { (col.gamma_multiply(0.88), 0.0) };
        let steps = (((f1 - f0) * 120.0).ceil() as usize).max(1);
        let gap = if f1 - f0 > 0.01 { 0.0025 } else { 0.0 }; // hairline between slices
        let pt = |f: f32, r: f32| c + vec2((f * tau).sin(), -(f * tau).cos()) * r;
        for s in 0..steps {
            let a = f0 + gap + (f1 - f0 - 2.0 * gap) * s as f32 / steps as f32;
            let b = f0 + gap + (f1 - f0 - 2.0 * gap) * (s + 1) as f32 / steps as f32;
            let base = mesh.vertices.len() as u32;
            for p in [pt(a, r0), pt(a, r1 + grow), pt(b, r1 + grow), pt(b, r0)] {
                mesh.colored_vertex(p, col);
            }
            mesh.add_triangle(base, base + 1, base + 2);
            mesh.add_triangle(base, base + 2, base + 3);
        }
    }
    let p = ui.painter();
    p.add(Shape::mesh(mesh));
    let (top, bottom) = match hit {
        Some(i) => (items[i].0.clone(), format!("{} · {:.1}%", human(items[i].1), items[i].1 as f32 / total as f32 * 100.0)),
        None => ("Total".into(), human(total)),
    };
    p.text(c - vec2(0.0, 9.0), Align2::CENTER_CENTER, top, FontId::proportional(13.0), DIM);
    p.text(c + vec2(0.0, 9.0), Align2::CENTER_CENTER, bottom, FontId::proportional(14.0), TEXT);
    if resp.on_hover_cursor(CursorIcon::PointingHand).clicked() { hit } else { None }
}

/// Classic squarified treemap of `items` (sorted, largest first) into `rect`.
fn squarify(items: &[(u32, f64)], mut rect: Rect, out: &mut Vec<(u32, Rect)>) {
    let total: f64 = items.iter().map(|i| i.1).sum();
    if total <= 0.0 || rect.width() < 1.0 || rect.height() < 1.0 {
        return;
    }
    let scale = (rect.width() * rect.height()) as f64 / total;
    let mut i = 0;
    while i < items.len() {
        let short = rect.width().min(rect.height()) as f64;
        if short < 1.0 {
            break;
        }
        // grow the row while its worst aspect ratio keeps improving
        let (mut j, mut sum, mut best) = (i, 0.0, f64::MAX);
        while j < items.len() {
            let s = sum + items[j].1 * scale;
            let worst = (short * short * items[i].1 * scale / (s * s)).max(s * s / (short * short * items[j].1 * scale));
            if worst > best {
                break;
            }
            (best, sum, j) = (worst, s, j + 1);
        }
        let thick = (sum / short) as f32;
        let wide = rect.width() >= rect.height();
        let mut off = 0.0;
        for it in &items[i..j] {
            let len = (it.1 * scale / sum * short) as f32;
            out.push((it.0, if wide { Rect::from_min_size(rect.min + vec2(0.0, off), vec2(thick, len)) } else { Rect::from_min_size(rect.min + vec2(off, 0.0), vec2(len, thick)) }));
            off += len;
        }
        if wide { rect.min.x += thick } else { rect.min.y += thick }
        i = j;
    }
}

fn tm_layout(t: &Tree, w: &dyn Fn(u32) -> u64, id: u32, rect: Rect, depth: u8, out: &mut Vec<Tile>) {
    let mut items: Vec<(u32, f64)> = t.nodes[id as usize].children.iter().filter(|&&c| !t.nodes[c as usize].dead).map(|&c| (c, w(c) as f64)).filter(|x| x.1 > 0.0).collect();
    items.sort_by(|a, b| b.1.total_cmp(&a.1));
    items.truncate(400); // ponytail: the 400 biggest children per folder; the rest would be sub-pixel anyway
    let mut placed = vec![];
    squarify(&items, rect, &mut placed);
    for (c, r) in placed {
        let r = r.shrink(1.0);
        if r.width() < 2.0 || r.height() < 2.0 {
            continue;
        }
        let n = &t.nodes[c as usize];
        if n.is_dir && depth < 5 && r.width() > 64.0 && r.height() > 46.0 {
            out.push(Tile { id: c, rect: r, leaf: false, color: Color32::from_rgb(30 + depth * 8, 34 + depth * 8, 50 + depth * 10) });
            tm_layout(t, w, c, Rect::from_min_max(r.min + vec2(3.0, 19.0), r.max - vec2(3.0, 3.0)), depth + 1, out);
        } else {
            let color = if n.is_dir { Color32::from_rgb(58, 64, 96) } else { file_color(&t.ext(c)).gamma_multiply(0.82) };
            out.push(Tile { id: c, rect: r, leaf: true, color });
        }
    }
}

fn folder_tree(ui: &mut Ui, t: &Tree, id: u32, cur: u32, acts: &mut Vec<Act>) {
    let n = &t.nodes[id as usize];
    let name = if id == 0 { t.root.display().to_string() } else { n.name.clone() };
    let mut text = RichText::new(format!("{name}   {}", human(n.size)));
    if id == cur {
        text = text.color(ACCENT2).strong();
    }
    if n.dirs == 0 {
        if ui.add(Button::new(text).frame(false)).clicked() {
            acts.push(Act::Go(id));
        }
        return;
    }
    let r = CollapsingHeader::new(text).id_salt(id).default_open(id == 0).show(ui, |ui| {
        // ponytail: 300 biggest subfolders per node keeps huge folders snappy; the table shows everything
        for &c in n.children.iter().filter(|&&c| t.nodes[c as usize].is_dir).take(300) {
            folder_tree(ui, t, c, cur, acts);
        }
    });
    if r.header_response.clicked() {
        acts.push(Act::Go(id));
    }
}

fn reveal(p: &Path) {
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer").arg("/select,").arg(p).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg("-R").arg(p).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = open::that(p.parent().unwrap_or(p));
}

const CHIPS: [(&str, &str); 8] = [
    ("Huge > 1 GB", "size > 1gb"),
    ("Old > 1 year", "age > 365"),
    ("Recent 7 days", "age < 7"),
    ("Videos", "category = videos"),
    ("Archives", "category = archives"),
    ("Installers", "category = installers"),
    ("Empty folders", "dir and empty"),
    ("Hidden", "hidden"),
];
const JOINS: [&str; 4] = ["AND", "OR", "AND NOT", "OR NOT"];
const OPS: [(&str, &str); 9] = [("=", "is"), ("!=", "is not"), (">", ">"), (">=", "≥"), ("<", "<"), ("<=", "≤"), ("~", "contains"), ("!~", "doesn't contain"), ("=~", "matches regex")];

impl App {
    fn new(ctx: &Context, initial: Option<PathBuf>) -> Self {
        theme(ctx);
        let (tx, rx) = channel();
        let disks = ops::disks();
        let mut app = App {
            tab: Tab::Explorer,
            modified: false,
            tx,
            rx,
            sys_tips: ops::system_tips(&disks),
            disks,
            tree: None,
            scan: None,
            scan_took: 0.0,
            busy: None,
            toast: None,
            confirm: None,
            cur: 0,
            back: vec![],
            fwd: vec![],
            qtext: String::new(),
            filter: None,
            qerr: None,
            deep: false,
            show_builder: false,
            builder: (0, 5, 2, String::new()),
            sort: vec![SortKey { field: Field::Size, desc: true }],
            dirty: false,
            fdirty: false,
            gen: 0,
            rows: vec![],
            fsize: vec![],
            sel: HashSet::new(),
            anchor: None,
            stats: None,
            tiles: (u64::MAX, Rect::NOTHING, vec![]),
            tm_ctx: None,
            dup_job: None,
            dups: vec![],
            dup_sel: HashSet::new(),
            dup_min: "1mb".into(),
            temps: ops::temp_dirs().into_iter().map(|p| (p, true, None)).collect(),
            temps_measured: false,
            junk: vec![],
            junk_sel: HashSet::new(),
            tips: vec![],
            bench: HashMap::new(),
            org_dir: None,
            org_plan: vec![],
            org_done: vec![],
            rep_on: api::SECTIONS.iter().map(|s| s.2).collect(),
            rep_fmt: 0,
            rep_top: 50,
            rep_text: String::new(),
        };
        if let Some(p) = initial {
            app.start_scan(p, ctx);
        } else if let Some(last) = scan::saved().into_iter().next() {
            app.load_scan(last.0, ctx); // reopen the most recent scan instead of starting empty
        }
        // DISKVIBE_TAB=charts opens straight on a tab (handy for shortcuts and screenshots)
        if let Some(t) = std::env::var("DISKVIBE_TAB").ok().and_then(|n| TABS.iter().find(|t| t.2.eq_ignore_ascii_case(&n))) {
            app.tab = t.0;
        }
        // DISKVIBE_QUERY="size > 1gb" starts with that filter applied across all subfolders
        if let Ok(q) = std::env::var("DISKVIBE_QUERY") {
            app.qtext = q;
            app.set_query();
            app.deep = true;
        }
        app
    }

    fn spawn(&self, ctx: &Context, f: impl FnOnce() -> Bg + Send + 'static) {
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(f());
            ctx.request_repaint();
        });
    }

    fn say(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    fn start_scan(&mut self, path: PathBuf, ctx: &Context) {
        if let Some((p, ..)) = &self.scan {
            p.cancel.store(true, Relaxed);
        }
        let p = Arc::new(Progress::default());
        self.scan = Some((p.clone(), path.clone(), Instant::now()));
        self.spawn(ctx, move || {
            let t = scan::scan(&path, &p);
            if !p.cancel.load(Relaxed) {
                let _ = t.save(); // so the next launch opens straight into this scan
            }
            Bg::Scanned(t)
        });
    }

    fn load_scan(&mut self, file: PathBuf, ctx: &Context) {
        self.busy = Some("Opening saved scan…".into());
        self.spawn(ctx, move || Bg::Loaded(Tree::load(&file).map_err(|e| format!("Could not open saved scan: {e}"))));
    }

    fn set_tree(&mut self, t: Tree) {
        self.tree = Some(t);
        (self.cur, self.anchor, self.modified) = (0, None, false);
        self.back.clear();
        self.fwd.clear();
        self.sel.clear();
        self.dups.clear();
        self.dup_sel.clear();
        self.junk_sel.clear();
        self.rep_text.clear();
        self.disks = ops::disks();
        self.tree_changed();
    }

    fn measure_temps(&mut self, ctx: &Context) {
        self.temps_measured = true;
        for (i, (p, _, size)) in self.temps.iter_mut().enumerate() {
            *size = None;
            let p = p.clone();
            let (tx, ctx) = (self.tx.clone(), ctx.clone());
            std::thread::spawn(move || {
                let (b, f) = ops::dir_size(&p);
                let _ = tx.send(Bg::TempSize(i, b, f));
                ctx.request_repaint();
            });
        }
    }

    /// Call after the tree was replaced or nodes were removed.
    fn tree_changed(&mut self) {
        let Some(t) = &self.tree else { return };
        self.junk = ops::junk(t, 0);
        self.tips = ops::offload_tips(t, &self.disks);
        let alive = |i: &u32| !t.nodes[*i as usize].dead;
        self.sel.retain(alive);
        self.dup_sel.retain(alive);
        self.junk_sel.retain(alive);
        for g in &mut self.dups {
            g.files.retain(alive);
        }
        self.dups.retain(|g| g.files.len() > 1);
        (self.dirty, self.fdirty) = (true, true);
    }

    fn poll(&mut self, ctx: &Context) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Bg::Scanned(t) => {
                    let Some((p, path, t0)) = &self.scan else { continue };
                    if *path != t.root {
                        continue; // a superseded scan finishing late
                    }
                    let cancelled = p.cancel.load(Relaxed);
                    self.scan_took = t0.elapsed().as_secs_f32();
                    self.scan = None;
                    if cancelled {
                        self.say("Scan cancelled");
                        continue;
                    }
                    self.say(format!("Scanned {} files in {:.1}s", commas(t.nodes[0].files), self.scan_took));
                    self.set_tree(t);
                }
                Bg::Loaded(res) => {
                    self.busy = None;
                    match res {
                        Ok(t) => {
                            self.say(format!("Opened saved scan of {} from {} — Rescan to refresh", t.root.display(), date(t.scanned)));
                            self.set_tree(t);
                        }
                        Err(e) => self.say(e),
                    }
                }
                Bg::Dups(d) => {
                    self.dup_job = None;
                    let wasted: u64 = d.iter().map(|g| g.size * (g.files.len() as u64 - 1)).sum();
                    self.say(format!("{} duplicate groups — {} reclaimable", d.len(), human(wasted)));
                    self.dups = d;
                    self.dup_sel.clear();
                }
                Bg::TempSize(i, b, f) => self.temps[i].2 = Some((b, f)),
                Bg::TempCleared(msg) => {
                    self.busy = None;
                    self.say(msg);
                    self.measure_temps(ctx);
                    self.disks = ops::disks();
                }
                Bg::Bench(p, s) => {
                    self.bench.insert(p, s);
                }
                Bg::Offloaded(ids, msg) => {
                    self.busy = None;
                    self.say(msg);
                    if let Some(t) = &mut self.tree {
                        ids.into_iter().for_each(|i| t.remove(i));
                        self.modified = true;
                    }
                    self.disks = ops::disks();
                    self.tree_changed();
                }
            }
        }
        if self.scan.is_some() || self.dup_job.is_some() || self.busy.is_some() {
            ctx.request_repaint_after(Duration::from_millis(120));
        }
    }

    fn set_query(&mut self) {
        match self.qtext.trim() {
            "" => (self.filter, self.qerr) = (None, None),
            q => match query::parse(q) {
                Ok(e) => (self.filter, self.qerr) = (Some(e), None),
                Err(e) => self.qerr = Some(e), // keep the last good filter while typing
            },
        }
        (self.dirty, self.fdirty) = (true, true);
    }

    fn refresh(&mut self) {
        self.dirty = false;
        self.gen += 1;
        self.stats = None;
        let Some(t) = &self.tree else { return };
        if t.nodes[self.cur as usize].dead {
            self.cur = 0;
        }
        let f = self.filter.as_ref();
        if std::mem::take(&mut self.fdirty) {
            self.fsize.clear();
            if let Some(f) = f {
                // children always have larger ids than their parent, so one reverse pass aggregates
                let mut v = vec![0u64; t.nodes.len()];
                for id in (0..t.nodes.len()).rev() {
                    let n = &t.nodes[id];
                    if n.dead {
                        continue;
                    }
                    if !n.is_dir && f.eval(t, id as u32) {
                        v[id] = n.size;
                    }
                    if n.parent != u32::MAX {
                        v[n.parent as usize] += v[id];
                    }
                }
                self.fsize = v;
            }
        }
        let hit = |i: u32| f.map_or(true, |f| f.eval(t, i));
        self.rows = if self.deep {
            t.descendants(self.cur).filter(|&i| hit(i)).collect()
        } else {
            // a folder stays visible when something inside it matches
            t.nodes[self.cur as usize].children.iter().copied().filter(|&i| hit(i) || self.fsize.get(i as usize).is_some_and(|&b| b > 0)).collect()
        };
        query::sort(t, &mut self.rows, &self.sort);
    }

    // ───────────── chrome ─────────────

    fn top_bar(&mut self, ui: &mut Ui, acts: &mut Vec<Act>) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("DiskVibe").size(21.0).strong().color(ACCENT));
            ui.add_space(14.0);
            if let Some((p, path, _)) = &self.scan {
                ui.spinner();
                ui.label(format!("Scanning {} — {} files, {}", path.display(), commas(p.files.load(Relaxed)), human(p.bytes.load(Relaxed))));
                if ui.button("Cancel").clicked() {
                    p.cancel.store(true, Relaxed);
                }
            } else {
                ui.menu_button("🖴  Scan a drive", |ui| {
                    for d in &self.disks {
                        if ui.button(format!("{}   {} · {} free of {}", d.mount.display(), d.kind, human(d.free), human(d.total))).clicked() {
                            acts.push(Act::Scan(d.mount.clone()));
                            ui.close_menu();
                        }
                    }
                });
                if ui.button("🗀  Scan a folder…").clicked() {
                    if let Some(p) = rfd::FileDialog::new().pick_folder() {
                        acts.push(Act::Scan(p));
                    }
                }
                if let Some(t) = &self.tree {
                    if ui.button("⟲  Rescan").on_hover_text(format!("{} — last scanned {}", t.root.display(), date(t.scanned))).clicked() {
                        acts.push(Act::Scan(t.root.clone()));
                    }
                }
                let mut open = None;
                ui.menu_button("💾  Saved scans", |ui| {
                    let saved = scan::saved();
                    if saved.is_empty() {
                        ui.label(RichText::new("Every scan is saved automatically and listed here.").color(DIM));
                    }
                    for (file, root, at) in saved {
                        ui.horizontal(|ui| {
                            if ui.button(format!("{}   ·   {}", root.display(), date(at))).clicked() {
                                open = Some(file.clone());
                                ui.close_menu();
                            }
                            if ui.small_button("✖").on_hover_text("Forget this saved scan").clicked() {
                                let _ = std::fs::remove_file(&file);
                            }
                        });
                    }
                });
                if let Some(file) = open {
                    self.load_scan(file, ui.ctx());
                }
            }
            if let Some(b) = &self.busy {
                ui.spinner();
                ui.label(RichText::new(b).color(WARN));
            }
        });
    }

    fn status(&self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            match (&self.toast, &self.tree) {
                (Some((msg, at)), _) if at.elapsed().as_secs() < 7 => {
                    ui.label(RichText::new(msg).color(GOOD));
                    ui.ctx().request_repaint_after(Duration::from_secs(1));
                }
                (_, Some(t)) => {
                    let r = &t.nodes[0];
                    ui.label(RichText::new(format!("{} · {} · {} files · {} folders · scanned {}", t.root.display(), human(r.size), commas(r.files), commas(r.dirs), date(t.scanned))).color(DIM));
                }
                _ => {
                    ui.label(RichText::new("No scan yet").color(DIM));
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let (Some(t), false) = (&self.tree, self.sel.is_empty()) {
                    let bytes: u64 = self.sel.iter().map(|&i| t.nodes[i as usize].size).sum();
                    ui.label(format!("{} selected · {}", self.sel.len(), human(bytes)));
                } else if self.tree.is_some() && matches!(self.tab, Tab::Explorer) {
                    ui.label(RichText::new(format!("{} items", commas(self.rows.len() as u64))).color(DIM));
                }
            });
        });
    }

    fn welcome(&mut self, ui: &mut Ui, acts: &mut Vec<Act>) {
        ui.add_space(30.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("Where did the space go?").size(30.0).strong());
            ui.label(RichText::new("Pick a drive to scan, or choose any folder from the top bar.").color(DIM));
        });
        ui.add_space(20.0);
        if let Some(m) = disk_grid(ui, &self.disks, "Scan this drive", &HashMap::new()) {
            acts.push(Act::Scan(m));
        }
    }

    /// Back / forward / up + breadcrumb, then the shared filter bar.
    fn header(&mut self, ui: &mut Ui, acts: &mut Vec<Act>) {
        let t = self.tree.as_ref().unwrap();
        ui.horizontal(|ui| {
            if ui.add_enabled(!self.back.is_empty(), Button::new("⏴")).on_hover_text("Back").clicked() {
                acts.push(Act::Back);
            }
            if ui.add_enabled(!self.fwd.is_empty(), Button::new("⏵")).on_hover_text("Forward").clicked() {
                acts.push(Act::Fwd);
            }
            let parent = t.nodes[self.cur as usize].parent;
            if ui.add_enabled(parent != u32::MAX, Button::new("⏶")).on_hover_text("Up one folder").clicked() {
                acts.push(Act::Go(parent));
            }
            let mut chain = vec![self.cur];
            while t.nodes[*chain.last().unwrap() as usize].parent != u32::MAX {
                chain.push(t.nodes[*chain.last().unwrap() as usize].parent);
            }
            for (i, &id) in chain.iter().rev().enumerate() {
                if i > 0 {
                    ui.label(RichText::new("›").color(DIM));
                }
                let name = if id == 0 { t.root.display().to_string() } else { t.nodes[id as usize].name.clone() };
                let text = if id == self.cur { RichText::new(name).strong() } else { RichText::new(name).color(DIM) };
                if ui.add(Button::new(text).frame(false)).clicked() {
                    acts.push(Act::Go(id));
                }
            }
            ui.label(RichText::new(human(t.nodes[self.cur as usize].size)).color(ACCENT2));
        });
        self.query_bar(ui);
    }

    fn query_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label("🔍");
            let hint = "Type a name, or a filter like   size > 100mb and (ext in (mp4, mkv) or name ~ setup) and not hidden";
            let w = (ui.available_width() - 330.0).max(120.0);
            if ui.add(TextEdit::singleline(&mut self.qtext).hint_text(hint).desired_width(w)).changed() {
                self.set_query();
            }
            if ui.add_enabled(!self.qtext.is_empty(), Button::new("✖")).on_hover_text("Clear filter").clicked() {
                self.qtext.clear();
                self.set_query();
            }
            if ui.selectable_label(self.deep, "All subfolders").on_hover_text("Search everything below this folder, not just its direct contents").clicked() {
                self.deep = !self.deep;
                self.dirty = true;
            }
            {
                if ui.selectable_label(self.show_builder, "➕ Condition").on_hover_text("Build a filter without typing").clicked() {
                    self.show_builder = !self.show_builder;
                }
                ui.label(RichText::new("❓").color(DIM)).on_hover_ui(|ui| {
                    ui.monospace(query::HELP);
                });
            }
        });
        if let Some(e) = &self.qerr {
            ui.label(RichText::new(format!("⚠ {e}")).color(WARN).small());
        }
        let mut set = None;
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Quick:").color(DIM).small());
            for (label, q) in CHIPS {
                if ui.add(Button::new(RichText::new(label).small()).rounding(12.0)).on_hover_text(q).clicked() {
                    set = Some(if self.qtext.trim().is_empty() { q.to_string() } else { format!("{} and {q}", self.qtext.trim()) });
                }
            }
        });
        if self.show_builder {
            Frame::none().fill(CARD).rounding(8.0).inner_margin(8.0).show(ui, |ui| {
                ui.horizontal(|ui| {
                    let b = &mut self.builder;
                    if !self.qtext.trim().is_empty() {
                        ComboBox::from_id_salt("join").width(90.0).selected_text(JOINS[b.0]).show_ui(ui, |ui| {
                            for (i, j) in JOINS.iter().enumerate() {
                                ui.selectable_value(&mut b.0, i, *j);
                            }
                        });
                    }
                    ComboBox::from_id_salt("field").width(100.0).selected_text(Field::ALL[b.1].0).show_ui(ui, |ui| {
                        for (i, f) in Field::ALL.iter().enumerate() {
                            ui.selectable_value(&mut b.1, i, f.0);
                        }
                    });
                    ComboBox::from_id_salt("op").width(120.0).selected_text(OPS[b.2].1).show_ui(ui, |ui| {
                        for (i, o) in OPS.iter().enumerate() {
                            ui.selectable_value(&mut b.2, i, o.1);
                        }
                    });
                    let hint = match Field::ALL[b.1].1 {
                        Field::Size | Field::Alloc => "100mb, 1.5gb…",
                        Field::Modified | Field::Created | Field::Accessed => "2024-01-31 or 30d, 6mo, 1y",
                        Field::Age => "days, e.g. 365",
                        Field::Kind => "file or dir",
                        Field::Category => "videos, archives…",
                        Field::Ext => "mp4",
                        f if f.is_num() => "number",
                        _ => "text, * wildcards ok",
                    };
                    ui.add(TextEdit::singleline(&mut b.3).hint_text(hint).desired_width(200.0));
                    if ui.add_enabled(!b.3.trim().is_empty(), primary("Add")).clicked() {
                        let v = if b.3.contains(char::is_whitespace) { format!("\"{}\"", b.3.trim()) } else { b.3.trim().to_string() };
                        let cond = format!("{} {} {v}", Field::ALL[b.1].0, OPS[b.2].0);
                        let old = self.qtext.trim();
                        set = Some(if old.is_empty() {
                            cond
                        } else {
                            let (join, not) = [("and", ""), ("or", ""), ("and", "not "), ("or", "not ")][b.0];
                            format!("({old}) {join} {not}{cond}")
                        });
                        b.3.clear();
                    }
                });
            });
        }
        if let Some(q) = set {
            self.qtext = q;
            self.set_query();
        }
    }

    // ───────────── explorer ─────────────

    fn cols(&self) -> Vec<Col> {
        let mut c = vec![Col::Name];
        if self.deep {
            c.push(Col::Folder);
        }
        c.extend([Col::Size, Col::Pct]);
        if self.filter.is_some() && !self.deep {
            c.push(Col::Matched);
        }
        c.extend([Col::Alloc, Col::Files, Col::Dirs, Col::Modified, Col::Created, Col::Accessed, Col::Cat, Col::Attr]);
        c
    }

    fn explorer(&mut self, ui: &mut Ui, acts: &mut Vec<Act>) {
        {
            let t = self.tree.as_ref().unwrap();
            SidePanel::left("folders").default_width(260.0).width_range(140.0..=800.0).frame(Frame::none().fill(PANEL).rounding(12.0).inner_margin(10.0)).show_inside(ui, |ui| {
                ui.label(RichText::new("FOLDERS").small().color(DIM));
                // The scroll area must fill the panel: a shrink-to-content one drags the panel's
                // remembered width back to the content width whenever the tree changes.
                ScrollArea::both().auto_shrink(false).show(ui, |ui| folder_tree(ui, t, 0, self.cur, acts));
            });
        }
        let gap = Margin { left: 16.0, ..Default::default() };
        CentralPanel::default().frame(Frame::none().inner_margin(gap)).show_inside(ui, |ui| self.explorer_table(ui, acts));
    }

    fn explorer_table(&mut self, ui: &mut Ui, acts: &mut Vec<Act>) {
        self.header(ui, acts);
        if self.sort.len() > 1 {
            ui.horizontal(|ui| {
                let keys: Vec<String> = self.sort.iter().map(|k| format!("{} {}", Field::ALL.iter().find(|f| f.1 == k.field).unwrap().0, if k.desc { "⏷" } else { "⏶" })).collect();
                ui.label(RichText::new(format!("Sorted by {}", keys.join(", then "))).small().color(DIM));
                if ui.small_button("reset").clicked() {
                    self.sort.truncate(1);
                    self.dirty = true;
                }
            });
        }
        ui.add_space(2.0);

        let t = self.tree.as_ref().unwrap();
        let cols = self.cols();
        let (shift, ctrl) = ui.input(|i| (i.modifiers.shift, i.modifiers.command));
        let folder_size = t.nodes[self.cur as usize].size.max(1);
        let (rows, sel, sort, fsize) = (&self.rows, &self.sel, &self.sort, &self.fsize);
        let (mut sort_click, mut click) = (None, None);
        let mut tb = TableBuilder::new(ui).striped(true).resizable(true).sense(Sense::click()).cell_layout(Layout::left_to_right(Align::Center)).min_scrolled_height(0.0);
        for c in &cols {
            tb = tb.column(c.column());
        }
        tb.header(28.0, |mut h| {
            for c in &cols {
                h.col(|ui| {
                    let Some(f) = c.field() else {
                        ui.label(RichText::new(c.title()).strong().color(DIM));
                        return;
                    };
                    let mut text = c.title().to_string();
                    if let Some(i) = sort.iter().position(|k| k.field == f) {
                        text += if sort[i].desc { " ⏷" } else { " ⏶" };
                        if sort.len() > 1 {
                            text += &(i + 1).to_string();
                        }
                    }
                    if ui.add(Button::new(RichText::new(text).strong().color(DIM)).frame(false)).on_hover_text("Click to sort · Shift+click to add as a secondary sort").clicked() {
                        sort_click = Some(f);
                    }
                });
            }
        })
        .body(|body| {
            body.rows(25.0, rows.len(), |mut row| {
                let i = row.index();
                let id = rows[i];
                let n = &t.nodes[id as usize];
                row.set_selected(sel.contains(&id));
                for c in &cols {
                    row.col(|ui| match c {
                        Col::Name => {
                            let (icon, color) = if n.is_dir { ("🗀", ACCENT2) } else { ("🗋", cat_color(category(&t.ext(id)))) };
                            ui.label(RichText::new(icon).color(color));
                            ui.label(if n.hidden { RichText::new(&n.name).color(DIM) } else { RichText::new(&n.name) });
                        }
                        Col::Folder => {
                            ui.label(RichText::new(t.path(n.parent).strip_prefix(&t.root).map_or(String::new(), |p| p.display().to_string())).color(DIM));
                        }
                        Col::Size => {
                            ui.label(human(n.size));
                        }
                        Col::Pct => {
                            let f = n.size as f32 / folder_size as f32;
                            meter(ui, f, if n.is_dir { ACCENT } else { ACCENT2.gamma_multiply(0.7) }, 15.0, Some(format!("{:.1}%", f * 100.0)));
                        }
                        Col::Matched => {
                            let m = if n.is_dir { fsize.get(id as usize).copied().unwrap_or(0) } else { n.size };
                            ui.label(RichText::new(human(m)).color(GOOD));
                        }
                        Col::Alloc => {
                            ui.label(human(n.alloc));
                        }
                        Col::Files => {
                            ui.label(if n.is_dir { commas(n.files) } else { String::new() });
                        }
                        Col::Dirs => {
                            ui.label(if n.is_dir { commas(n.dirs) } else { String::new() });
                        }
                        Col::Modified => {
                            ui.label(date(n.modified));
                        }
                        Col::Created => {
                            ui.label(date(n.created));
                        }
                        Col::Accessed => {
                            ui.label(date(n.accessed));
                        }
                        Col::Cat => {
                            if !n.is_dir {
                                let cat = category(&t.ext(id));
                                ui.label(RichText::new(cat).color(cat_color(cat)));
                            }
                        }
                        Col::Attr => {
                            ui.label(RichText::new(format!("{}{}", if n.hidden { "H" } else { "" }, if n.readonly { "R" } else { "" })).color(DIM));
                        }
                    });
                }
                let r = row.response();
                if r.double_clicked() {
                    acts.push(if n.is_dir { Act::Go(id) } else { Act::Open(id) });
                } else if r.clicked() {
                    click = Some(i);
                }
                r.context_menu(|ui| {
                    let mut item = |ui: &mut Ui, label: &str, a: Act| {
                        if ui.button(label).clicked() {
                            acts.push(a);
                            ui.close_menu();
                        }
                    };
                    if n.is_dir {
                        item(ui, "🗀  Open folder here", Act::Go(id));
                    }
                    item(ui, "⏵  Open with default app", Act::Open(id));
                    item(ui, "🔍  Show in file manager", Act::Reveal(id));
                    item(ui, "📋  Copy path", Act::Copy(id));
                    if self.deep {
                        item(ui, "⏶  Go to containing folder", Act::Show(id));
                    }
                    ui.separator();
                    let ids: Vec<u32> = if sel.contains(&id) { sel.iter().copied().collect() } else { vec![id] };
                    item(ui, &format!("🗑  Move {} to recycle bin", if ids.len() == 1 { "this".into() } else { format!("{} items", ids.len()) }), Act::Ask(Confirm::Trash(ids)));
                });
            });
        });

        if let Some(f) = sort_click {
            if shift {
                match self.sort.iter_mut().find(|k| k.field == f) {
                    Some(k) => k.desc = !k.desc,
                    None => self.sort.push(SortKey { field: f, desc: f.is_num() }),
                }
            } else {
                let desc = match self.sort.as_slice() {
                    [k] if k.field == f => !k.desc,
                    _ => f.is_num(), // numbers start biggest-first, text A→Z
                };
                self.sort = vec![SortKey { field: f, desc }];
            }
            self.dirty = true;
        }
        if let Some(i) = click {
            let id = self.rows[i];
            match (shift, ctrl, self.anchor) {
                (true, _, Some(a)) => self.sel.extend(self.rows[a.min(i)..=a.max(i)].iter().copied()),
                (_, true, _) => {
                    if !self.sel.remove(&id) {
                        self.sel.insert(id);
                    }
                }
                _ => {
                    self.sel.clear();
                    self.sel.insert(id);
                }
            }
            self.anchor = Some(i);
        }
    }

    // ───────────── treemap ─────────────

    fn treemap(&mut self, ui: &mut Ui, acts: &mut Vec<Act>) {
        self.header(ui, acts);
        let t = self.tree.as_ref().unwrap();
        let (resp, painter) = ui.allocate_painter(ui.available_size(), Sense::click());
        let rect = resp.rect;
        if self.tiles.0 != self.gen || self.tiles.1 != rect {
            let fsize = &self.fsize;
            let w = |id: u32| if fsize.is_empty() { t.nodes[id as usize].size } else { fsize[id as usize] };
            let mut tiles = vec![];
            tm_layout(t, &w, self.cur, rect, 0, &mut tiles);
            self.tiles = (self.gen, rect, tiles);
        }
        let tiles = &self.tiles.2;
        if tiles.is_empty() {
            painter.text(rect.center(), Align2::CENTER_CENTER, "Nothing to show here", FontId::proportional(16.0), DIM);
            return;
        }
        let hover = resp.hover_pos().and_then(|p| tiles.iter().rposition(|t| t.rect.contains(p)));
        for (i, tile) in tiles.iter().enumerate() {
            let n = &t.nodes[tile.id as usize];
            let p = painter.with_clip_rect(tile.rect);
            p.rect_filled(tile.rect, 4.0, if tile.leaf && hover == Some(i) { tile.color.gamma_multiply(1.25) } else { tile.color });
            if !tile.leaf {
                p.text(tile.rect.min + vec2(5.0, 2.0), Align2::LEFT_TOP, format!("{}  {}", n.name, human(n.size)), FontId::proportional(12.0), TEXT);
            } else if tile.rect.width() > 60.0 && tile.rect.height() > 32.0 {
                p.text(tile.rect.min + vec2(5.0, 3.0), Align2::LEFT_TOP, &n.name, FontId::proportional(12.0), Color32::from_rgb(12, 14, 20));
                p.text(tile.rect.min + vec2(5.0, 17.0), Align2::LEFT_TOP, human(n.size), FontId::proportional(11.0), Color32::from_rgb(12, 14, 20));
            }
            if hover == Some(i) {
                painter.rect_stroke(tile.rect, 4.0, Stroke::new(2.0_f32, Color32::WHITE));
            }
        }
        let hovered = hover.map(|i| tiles[i].id);
        if let Some(id) = hovered {
            let n = &t.nodes[id as usize];
            resp.clone().on_hover_ui_at_pointer(|ui| {
                ui.label(RichText::new(t.path(id).display().to_string()).strong());
                ui.label(format!("{}{}", human(n.size), if n.is_dir { format!(" · {} files", commas(n.files)) } else { format!(" · {} · {}", category(&t.ext(id)), date(n.modified)) }));
                ui.label(RichText::new("Click to zoom in · right-click for actions").small().color(DIM));
            });
            if resp.clicked() {
                let n = &t.nodes[id as usize];
                acts.push(Act::Go(if n.is_dir { id } else { n.parent }));
            }
            if resp.secondary_clicked() {
                self.tm_ctx = Some(id);
            }
        }
        if let Some(id) = self.tm_ctx {
            resp.context_menu(|ui| {
                ui.label(RichText::new(&t.nodes[id as usize].name).strong());
                for (label, a) in [("⏶  Show in Explorer", Act::Show(id)), ("⏵  Open", Act::Open(id)), ("🔍  Show in file manager", Act::Reveal(id)), ("📋  Copy path", Act::Copy(id)), ("🗑  Move to recycle bin", Act::Ask(Confirm::Trash(vec![id])))] {
                    if ui.button(label).clicked() {
                        acts.push(a);
                        ui.close_menu();
                    }
                }
            });
        }
    }

    // ───────────── charts ─────────────

    fn charts(&mut self, ui: &mut Ui, acts: &mut Vec<Act>) {
        self.header(ui, acts);
        if self.stats.is_none() {
            self.stats = Some(api::stats(self.tree.as_ref().unwrap(), self.cur, self.filter.as_ref()));
        }
        let (t, st) = (self.tree.as_ref().unwrap(), self.stats.as_ref().unwrap());
        let files = |n: u64| format!("{} files", commas(n));
        let ramp = |i: usize, n: usize| ACCENT2.lerp_to_gamma(ACCENT, i as f32 / n.max(2).saturating_sub(1) as f32);
        ui.label(RichText::new("Click any slice or bar to list exactly those files.").small().color(DIM));
        ScrollArea::vertical().show(ui, |ui| {
            ui.columns(2, |c| {
                card(&mut c[0], "By category", |ui| {
                    let items: Vec<_> = st.cats.iter().map(|b| (b.label.clone(), b.bytes, files(b.count), cat_color(&b.label))).collect();
                    ui.horizontal(|ui| {
                        if let Some(i) = donut(ui, &items, 210.0) {
                            acts.push(Act::Query(st.cats[i].query.clone()));
                        }
                        ui.vertical(|ui| {
                            if let Some(i) = hbars(ui, &items) {
                                acts.push(Act::Query(st.cats[i].query.clone()));
                            }
                        });
                    });
                });
                card(&mut c[1], "Top extensions", |ui| {
                    let items: Vec<_> = st.exts.iter().take(9).map(|b| (format!(".{}", b.label), b.bytes, files(b.count), cat_color(category(&b.label)))).collect();
                    if let Some(i) = hbars(ui, &items) {
                        acts.push(Act::Query(st.exts[i].query.clone()));
                    }
                });
                c[0].add_space(8.0);
                c[1].add_space(8.0);
                card(&mut c[0], "File age (last modified)", |ui| {
                    let items: Vec<_> = st.ages.iter().enumerate().map(|(i, b)| (b.label.clone(), b.bytes, files(b.count), ramp(i, 6))).collect();
                    if let Some(i) = hbars(ui, &items) {
                        acts.push(Act::Query(st.ages[i].query.clone()));
                    }
                });
                card(&mut c[1], "File size", |ui| {
                    let items: Vec<_> = st.sizes.iter().enumerate().map(|(i, b)| (b.label.clone(), b.bytes, files(b.count), ramp(i, 6))).collect();
                    if let Some(i) = hbars(ui, &items) {
                        acts.push(Act::Query(st.sizes[i].query.clone()));
                    }
                });
            });
            ui.add_space(8.0);
            card(ui, "Biggest subfolders", |ui| {
                let w = |id: u32| if self.fsize.is_empty() { t.nodes[id as usize].size } else { self.fsize[id as usize] };
                let mut dirs: Vec<u32> = t.nodes[self.cur as usize].children.iter().copied().filter(|&c| t.nodes[c as usize].is_dir && w(c) > 0).collect();
                dirs.sort_by_key(|&c| std::cmp::Reverse(w(c)));
                dirs.truncate(12);
                let items: Vec<_> = dirs.iter().enumerate().map(|(i, &c)| (t.nodes[c as usize].name.clone(), w(c), files(t.nodes[c as usize].files), ramp(i, dirs.len()))).collect();
                if let Some(i) = hbars(ui, &items) {
                    acts.push(Act::Go(dirs[i]));
                }
            });
        });
    }

    // ───────────── duplicates ─────────────

    fn dupes(&mut self, ui: &mut Ui, acts: &mut Vec<Act>) {
        self.header(ui, acts);
        let t = self.tree.as_ref().unwrap();
        let mut start = false;
        let mut auto: Option<u8> = None;
        ui.horizontal(|ui| {
            if let Some((done, cancel)) = &self.dup_job {
                ui.spinner();
                ui.label(format!("Comparing contents… {} files hashed", commas(done.load(Relaxed))));
                if ui.button("Cancel").clicked() {
                    cancel.store(true, Relaxed);
                }
            } else {
                start = ui.add(primary("🔍  Find duplicates in this folder")).on_hover_text("Compares by size, then content hash. Respects the filter above.").clicked();
                ui.label("ignore files smaller than");
                ui.add(TextEdit::singleline(&mut self.dup_min).desired_width(60.0));
            }
        });
        if !self.dups.is_empty() {
            let wasted: u64 = self.dups.iter().map(|g| g.size * (g.files.len() as u64 - 1)).sum();
            let picked: u64 = self.dup_sel.iter().map(|&i| t.nodes[i as usize].size).sum();
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{} groups · {} reclaimable", commas(self.dups.len() as u64), human(wasted))).strong().color(GOOD));
                ui.separator();
                ui.label("Select all copies except the");
                for (i, l) in ["newest", "oldest", "shortest path"].iter().enumerate() {
                    if ui.button(*l).clicked() {
                        auto = Some(i as u8);
                    }
                }
                if ui.button("none").clicked() {
                    self.dup_sel.clear();
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add_enabled(!self.dup_sel.is_empty(), primary(&format!("🗑  Recycle {} selected ({})", self.dup_sel.len(), human(picked)))).clicked() {
                        acts.push(Act::Ask(Confirm::Trash(self.dup_sel.iter().copied().collect())));
                    }
                });
            });
            ScrollArea::vertical().show(ui, |ui| {
                // ponytail: renders the 300 most wasteful groups; virtualize if anyone needs to page through more
                for g in self.dups.iter().take(300) {
                    card(ui, "", |ui| {
                        ui.label(RichText::new(format!("{} copies × {}  —  {} wasted", g.files.len(), human(g.size), human(g.size * (g.files.len() as u64 - 1)))).strong());
                        for &f in &g.files {
                            ui.horizontal(|ui| {
                                let mut on = self.dup_sel.contains(&f);
                                if ui.checkbox(&mut on, "").changed() {
                                    if on { self.dup_sel.insert(f) } else { self.dup_sel.remove(&f) };
                                }
                                let r = ui.add(Label::new(t.path(f).display().to_string()).sense(Sense::click())).on_hover_text("Click to show in file manager");
                                if r.clicked() {
                                    acts.push(Act::Reveal(f));
                                }
                                ui.label(RichText::new(date(t.nodes[f as usize].modified)).color(DIM).small());
                            });
                        }
                    });
                    ui.add_space(4.0);
                }
                if self.dups.len() > 300 {
                    ui.label(RichText::new(format!("…and {} smaller groups (see Reports for the full list)", self.dups.len() - 300)).color(DIM));
                }
            });
        } else if self.dup_job.is_none() {
            ui.add_space(20.0);
            ui.label(RichText::new("No duplicates listed yet. Navigate to the folder you care about, optionally filter, and press the button.").color(DIM));
        }
        if let Some(mode) = auto {
            self.dup_sel.clear();
            for g in &self.dups {
                let keep = match mode {
                    0 => g.files.iter().max_by_key(|&&f| t.nodes[f as usize].modified),
                    1 => g.files.iter().min_by_key(|&&f| t.nodes[f as usize].modified),
                    _ => g.files.iter().min_by_key(|&&f| t.path(f).as_os_str().len()),
                };
                self.dup_sel.extend(g.files.iter().filter(|f| Some(*f) != keep));
            }
        }
        if start {
            match query::parse_size(&self.dup_min) {
                Err(e) => self.say(e),
                Ok(min) => {
                    let f = self.filter.as_ref();
                    let files: Vec<_> = t
                        .descendants(self.cur)
                        .filter(|&i| !t.nodes[i as usize].is_dir && t.nodes[i as usize].size >= (min as u64).max(1) && f.map_or(true, |f| f.eval(t, i)))
                        .map(|i| (i, t.path(i), t.nodes[i as usize].size))
                        .collect();
                    let (done, cancel) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicBool::new(false)));
                    self.dup_job = Some((done.clone(), cancel.clone()));
                    self.spawn(ui.ctx(), move || Bg::Dups(ops::dupes(files, &done, &cancel)));
                }
            }
        }
    }

    // ───────────── cleanup ─────────────

    fn cleanup(&mut self, ui: &mut Ui, acts: &mut Vec<Act>) {
        if !self.temps_measured {
            self.measure_temps(ui.ctx());
        }
        card(ui, "System temp locations", |ui| {
            ui.label(RichText::new("Safe to clear. Anything used in the last 24 hours or currently locked is left alone.").small().color(DIM));
            let mut total = 0;
            for (p, on, size) in &mut self.temps {
                ui.horizontal(|ui| {
                    ui.checkbox(on, p.display().to_string());
                    match size {
                        Some((b, f)) => {
                            total += if *on { *b } else { 0 };
                            ui.label(RichText::new(format!("{} · {} files", human(*b), commas(*f))).color(ACCENT2));
                        }
                        None => {
                            ui.spinner();
                        }
                    }
                });
            }
            ui.horizontal(|ui| {
                if ui.add_enabled(self.busy.is_none() && total > 0, primary(&format!("🗑  Clear selected (up to {})", human(total)))).clicked() {
                    acts.push(Act::Ask(Confirm::ClearTemp(self.temps.iter().filter(|t| t.1).map(|t| t.0.clone()).collect())));
                }
                if ui.button("⟲  Re-measure").clicked() {
                    self.temps_measured = false;
                }
            });
        });
        ui.add_space(8.0);
        let Some(t) = &self.tree else {
            card(ui, "Caches & junk", |ui| ui.label(RichText::new("Scan a drive or folder to detect cache folders, build outputs, dumps and leftovers.").color(DIM)));
            return;
        };
        let total: u64 = self.junk.iter().map(|j| t.nodes[j.id as usize].size).sum();
        let picked: u64 = self.junk_sel.iter().map(|&i| t.nodes[i as usize].size).sum();
        card(ui, &format!("Caches & junk in this scan — {} items, {}", commas(self.junk.len() as u64), human(total)), |ui| {
            ui.label(RichText::new("Regenerable stuff: app caches, build outputs, dependency folders, crash dumps, leftovers. Goes to the recycle bin, so it is recoverable.").small().color(DIM));
            ui.horizontal(|ui| {
                if ui.button("Select all").clicked() {
                    self.junk_sel = self.junk.iter().map(|j| j.id).collect();
                }
                if ui.button("Select none").clicked() {
                    self.junk_sel.clear();
                }
                if ui.add_enabled(!self.junk_sel.is_empty(), primary(&format!("🗑  Recycle {} selected ({})", self.junk_sel.len(), human(picked)))).clicked() {
                    acts.push(Act::Ask(Confirm::Trash(self.junk_sel.iter().copied().collect())));
                }
            });
            ScrollArea::vertical().auto_shrink(false).show_rows(ui, 22.0, self.junk.len(), |ui, range| {
                for j in &self.junk[range] {
                    ui.horizontal(|ui| {
                        let mut on = self.junk_sel.contains(&j.id);
                        if ui.checkbox(&mut on, "").changed() {
                            if on { self.junk_sel.insert(j.id) } else { self.junk_sel.remove(&j.id) };
                        }
                        ui.add_sized([70.0, 18.0], Label::new(RichText::new(human(t.nodes[j.id as usize].size)).color(ACCENT2)));
                        if ui.add(Label::new(t.path(j.id).display().to_string()).truncate().sense(Sense::click())).on_hover_text(format!("{} — click to show in Explorer", j.why)).clicked() {
                            acts.push(Act::Show(j.id));
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.label(RichText::new(j.why).small().color(DIM)));
                    });
                }
            });
        });
    }

    // ───────────── advisor ─────────────

    fn advisor(&mut self, ui: &mut Ui, acts: &mut Vec<Act>) {
        ScrollArea::vertical().show(ui, |ui| {
            ui.label(RichText::new("Disks").strong().size(16.0));
            if let Some(m) = disk_grid(ui, &self.disks, "⏱  Test speed", &self.bench) {
                self.bench.insert(m.clone(), "testing… (writes a 256 MB temp file)".into());
                self.spawn(ui.ctx(), move || {
                    // the drive root is often not writable; the user's temp dir is when it's on the same disk
                    let tmp = std::env::temp_dir();
                    let dir = if tmp.starts_with(&m) { tmp } else { m.clone() };
                    let s = ops::bench(&dir).map_or_else(|e| format!("failed: {e}"), |(w, r)| format!("write {w:.0} MB/s · read {r:.0} MB/s (cached)"));
                    Bg::Bench(m, s)
                });
            }
            ui.add_space(10.0);
            ui.label(RichText::new("Suggestions").strong().size(16.0));
            if self.tree.is_none() {
                ui.label(RichText::new("Scan a drive to also get \"move this off your SSD\" suggestions.").small().color(DIM));
            }
            for tip in self.tips.iter().chain(&self.sys_tips) {
                card(ui, "", |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("💡").color(WARN));
                        ui.label(RichText::new(&tip.title).strong());
                        if tip.bytes > 0 {
                            badge(ui, &format!("up to {}", human(tip.bytes)), GOOD);
                        }
                    });
                    ui.label(RichText::new(&tip.detail).color(DIM));
                    if let Some(dir) = tip.dir {
                        ui.horizontal(|ui| {
                            if ui.button("Show in Explorer").clicked() {
                                acts.push(Act::Show(dir));
                            }
                            if let Some(dest) = &tip.dest {
                                if ui.add_enabled(self.busy.is_none(), primary(&format!("Move them to {}", dest.display()))).clicked() {
                                    acts.push(Act::Ask(Confirm::Offload(dir, dest.clone())));
                                }
                            }
                        });
                    }
                });
                ui.add_space(4.0);
            }
        });
    }

    // ───────────── organize ─────────────

    fn organize(&mut self, ui: &mut Ui, acts: &mut Vec<Act>) {
        let mut pick = None;
        card(ui, "Organize a folder", |ui| {
            ui.label(RichText::new("Sorts the loose files in one folder into Videos, Images, Documents, Archives, Installers… subfolders by type. Subfolders and unknown file types are left alone. You get a preview first, and an undo after.").color(DIM));
            ui.horizontal(|ui| {
                if ui.button("🗀  Choose folder…").clicked() {
                    pick = rfd::FileDialog::new().pick_folder();
                }
                if let Some(t) = &self.tree {
                    if ui.button("Use current Explorer folder").clicked() {
                        pick = Some(t.path(self.cur));
                    }
                }
                if let Some(d) = [dirs_download()].into_iter().flatten().next() {
                    if ui.button("Downloads").clicked() {
                        pick = Some(d);
                    }
                }
                if let Some(d) = &self.org_dir {
                    ui.label(RichText::new(d.display().to_string()).strong().color(ACCENT2));
                }
            });
        });
        if let Some(p) = pick {
            self.org_plan = ops::organize_plan(&p);
            self.org_dir = Some(p);
        }
        let Some(dir) = &self.org_dir else { return };
        ui.add_space(8.0);
        let mut undo = false;
        card(ui, &format!("Preview — {} files will move", self.org_plan.len()), |ui| {
            let mut counts: Vec<(String, usize)> = vec![];
            for m in &self.org_plan {
                let cat = m.to.parent().and_then(|p| p.file_name()).map_or(String::new(), |c| c.to_string_lossy().into_owned());
                match counts.iter_mut().find(|c| c.0 == cat) {
                    Some(c) => c.1 += 1,
                    None => counts.push((cat, 1)),
                }
            }
            ui.horizontal_wrapped(|ui| {
                for (cat, n) in &counts {
                    badge(ui, &format!("{cat}  {n}"), cat_color(cat));
                }
            });
            ui.horizontal(|ui| {
                if ui.add_enabled(!self.org_plan.is_empty(), primary("✔  Organize now")).clicked() {
                    acts.push(Act::Ask(Confirm::Organize));
                }
                undo = ui.add_enabled(!self.org_done.is_empty(), Button::new(format!("⟲  Undo last ({} files)", self.org_done.len()))).clicked();
            });
            ScrollArea::vertical().auto_shrink(false).show_rows(ui, 20.0, self.org_plan.len(), |ui, range| {
                for m in &self.org_plan[range] {
                    ui.horizontal(|ui| {
                        ui.label(m.from.file_name().unwrap_or_default().to_string_lossy());
                        ui.label(RichText::new("⏵").color(DIM));
                        ui.label(RichText::new(m.to.strip_prefix(dir).unwrap_or(&m.to).display().to_string()).color(ACCENT2));
                    });
                }
            });
        });
        if undo {
            let (ok, errs) = ops::apply_moves(&self.org_done, true);
            self.org_done.clear();
            self.org_plan = ops::organize_plan(self.org_dir.as_ref().unwrap());
            self.say(format!("Moved {ok} files back{}", if errs.is_empty() { String::new() } else { format!(" ({} failed)", errs.len()) }));
            if let Some(t) = &self.tree {
                acts.push(Act::Scan(t.root.clone()));
            }
        }
    }

    // ───────────── reports ─────────────

    fn reports(&mut self, ui: &mut Ui) {
        let fmt = api::FORMATS[self.rep_fmt];
        SidePanel::left("rep").exact_width(350.0).resizable(false).frame(Frame::none().fill(PANEL).rounding(12.0).inner_margin(14.0)).show_inside(ui, |ui| {
            let t = self.tree.as_ref().unwrap();
            ui.label(RichText::new("Report").strong().size(16.0));
            ui.label(RichText::new(format!("Scope: {}{}", t.path(self.cur).display(), if self.filter.is_some() { "\nFilter: active (from the filter bar)" } else { "" })).small().color(DIM));
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("Sections");
                if ui.small_button("all").clicked() {
                    self.rep_on.fill(true);
                }
                if ui.small_button("none").clicked() {
                    self.rep_on.fill(false);
                }
            });
            for (i, s) in api::SECTIONS.iter().enumerate() {
                let label = if s.0 == "duplicates" { "Duplicates (from the Duplicates tab)" } else { s.1 };
                ui.checkbox(&mut self.rep_on[i], label);
            }
            ui.add_space(4.0);
            ui.add(Slider::new(&mut self.rep_top, 5..=1000).logarithmic(true).text("rows per section"));
            ui.horizontal(|ui| {
                ui.label("Format");
                for (i, f) in api::FORMATS.iter().enumerate() {
                    ui.selectable_value(&mut self.rep_fmt, i, f.to_uppercase());
                }
            });
            ui.add_space(6.0);
            let (gen, save) = ui.horizontal(|ui| (ui.button("Preview").clicked(), ui.add(primary("💾  Save as…")).clicked())).inner;
            if gen || save {
                let which: Vec<String> = api::SECTIONS.iter().zip(&self.rep_on).filter(|x| *x.1).map(|x| x.0 .0.to_string()).collect();
                let secs = if which.is_empty() { vec![] } else { api::build(t, self.cur, self.filter.as_ref(), &which, self.rep_top, Some(&self.dups)) };
                let text = api::render(&secs, fmt).unwrap_or_default();
                if save {
                    if let Some(p) = rfd::FileDialog::new().set_file_name(format!("diskvibe-report.{fmt}")).save_file() {
                        let msg = match std::fs::write(&p, &text) {
                            Ok(()) => format!("Saved {}", p.display()),
                            Err(e) => format!("Could not save: {e}"),
                        };
                        self.toast = Some((msg, Instant::now()));
                    }
                }
                self.rep_text = text;
            }
        });
        if self.rep_text.is_empty() {
            ui.centered_and_justified(|ui| ui.label(RichText::new("Pick sections and a format, then Preview or Save.").color(DIM)));
            return;
        }
        ScrollArea::both().auto_shrink(false).show(ui, |ui| {
            // ponytail: preview shows the first 60k chars — laying out megabytes of text every frame is slow; Save writes everything
            let end = self.rep_text.char_indices().nth(60_000).map_or(self.rep_text.len(), |c| c.0);
            ui.add(Label::new(RichText::new(&self.rep_text[..end]).monospace()).extend());
        });
    }

    // ───────────── actions ─────────────

    fn confirm_ui(&mut self, ctx: &Context, acts: &mut Vec<Act>) {
        let Some(c) = &self.confirm else { return };
        let t = self.tree.as_ref();
        let (title, body, yes) = match c {
            Confirm::Trash(ids) => {
                let t = t.unwrap();
                let bytes: u64 = ids.iter().map(|&i| t.nodes[i as usize].size).sum();
                let mut list: Vec<String> = ids.iter().take(6).map(|&i| t.path(i).display().to_string()).collect();
                if ids.len() > 6 {
                    list.push(format!("…and {} more", ids.len() - 6));
                }
                ("Move to recycle bin?", format!("{} item(s), {} in total:\n\n{}\n\nYou can restore them from the recycle bin.", ids.len(), human(bytes), list.join("\n")), "Move to recycle bin")
            }
            Confirm::ClearTemp(paths) => (
                "Clear temp locations?",
                format!("Permanently delete the contents of:\n\n{}\n\nEntries used in the last 24 hours and files in use are kept. This cannot be undone.", paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n")),
                "Delete permanently",
            ),
            Confirm::Offload(dir, dest) => {
                let t = t.unwrap();
                let files = ops::cold_files(t, *dir);
                let bytes: u64 = files.iter().map(|&i| t.nodes[i as usize].size).sum();
                ("Move files to another disk?", format!("Move {} files ({}) from\n{}\nto\n{}\n\nEach file is copied, verified, then removed from the source. Existing files are never overwritten.", files.len(), human(bytes), t.path(*dir).display(), dest.display()), "Move files")
            }
            Confirm::Organize => (
                "Organize this folder?",
                format!("Move {} files into type subfolders inside\n{}\n\nYou can undo this afterwards.", self.org_plan.len(), self.org_dir.as_ref().map_or(String::new(), |d| d.display().to_string())),
                "Organize",
            ),
        };
        let mut answer = None;
        Window::new(title).collapsible(false).resizable(false).anchor(Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
            ui.set_max_width(560.0);
            ui.label(body);
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.add(primary(yes)).clicked() {
                    answer = Some(true);
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                    answer = Some(false);
                }
            });
        });
        if let Some(yes) = answer {
            let c = self.confirm.take().unwrap();
            if yes {
                acts.push(Act::Do(c));
            }
        }
    }

    fn apply(&mut self, a: Act, ctx: &Context) {
        let path = |s: &Self, id: u32| s.tree.as_ref().map(|t| t.path(id));
        match a {
            Act::Scan(p) => self.start_scan(p, ctx),
            Act::Go(id) if id != self.cur => {
                self.back.push(self.cur);
                self.fwd.clear();
                self.nav(id);
            }
            Act::Go(_) => {}
            Act::Back => {
                if let Some(p) = self.back.pop() {
                    self.fwd.push(self.cur);
                    self.nav(p);
                }
            }
            Act::Fwd => {
                if let Some(p) = self.fwd.pop() {
                    self.back.push(self.cur);
                    self.nav(p);
                }
            }
            Act::Open(id) => {
                if let Some(Err(e)) = path(self, id).map(open::that) {
                    self.say(format!("Could not open: {e}"));
                }
            }
            Act::Reveal(id) => {
                path(self, id).map(|p| reveal(&p));
            }
            Act::Copy(id) => {
                if let Some(p) = path(self, id) {
                    ctx.copy_text(p.display().to_string());
                    self.say("Path copied");
                }
            }
            Act::Query(q) => {
                self.qtext = q;
                self.set_query();
                (self.deep, self.tab) = (true, Tab::Explorer);
            }
            Act::Show(id) => {
                // jump to the item inside its folder, unfiltered, with it selected
                let Some(t) = &self.tree else { return };
                let n = &t.nodes[id as usize];
                let dir = if n.parent == u32::MAX { id } else { n.parent };
                self.qtext.clear();
                self.set_query();
                (self.deep, self.tab) = (false, Tab::Explorer);
                self.apply(Act::Go(dir), ctx);
                self.sel.insert(id);
            }
            Act::Ask(c) => self.confirm = Some(c),
            Act::Do(Confirm::Trash(ids)) => {
                // ponytail: runs on the UI thread — trashing thousands of entries freezes the window for a moment
                let Some(t) = &mut self.tree else { return };
                let (mut ok, mut freed, mut errs) = (0, 0, vec![]);
                for id in ids {
                    if t.nodes[id as usize].dead {
                        continue; // already gone with a selected parent
                    }
                    match trash::delete(t.path(id)) {
                        Ok(()) => {
                            freed += t.nodes[id as usize].size;
                            ok += 1;
                            t.remove(id);
                        }
                        Err(e) => errs.push(e.to_string()),
                    }
                }
                self.modified = true;
                self.say(format!("Recycled {ok} item(s), {} freed{}", human(freed), errs.first().map_or(String::new(), |e| format!(" · {} failed: {e}", errs.len()))));
                self.tree_changed();
            }
            Act::Do(Confirm::ClearTemp(paths)) => {
                self.busy = Some("Clearing temp files…".into());
                self.spawn(ctx, move || {
                    let (mut ok, mut kept) = (0, 0);
                    for p in &paths {
                        let (o, k) = ops::clear_dir(p);
                        (ok, kept) = (ok + o, kept + k);
                    }
                    Bg::TempCleared(format!("Temp cleared: {ok} entries deleted, {kept} kept (recent or in use)"))
                });
            }
            Act::Do(Confirm::Offload(dir, dest)) => {
                let Some(t) = &self.tree else { return };
                let files: Vec<_> = ops::cold_files(t, dir).into_iter().map(|i| (i, t.path(i))).collect();
                self.busy = Some(format!("Moving {} files to {}…", files.len(), dest.display()));
                self.spawn(ctx, move || {
                    let (moved, errs) = ops::offload(files, &dest);
                    let msg = format!("Moved {} files to {}{}", moved.len(), dest.display(), errs.first().map_or(String::new(), |e| format!(" · {} failed: {e}", errs.len())));
                    Bg::Offloaded(moved, msg)
                });
            }
            Act::Do(Confirm::Organize) => {
                let (ok, errs) = ops::apply_moves(&self.org_plan, false);
                self.org_done = std::mem::take(&mut self.org_plan);
                self.say(format!("Organized {ok} files{}", errs.first().map_or(String::new(), |e| format!(" · {} failed: {e}", errs.len()))));
                if let Some(t) = &self.tree {
                    self.start_scan(t.root.clone(), ctx); // sizes are unchanged but the listing isn't
                }
            }
        }
    }

    fn nav(&mut self, id: u32) {
        self.cur = id;
        self.sel.clear();
        self.anchor = None;
        self.dirty = true;
    }
}

fn dirs_download() -> Option<PathBuf> {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })?;
    Some(PathBuf::from(home).join("Downloads")).filter(|p| p.is_dir())
}

impl eframe::App for App {
    fn on_exit(&mut self, _: Option<&eframe::glow::Context>) {
        // keep the saved scan in step with what was recycled or moved this session
        if let (Some(t), true) = (&self.tree, self.modified) {
            let _ = t.save();
        }
    }

    fn update(&mut self, ctx: &Context, _: &mut eframe::Frame) {
        self.poll(ctx);
        if self.dirty {
            self.refresh();
        }
        let mut acts = vec![];
        TopBottomPanel::top("top").frame(Frame::none().fill(PANEL).inner_margin(Margin::symmetric(16.0, 10.0))).show(ctx, |ui| self.top_bar(ui, &mut acts));
        TopBottomPanel::bottom("status").frame(Frame::none().fill(PANEL).inner_margin(Margin::symmetric(16.0, 6.0))).show(ctx, |ui| self.status(ui));
        SidePanel::left("nav").exact_width(168.0).resizable(false).frame(Frame::none().fill(PANEL).inner_margin(10.0)).show(ctx, |ui| {
            ui.add_space(4.0);
            for (tab, icon, name) in TABS {
                let on = self.tab == tab;
                let text = RichText::new(format!("{icon}   {name}")).size(15.0).color(if on { Color32::WHITE } else { DIM });
                let b = Button::new(text).fill(if on { ACCENT.linear_multiply(0.55) } else { Color32::TRANSPARENT }).min_size(vec2(148.0, 36.0));
                if ui.add(b).clicked() {
                    self.tab = tab;
                }
            }
        });
        CentralPanel::default().frame(Frame::none().fill(BG).inner_margin(14.0)).show(ctx, |ui| {
            let needs_tree = !matches!(self.tab, Tab::Cleanup | Tab::Advisor | Tab::Organize);
            if needs_tree && self.tree.is_none() {
                return self.welcome(ui, &mut acts);
            }
            match self.tab {
                Tab::Explorer => self.explorer(ui, &mut acts),
                Tab::Treemap => self.treemap(ui, &mut acts),
                Tab::Charts => self.charts(ui, &mut acts),
                Tab::Dupes => self.dupes(ui, &mut acts),
                Tab::Cleanup => self.cleanup(ui, &mut acts),
                Tab::Advisor => self.advisor(ui, &mut acts),
                Tab::Organize => self.organize(ui, &mut acts),
                Tab::Reports => self.reports(ui),
            }
        });
        self.confirm_ui(ctx, &mut acts);
        // keyboard: Delete recycles the selection, Backspace goes up, Alt+arrows walk history
        if self.confirm.is_none() && self.tab == Tab::Explorer && ctx.memory(|m| m.focused().is_none()) {
            ctx.input(|i| {
                if i.key_pressed(Key::Delete) && !self.sel.is_empty() {
                    acts.push(Act::Ask(Confirm::Trash(self.sel.iter().copied().collect())));
                }
                if i.key_pressed(Key::Backspace) {
                    if let Some(t) = &self.tree {
                        acts.push(Act::Go(t.nodes[self.cur as usize].parent.min(self.cur)));
                    }
                }
                if i.modifiers.alt && i.key_pressed(Key::ArrowLeft) {
                    acts.push(Act::Back);
                }
                if i.modifiers.alt && i.key_pressed(Key::ArrowRight) {
                    acts.push(Act::Fwd);
                }
            });
        }
        for a in acts {
            self.apply(a, ctx);
        }
    }
}
