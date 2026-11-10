//! The one filter + sort language used by the GUI, CLI and MCP server.
use crate::scan::{category, now, Tree};
use regex::Regex;
use std::cmp::Ordering;

pub const HELP: &str = "\
FILTER SYNTAX
  Combine conditions with  and / or / not  and parentheses. A bare word matches the name.
  Fields   name path ext type category size alloc files dirs depth modified created accessed age
  Flags    file  dir  hidden  readonly  empty
  Compare  =  !=  >  >=  <  <=     ~ contains    !~ not contains    =~ regex    in (a, b, c)
  Values   sizes 500kb 100mb 1.5gb · dates 2024-01-31 or 30d / 2w / 6mo / 1y (\"that long ago\")
           age is days since modified · * and ? wildcards work with = and bare words
  Examples size > 100mb and (ext in (mp4, mkv) or name ~ setup) and not hidden
           category = videos and age > 365        (old videos)
           modified < 1y and size >= 1gb          (not touched for a year, big)
           dir and empty                          (empty folders)
           *.log or path =~ \"cache|temp\"
SORT SYNTAX
  Comma separated fields, each optionally followed by asc/desc:   size desc, name";

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Field {
    Name,
    Path,
    Ext,
    Kind,
    Category,
    Size,
    Alloc,
    Files,
    Dirs,
    Depth,
    Modified,
    Created,
    Accessed,
    Age,
}
use Field::*;

impl Field {
    pub const ALL: [(&'static str, Field); 14] = [
        ("name", Name),
        ("path", Path),
        ("ext", Ext),
        ("type", Kind),
        ("category", Category),
        ("size", Size),
        ("alloc", Alloc),
        ("files", Files),
        ("dirs", Dirs),
        ("depth", Depth),
        ("modified", Modified),
        ("created", Created),
        ("accessed", Accessed),
        ("age", Age),
    ];
    pub fn parse(s: &str) -> Option<Field> {
        Some(match s.to_lowercase().as_str() {
            "extension" => Ext,
            "kind" => Kind,
            "cat" => Category,
            "allocated" => Alloc,
            "folders" => Dirs,
            "mtime" => Modified,
            "ctime" => Created,
            "atime" => Accessed,
            s => Self::ALL.iter().find(|f| f.0 == s)?.1,
        })
    }
    pub fn is_num(self) -> bool {
        !matches!(self, Name | Path | Ext | Kind | Category)
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Has,
    NotHas,
    Re,
}

#[derive(Debug)]
pub enum Val {
    Num(f64),
    Str(String),
    Re(Regex),
}

#[derive(Debug)]
pub enum Expr {
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    Cmp(Field, Op, Val),
    In(Field, Vec<Val>),
    Hidden,
    Readonly,
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    L,
    R,
    Comma,
    Op(String),
    Word(String),
    Str(String),
}

const OPCH: &str = "=!<>~&|";

fn lex(s: &str) -> Result<Vec<Tok>, String> {
    let c: Vec<char> = s.chars().collect();
    let (mut i, mut out) = (0, vec![]);
    while i < c.len() {
        let st = i;
        match c[i] {
            w if w.is_whitespace() => i += 1,
            '(' => (out.push(Tok::L), i += 1).1,
            ')' => (out.push(Tok::R), i += 1).1,
            ',' => (out.push(Tok::Comma), i += 1).1,
            q @ ('"' | '\'') => {
                i += 1;
                while i < c.len() && c[i] != q {
                    i += 1;
                }
                if i == c.len() {
                    return Err("unterminated quote".into());
                }
                out.push(Tok::Str(c[st + 1..i].iter().collect()));
                i += 1;
            }
            x if OPCH.contains(x) => {
                while i < c.len() && OPCH.contains(c[i]) {
                    i += 1;
                }
                out.push(Tok::Op(c[st..i].iter().collect()));
            }
            _ => {
                while i < c.len() && !c[i].is_whitespace() && !"(),\"".contains(c[i]) && !OPCH.contains(c[i]) {
                    i += 1;
                }
                out.push(Tok::Word(c[st..i].iter().collect()));
            }
        }
    }
    Ok(out)
}

pub fn parse_size(s: &str) -> Result<f64, String> {
    let s = s.trim().to_lowercase();
    let split = s.find(|c: char| c.is_alphabetic()).unwrap_or(s.len());
    let n: f64 = s[..split].trim().parse().map_err(|_| format!("'{s}' is not a size (try 100mb)"))?;
    let mul = match s[split..].trim() {
        "" | "b" => 1u64,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1 << 40,
        u => return Err(format!("unknown size unit '{u}'")),
    };
    Ok(n * mul as f64)
}

fn parse_time(s: &str) -> Result<f64, String> {
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp() as f64);
    }
    let s = s.to_lowercase();
    let split = s.find(|c: char| c.is_alphabetic()).unwrap_or(s.len());
    let n: f64 = s[..split].parse().map_err(|_| format!("'{s}' is not a date (try 2024-01-31 or 30d)"))?;
    let unit = match &s[split..] {
        "h" => 3600.0,
        "d" | "" => 86400.0,
        "w" => 604800.0,
        "mo" | "m" => 2_592_000.0,
        "y" => 31_536_000.0,
        u => return Err(format!("unknown time unit '{u}' (use h, d, w, mo, y)")),
    };
    Ok(now() as f64 - n * unit)
}

fn val(f: Field, op: Op, s: &str) -> Result<Val, String> {
    let re = |p: String| Regex::new(&p).map(Val::Re).map_err(|e| e.to_string());
    if op == Op::Re {
        return re(format!("(?i){s}"));
    }
    if f.is_num() {
        if matches!(op, Op::Has | Op::NotHas) {
            return Err("'~' only works on text fields (name, path, ext, type, category)".into());
        }
        return Ok(Val::Num(match f {
            Size | Alloc => parse_size(s)?,
            Modified | Created | Accessed => parse_time(s)?,
            _ => s.parse().map_err(|_| format!("'{s}' is not a number"))?,
        }));
    }
    let s = if f == Ext { s.trim_start_matches('.') } else { s };
    if matches!(op, Op::Eq | Op::Ne) && s.contains(['*', '?']) {
        return re(format!("(?i)^{}$", regex::escape(s).replace("\\*", ".*").replace("\\?", ".")));
    }
    Ok(Val::Str(s.to_lowercase()))
}

struct P {
    t: Vec<Tok>,
    i: usize,
}

impl P {
    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.i)
    }
    fn next(&mut self) -> Option<Tok> {
        self.i += 1;
        self.t.get(self.i - 1).cloned()
    }
    fn is_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Tok::Word(w)) if w.eq_ignore_ascii_case(kw))
            || matches!((self.peek(), kw), (Some(Tok::Op(o)), "and") if o == "&&")
            || matches!((self.peek(), kw), (Some(Tok::Op(o)), "or") if o == "||")
            || matches!((self.peek(), kw), (Some(Tok::Op(o)), "not") if o == "!")
    }
    fn eat(&mut self, kw: &str) -> bool {
        let hit = self.is_kw(kw);
        self.i += hit as usize;
        hit
    }
    fn or(&mut self) -> Result<Expr, String> {
        let mut l = self.and()?;
        while self.eat("or") {
            l = Expr::Or(Box::new(l), Box::new(self.and()?));
        }
        Ok(l)
    }
    fn and(&mut self) -> Result<Expr, String> {
        let mut l = self.not()?;
        // "and" is optional: two conditions side by side are ANDed
        while self.eat("and") || !(self.peek().is_none() || self.peek() == Some(&Tok::R) || self.is_kw("or")) {
            l = Expr::And(Box::new(l), Box::new(self.not()?));
        }
        Ok(l)
    }
    fn not(&mut self) -> Result<Expr, String> {
        if self.eat("not") {
            return Ok(Expr::Not(Box::new(self.not()?)));
        }
        self.primary()
    }
    fn value(&mut self, after: &str) -> Result<String, String> {
        match self.next() {
            Some(Tok::Word(v) | Tok::Str(v)) => Ok(v),
            _ => Err(format!("expected a value after '{after}'")),
        }
    }
    fn primary(&mut self) -> Result<Expr, String> {
        let name = |s: &str| {
            let op = if s.contains(['*', '?']) { Op::Eq } else { Op::Has };
            Ok(Expr::Cmp(Name, op, val(Name, op, s)?))
        };
        match self.next() {
            Some(Tok::L) => {
                let e = self.or()?;
                if self.next() != Some(Tok::R) {
                    return Err("missing ')'".into());
                }
                Ok(e)
            }
            Some(Tok::Str(s)) => name(&s),
            Some(Tok::Word(w)) => {
                if let Some(f) = Field::parse(&w) {
                    if let Some(Tok::Op(o)) = self.peek().cloned() {
                        self.i += 1;
                        let op = match o.as_str() {
                            "=" | "==" => Op::Eq,
                            "!=" | "<>" => Op::Ne,
                            "<" => Op::Lt,
                            "<=" => Op::Le,
                            ">" => Op::Gt,
                            ">=" => Op::Ge,
                            "~" => Op::Has,
                            "!~" => Op::NotHas,
                            "=~" => Op::Re,
                            _ => return Err(format!("unknown operator '{o}'")),
                        };
                        let v = self.value(&format!("{w} {o}"))?;
                        return Ok(Expr::Cmp(f, op, val(f, op, &v)?));
                    }
                    if self.eat("in") {
                        if self.next() != Some(Tok::L) {
                            return Err(format!("expected '(' after '{w} in'"));
                        }
                        let mut vals = vec![];
                        loop {
                            vals.push(val(f, Op::Eq, &self.value("in (")?)?);
                            match self.next() {
                                Some(Tok::Comma) => {}
                                Some(Tok::R) => return Ok(Expr::In(f, vals)),
                                _ => return Err("expected ',' or ')' in list".into()),
                            }
                        }
                    }
                }
                match w.to_lowercase().as_str() {
                    "hidden" => Ok(Expr::Hidden),
                    "readonly" => Ok(Expr::Readonly),
                    "dir" | "folder" => Ok(Expr::Cmp(Kind, Op::Eq, Val::Str("dir".into()))),
                    "file" => Ok(Expr::Cmp(Kind, Op::Eq, Val::Str("file".into()))),
                    "empty" => Ok(Expr::Cmp(Size, Op::Eq, Val::Num(0.0))),
                    _ => name(&w),
                }
            }
            Some(Tok::Op(o)) => Err(format!("unexpected '{o}' — a condition looks like: size > 100mb")),
            Some(Tok::Comma) => Err("unexpected ','".into()),
            Some(Tok::R) => Err("unexpected ')'".into()),
            None => Err("incomplete filter".into()),
        }
    }
}

pub fn parse(s: &str) -> Result<Expr, String> {
    let mut p = P { t: lex(s)?, i: 0 };
    let e = p.or()?;
    if p.i < p.t.len() {
        return Err("unexpected ')'".into());
    }
    Ok(e)
}

fn num(t: &Tree, id: u32, f: Field) -> f64 {
    let n = &t.nodes[id as usize];
    match f {
        Size => n.size as f64,
        Alloc => n.alloc as f64,
        Files => n.files as f64,
        Dirs => n.dirs as f64,
        Depth => n.depth as f64,
        Modified => n.modified as f64,
        Created => n.created as f64,
        Accessed => n.accessed as f64,
        _ => (now() - n.modified) as f64 / 86400.0,
    }
}

fn text(t: &Tree, id: u32, f: Field) -> String {
    let n = &t.nodes[id as usize];
    match f {
        Path => t.path(id).to_string_lossy().to_lowercase(),
        Ext => t.ext(id),
        Kind => if n.is_dir { "dir" } else { "file" }.into(),
        Category => if n.is_dir { String::new() } else { category(&t.ext(id)).to_lowercase() },
        _ => n.name.to_lowercase(),
    }
}

fn cmp(t: &Tree, id: u32, f: Field, op: Op, v: &Val) -> bool {
    let by = |o: Ordering| match op {
        Op::Eq => o.is_eq(),
        Op::Ne => o.is_ne(),
        Op::Lt => o.is_lt(),
        Op::Le => o.is_le(),
        Op::Gt => o.is_gt(),
        Op::Ge => o.is_ge(),
        _ => false,
    };
    match v {
        Val::Num(x) => by(num(t, id, f).total_cmp(x)),
        Val::Re(r) => r.is_match(&text(t, id, f)) != matches!(op, Op::Ne | Op::NotHas),
        Val::Str(s) => {
            let a = text(t, id, f);
            match op {
                Op::Has => a.contains(s.as_str()),
                Op::NotHas => !a.contains(s.as_str()),
                _ => by(a.as_str().cmp(s.as_str())),
            }
        }
    }
}

impl Expr {
    pub fn eval(&self, t: &Tree, id: u32) -> bool {
        match self {
            Expr::And(a, b) => a.eval(t, id) && b.eval(t, id),
            Expr::Or(a, b) => a.eval(t, id) || b.eval(t, id),
            Expr::Not(a) => !a.eval(t, id),
            Expr::Cmp(f, op, v) => cmp(t, id, *f, *op, v),
            Expr::In(f, vs) => vs.iter().any(|v| cmp(t, id, *f, Op::Eq, v)),
            Expr::Hidden => t.nodes[id as usize].hidden,
            Expr::Readonly => t.nodes[id as usize].readonly,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct SortKey {
    pub field: Field,
    pub desc: bool,
}

pub fn parse_sort(s: &str) -> Result<Vec<SortKey>, String> {
    s.split(',')
        .filter(|p| !p.trim().is_empty())
        .map(|p| {
            let mut w = p.split_whitespace();
            let name = w.next().unwrap_or("");
            let field = Field::parse(name).ok_or(format!("unknown sort field '{name}'"))?;
            let desc = match w.next().map(str::to_lowercase).as_deref() {
                None | Some("asc") => false,
                Some("desc") => true,
                Some(x) => return Err(format!("expected asc or desc, got '{x}'")),
            };
            Ok(SortKey { field, desc })
        })
        .collect()
}

enum Key {
    N(f64),
    S(String),
}

/// Multi-key sort; keys are extracted once per item.
pub fn sort(t: &Tree, ids: &mut Vec<u32>, keys: &[SortKey]) {
    if keys.is_empty() {
        return;
    }
    let mut keyed: Vec<(u32, Vec<Key>)> = ids
        .iter()
        .map(|&id| {
            let k = keys.iter().map(|k| if k.field.is_num() { Key::N(num(t, id, k.field)) } else { Key::S(text(t, id, k.field)) });
            (id, k.collect())
        })
        .collect();
    keyed.sort_by(|a, b| {
        for (i, k) in keys.iter().enumerate() {
            let o = match (&a.1[i], &b.1[i]) {
                (Key::N(x), Key::N(y)) => x.total_cmp(y),
                (Key::S(x), Key::S(y)) => x.cmp(y),
                _ => Ordering::Equal,
            };
            if o.is_ne() {
                return if k.desc { o.reverse() } else { o };
            }
        }
        Ordering::Equal
    });
    ids.clear();
    ids.extend(keyed.into_iter().map(|k| k.0));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::Node;

    #[test]
    fn filter_and_sort() {
        let file = |name: &str, size, hidden| Node { name: name.into(), parent: 0, size, hidden, depth: 1, modified: now(), ..Default::default() };
        let t = Tree {
            root: "/r".into(),
            scanned: 0,
            nodes: vec![
                Node { name: "/r".into(), parent: u32::MAX, is_dir: true, end: 3, children: vec![1, 2], ..Default::default() },
                file("Movie.MP4", 2 << 30, false),
                file("notes.txt", 10, true),
            ],
        };
        let m = |q: &str| (1..3).filter(|&i| parse(q).unwrap().eval(&t, i)).collect::<Vec<u32>>();
        assert_eq!(m("size > 1gb and ext in (mp4, mkv)"), [1]);
        assert_eq!(m("not hidden or name ~ zzz"), [1]);
        assert_eq!(m("*.txt"), [2]);
        assert_eq!(m("category = videos or (hidden and size < 1kb)"), [1, 2]);
        assert_eq!(m("movie !hidden age < 1"), [1]);
        assert_eq!(m("path =~ \"r.notes\" modified > 1d"), [2]);
        assert!(parse("size >").is_err() && parse("(a").is_err() && parse("size ~ 3").is_err());
        let mut ids = vec![1, 2];
        sort(&t, &mut ids, &parse_sort("type, size asc").unwrap());
        assert_eq!(ids, [2, 1]);
    }

    #[test]
    fn saved_scan_round_trip() {
        let dir = std::env::temp_dir().join(format!("diskvibe-test-{}", now()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.bin"), [0u8; 100]).unwrap();
        std::fs::write(dir.join("sub/b.bin"), [0u8; 50]).unwrap();
        let t = crate::scan::scan(&dir, &Default::default());
        t.save().unwrap();
        let file = crate::scan::saved().into_iter().find(|s| s.1 == dir).unwrap().0;
        let l = Tree::load(&file).unwrap();
        let shape = |t: &Tree| t.nodes.iter().map(|n| (n.name.clone(), n.size, n.files, n.dirs, n.end, n.depth, n.children.clone())).collect::<Vec<_>>();
        assert_eq!(shape(&t), shape(&l));
        assert_eq!(l.nodes[0].size, 150);
        std::fs::remove_file(file).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
