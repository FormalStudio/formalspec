//! `fspec search`, and the suggestions a name that does not resolve gets: every catalog this host
//! can read cheaply, ranked against one query. Indexes are read from local caches where the tool
//! keeps one (Homebrew) or from fspec's own daily copy (the aqua registry, Flathub's app ids);
//! only npm and crates.io are asked per query, over a fetch with a short timeout.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow};
use fspec_core::fetch::Fetch;
use fspec_core::runner::Cmd;
use serde::Deserialize;
use serde_json::Value;

use crate::Cx;
use crate::aqua::{self, Registry};

/// Set by `--verbose`: the suggestions name the sources that failed.
pub static VERBOSE: AtomicBool = AtomicBool::new(false);

/// How long a downloaded index is used before it is fetched again.
const FRESH: Duration = Duration::from_secs(24 * 3600);
const FLATHUB_IDS: &str = "https://flathub.org/api/v2/appstream";
/// The suggestions under an unresolved name: at most this many, and per source.
const SUGGEST: usize = 8;
const PER_SOURCE: usize = 3;

/// Where this machine's catalogs live. Without one, [`Cx`] searches nothing, so tests and callers
/// that never set it touch no file and no network.
pub struct Catalog {
    /// `~/.cache/fspec/catalog`: fspec's own copies of downloaded indexes.
    pub cache: PathBuf,
    /// Homebrew's API cache (`~/Library/Caches/Homebrew/api`), on macOS.
    pub brew: Option<PathBuf>,
    /// HTTP with a short timeout, for per-query catalogs (npm, crates.io) and Flathub's id list.
    /// `None` searches offline.
    pub net: Option<Box<dyn Fetch>>,
    /// Flatpak's copies of Flathub's AppStream catalog (`.../appstream/flathub/<arch>/active`),
    /// the newest of which gives Flathub apps their names, summaries, versions and icons. With none
    /// of them there, Flathub is its bare id list.
    pub appstream: Vec<PathBuf>,
}

impl Catalog {
    pub fn local(home: &Path, net: Option<Box<dyn Fetch>>) -> Catalog {
        let cache = std::env::var_os("XDG_CACHE_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".cache"))
            .join("fspec/catalog");
        let brew = cfg!(target_os = "macos").then(|| home.join("Library/Caches/Homebrew/api"));
        let active = format!("appstream/flathub/{}/active", std::env::consts::ARCH);
        let appstream = vec![
            fspec_flatpak(home).join(&active),
            home.join(".local/share/flatpak").join(&active),
            PathBuf::from("/var/lib/flatpak").join(&active),
        ];
        Catalog { cache, brew, net, appstream }
    }
}

/// The flatpak installation `fspec run` and the catalog refresh use, `fspec_dev::cache_dir`'s
/// `flatpak/installation`.
pub fn fspec_flatpak(home: &Path) -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".cache"))
        .join("fspec/dev/flatpak/installation")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Origin {
    Aqua,
    Cask,
    Brew,
    Flathub,
    Zypper,
    Pacman,
    Apt,
    Cargo,
    Npm,
}

impl Origin {
    /// The id prefix, which `fspec run` and the spec's source builtin share.
    pub fn prefix(self) -> &'static str {
        match self {
            Origin::Aqua => "aqua",
            Origin::Cask | Origin::Brew => "brew",
            Origin::Flathub => "flatpak",
            Origin::Zypper => "zypper",
            Origin::Pacman => "pacman",
            Origin::Apt => "apt",
            Origin::Cargo => "cargo",
            Origin::Npm => "npm",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Origin::Aqua => "aqua",
            Origin::Cask => "Homebrew cask",
            Origin::Brew => "Homebrew",
            Origin::Flathub => "Flathub",
            Origin::Zypper => "zypper",
            Origin::Pacman => "pacman",
            Origin::Apt => "apt",
            Origin::Cargo => "crates.io",
            Origin::Npm => "npm",
        }
    }

    /// Whether `fspec run` starts it without installing. A Flathub app is installed into fspec's
    /// own flatpak installation, never the user's; a cask's app is unpacked into fspec's cache.
    pub fn runs(self) -> bool {
        matches!(self, Origin::Aqua | Origin::Cask | Origin::Flathub | Origin::Cargo | Origin::Npm)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub origin: Origin,
    pub name: String,
    /// Other names it answers to: aliases, an app id's last segment, a display name.
    pub keys: Vec<String>,
    pub description: Option<String>,
    /// What people call it, when the catalog says (`Firefox` for `org.mozilla.firefox`).
    pub title: Option<String>,
    pub version: Option<String>,
    /// A theme icon name or an absolute path.
    pub icon: Option<String>,
}

impl Hit {
    fn new(origin: Origin, name: impl Into<String>, description: Option<&str>) -> Hit {
        let description = description.map(str::trim).filter(|d| !d.is_empty()).map(str::to_string);
        Hit { origin, name: name.into(), keys: vec![], description, title: None, version: None, icon: None }
    }

    pub fn id(&self) -> String {
        format!("{}:{}", self.origin.prefix(), self.name)
    }

    /// What `fspec run` takes: a bare name for aqua, the id for the rest.
    pub fn run_arg(&self) -> Option<String> {
        match self.origin {
            Origin::Aqua => Some(self.name.clone()),
            o if o.runs() => Some(self.id()),
            _ => None,
        }
    }

    /// The `pkg(...)` a spec or `dev.star` declares it with.
    pub fn spec(&self) -> String {
        let last = self.name.rsplit('/').next().unwrap_or(&self.name);
        let n = &self.name;
        match self.origin {
            Origin::Aqua => format!("pkg({n:?})"),
            Origin::Npm => format!("pkg({last:?}, source = npm({n:?}))"),
            Origin::Cargo => format!("pkg({n:?}, source = cargo({n:?}))"),
            Origin::Cask => format!("pkg({n:?}, source = brew(cask = True))"),
            Origin::Brew => format!("pkg({n:?}, source = brew())"),
            Origin::Flathub => format!("pkg({n:?}, source = flatpak())"),
            o => format!("pkg({n:?}, source = {}())", o.prefix()),
        }
    }
}

/// A search's answer: hits best first with how each matched, and the sources that could not be
/// read.
#[derive(Default, Debug)]
pub struct Found {
    pub hits: Vec<(Match, Hit)>,
    pub skipped: Vec<(String, String)>,
}

/// How well a hit matches, smaller first: the tier, then a distance within it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Match {
    /// A name equal to the query, ignoring case.
    Exact,
    /// The query is the last word of a name run together from up to three, a vendor's before it
    /// (`MozillaFirefox`), by word count. `is-firefox` is a substring, not this.
    Word(usize),
    /// A spelling within a couple of edits (`rpigrep`), by edit distance.
    Close(usize),
    /// The query inside a name: at its start first, then by how much longer the name is.
    Substring(bool, usize),
    /// The query as a word of the description.
    Described,
}

/// Rank `hits` against `query`, dropping the ones that do not match at all. Equal matches keep
/// source order (aqua, Homebrew, Flathub, the distro, crates.io, npm), then the shorter name.
pub fn rank(query: &str, hits: Vec<Hit>) -> Vec<(Match, Hit)> {
    let q = query.to_lowercase();
    let mut out: Vec<(Match, Hit)> = hits.into_iter().filter_map(|h| Some((score(&q, &h)?, h))).collect();
    out.sort_by(|(ma, a), (mb, b)| (ma, a.origin, a.name.len(), &a.name).cmp(&(mb, b.origin, b.name.len(), &b.name)));
    out.dedup_by(|(_, a), (_, b)| a.origin == b.origin && a.name == b.name);
    out
}

fn score(q: &str, hit: &Hit) -> Option<Match> {
    let budget = match q.chars().count() {
        0..=3 => 0,
        4..=5 => 1,
        _ => 2,
    };
    let mut best: Option<Match> = None;
    for key in std::iter::once(&hit.name).chain(&hit.keys) {
        let k = key.to_lowercase();
        let w = words(key);
        let m = if k == q {
            Match::Exact
        } else if w.len() <= 3 && key.chars().all(char::is_alphanumeric) && w.last().is_some_and(|l| l == q) {
            Match::Word(w.len())
        } else if let Some(d) = (budget > 0 && k.chars().count().abs_diff(q.chars().count()) <= budget)
            .then(|| distance(&k, q))
            .filter(|d| *d <= budget)
        {
            Match::Close(d)
        } else if q.len() >= 2 && k.contains(q) {
            Match::Substring(!k.starts_with(q), k.len() - q.len())
        } else {
            continue;
        };
        best = Some(best.map_or(m, |b| b.min(m)));
    }
    if best.is_none() && q.len() >= 3 && hit.description.as_deref().is_some_and(|d| words(d).iter().any(|w| w == q)) {
        best = Some(Match::Described);
    }
    best
}

/// Lowercased words: split on anything not alphanumeric and where a capital follows a small
/// letter (`MozillaFirefox`).
fn words(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in s.chars() {
        if !c.is_alphanumeric() {
            out.extend((!cur.is_empty()).then(|| std::mem::take(&mut cur)));
            prev_lower = false;
            continue;
        }
        if c.is_uppercase() && prev_lower {
            out.push(std::mem::take(&mut cur));
        }
        prev_lower = c.is_lowercase() || c.is_numeric();
        cur.extend(c.to_lowercase());
    }
    out.extend((!cur.is_empty()).then_some(cur));
    out
}

/// Optimal string alignment distance: edits, with a swap of two neighbours counting once.
fn distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }
    d[a.len()][b.len()]
}

type Job<'a> = Box<dyn Fn() -> Result<Vec<Hit>> + Sync + 'a>;

/// Search every source this host has, in parallel. A source that fails is left out and named in
/// `skipped`. Without a [`Catalog`] on `cx` nothing is searched.
pub fn search(query: &str, cx: &Cx) -> Found {
    let Some(cat) = cx.catalog.as_ref() else { return Found::default() };
    let linux = cx.local.0.ends_with("-linux");
    let mut jobs: Vec<(&str, Job)> = vec![
        ("aqua", Box::new(|| aqua_hits(cx, cat))),
        ("cargo", Box::new(|| cat.net.as_deref().map_or(Ok(vec![]), |n| cargo_hits(query, n)))),
        ("npm", Box::new(|| cat.net.as_deref().map_or(Ok(vec![]), |n| npm_hits(query, n)))),
    ];
    if let Some(dir) = &cat.brew {
        jobs.push(("Homebrew", Box::new(|| brew_hits(dir))));
    }
    if linux {
        jobs.push(("Flathub", Box::new(|| flathub_hits(cat))));
        jobs.push(("distro", Box::new(|| distro_hits(query, cx))));
    }
    let results: Vec<(&str, Result<Vec<Hit>>)> = std::thread::scope(|s| {
        let handles: Vec<_> = jobs.iter().map(|(name, job)| (*name, s.spawn(job))).collect();
        handles.into_iter().map(|(name, h)| (name, h.join().unwrap_or_else(|_| Err(anyhow!("panicked"))))).collect()
    });
    let mut found = Found::default();
    let mut all = vec![];
    for (name, r) in results {
        match r {
            Ok(hits) => all.extend(hits),
            Err(e) => found.skipped.push((name.to_string(), format!("{e:#}"))),
        }
    }
    found.hits = rank(query, all);
    // The registry names packages `owner/repo`; what a user types is the short name.
    if let Some(reg) = cx.registry.get() {
        for (_, h) in found.hits.iter_mut().filter(|(_, h)| h.origin == Origin::Aqua).take(30) {
            h.name = reg.short_name(&h.name);
        }
    }
    found
}

/// Whether the aqua registry has `name`, read from the daily copy search keeps. A name several
/// entries share is there too: resolving it says which.
pub fn aqua_has(name: &str, cx: &Cx) -> Result<bool> {
    let cat = cx.catalog.as_ref().ok_or_else(|| anyhow!("no catalog to look `{name}` up in"))?;
    aqua_hits(cx, cat)?;
    let reg = cx.registry.get().expect("set by aqua_hits");
    Ok(reg.has(name))
}

/// The one package a name aqua lacks clearly means, which `fspec run` starts without asking: the
/// only runnable package of that exact name. An app (Flathub, Homebrew, the distro) of the name
/// outranks npm and crates.io, where short names are often squatted, so `firefox` is Flathub's
/// and never npm's `firefox`; with no runnable app among them, or two, nothing is picked.
pub fn pick<'f>(query: &str, found: &'f Found) -> Option<&'f Hit> {
    let q = query.to_lowercase();
    let named: Vec<&Hit> = found
        .hits
        .iter()
        .filter(|(m, h)| match m {
            Match::Exact => true,
            // `MozillaFirefox`, whose summary names Firefox.
            Match::Word(_) => {
                matches!(h.origin, Origin::Zypper | Origin::Pacman | Origin::Apt)
                    && h.description.as_deref().is_some_and(|d| words(d).contains(&q))
            }
            _ => false,
        })
        .map(|(_, h)| h)
        .collect();
    let apps: Vec<&Hit> = named.iter().filter(|h| !matches!(h.origin, Origin::Npm | Origin::Cargo)).copied().collect();
    // With an app catalog unread, an npm or crates.io name may be squatting on an app it holds.
    if apps.is_empty() && found.skipped.iter().any(|(s, _)| matches!(s.as_str(), "Homebrew" | "Flathub" | "distro")) {
        return None;
    }
    let pool = if apps.is_empty() { named } else { apps };
    match pool.into_iter().filter(|h| h.origin.runs()).collect::<Vec<_>>().as_slice() {
        [one] => Some(one),
        _ => None,
    }
}

/// The lines that follow `` `query` is not in the aqua registry ``: the best few matches from
/// every source, each with what to type, the ones `fspec run` starts first.
pub fn suggest(query: &str, cx: &Cx) -> Option<String> {
    suggest_from(&search(query, cx))
}

/// [`suggest`] for a search already made.
pub fn suggest_from(found: &Found) -> Option<String> {
    let mut per: std::collections::HashMap<Origin, usize> = Default::default();
    // A description that mentions the name is too loose a match to offer as a correction.
    let mut hits: Vec<&Hit> = found
        .hits
        .iter()
        .filter(|(m, _)| *m != Match::Described)
        .map(|(_, h)| h)
        .filter(|h| {
            let n = per.entry(h.origin).or_default();
            *n += 1;
            *n <= PER_SOURCE
        })
        .take(SUGGEST)
        .collect();
    hits.sort_by_key(|h| !h.origin.runs());
    let mut out = String::new();
    if !hits.is_empty() {
        out.push_str("similar packages:\n");
        out.push_str(&table(&hits));
    }
    if VERBOSE.load(Ordering::Relaxed) {
        for (name, e) in &found.skipped {
            out.push_str(&format!("  (searching {name} failed: {e})\n"));
        }
    }
    (!out.is_empty()).then(|| out.trim_end().to_string())
}

/// One row per hit: its id, source, description, and the `fspec run` that starts it or the
/// `pkg(...)` that installs it.
pub fn table(hits: &[&Hit]) -> String {
    let cut = |s: &str, n: usize| {
        if s.chars().count() <= n {
            s.to_string()
        } else {
            format!("{}...", s.chars().take(n - 3).collect::<String>().trim_end())
        }
    };
    let rows: Vec<[String; 4]> = hits
        .iter()
        .map(|h| {
            let how = match h.run_arg() {
                Some(a) => format!("fspec run {a}"),
                None => format!("install only: {}", h.spec()),
            };
            [h.id(), h.origin.label().to_string(), cut(h.description.as_deref().unwrap_or(""), 44), how]
        })
        .collect();
    let width = |i: usize| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0);
    let (w0, w1, w2) = (width(0), width(1), width(2));
    rows.iter().map(|r| format!("  {:w0$}  {:w1$}  {:w2$}  {}\n", r[0], r[1], r[2], r[3])).collect()
}

fn aqua_hits(cx: &Cx, cat: &Catalog) -> Result<Vec<Hit>> {
    if cx.registry.get().is_none() {
        let body = cached(&cat.cache.join("aqua-registry.yaml"), aqua::REGISTRY, Some(cx.fetch))?;
        let _ = cx.registry.set(Registry::parse(&body)?);
    }
    let reg = cx.registry.get().expect("set above");
    Ok(reg.listing().map(|(full, keys, desc)| Hit { keys, ..Hit::new(Origin::Aqua, full, desc) }).collect())
}

/// Flathub's app ids, one download a day. They carry no description; the last segment of an id
/// is the name a user knows the app by.
fn flathub_hits(cat: &Catalog) -> Result<Vec<Hit>> {
    if let Some(active) = crate::appstream::newest(&cat.appstream) {
        let apps = crate::appstream::apps(&active, &cat.cache.join("flathub-apps.json"))?;
        return Ok(apps
            .into_iter()
            .map(|a| {
                let last = a.id.rsplit('.').next().unwrap_or(&a.id).to_string();
                let keys = std::iter::once(last).chain(a.name.clone()).collect();
                Hit {
                    keys,
                    title: a.name,
                    version: a.version,
                    icon: a.icon,
                    ..Hit::new(Origin::Flathub, &a.id, a.summary.as_deref())
                }
            })
            .collect());
    }
    let body = cached(&cat.cache.join("flathub-ids.json"), FLATHUB_IDS, cat.net.as_deref())?;
    let ids: Vec<String> = serde_json::from_slice(&body).context("parsing Flathub's app ids")?;
    Ok(ids
        .into_iter()
        .map(|id| {
            let last = id.rsplit('.').next().unwrap_or(&id).to_string();
            Hit { keys: vec![last], ..Hit::new(Origin::Flathub, id, None) }
        })
        .collect())
}

/// Homebrew's own API cache: the internal packages file carries names and descriptions; an
/// older Homebrew keeps only the name lists.
fn brew_hits(dir: &Path) -> Result<Vec<Hit>> {
    let payload = std::fs::read_dir(dir.join("internal"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.to_string_lossy().ends_with(".jws.json.payload"));
    if let Some(p) = payload {
        return brew_payload(&std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?);
    }
    let lines =
        |f: &str| std::fs::read_to_string(dir.join(f)).with_context(|| format!("reading {}", dir.join(f).display()));
    let mut hits: Vec<Hit> = lines("formula_names.txt")?.lines().map(|n| Hit::new(Origin::Brew, n, None)).collect();
    hits.extend(lines("cask_names.txt")?.lines().map(|n| Hit::new(Origin::Cask, n, None)));
    Ok(hits)
}

fn brew_payload(body: &[u8]) -> Result<Vec<Hit>> {
    #[derive(Deserialize)]
    struct Formula {
        desc: Option<String>,
    }
    #[derive(Deserialize)]
    struct Cask {
        desc: Option<String>,
        #[serde(default)]
        names: Vec<String>,
    }
    #[derive(Deserialize)]
    struct Payload {
        #[serde(default)]
        formulae: std::collections::BTreeMap<String, Formula>,
        #[serde(default)]
        casks: std::collections::BTreeMap<String, Cask>,
        #[serde(default)]
        formula_aliases: std::collections::BTreeMap<String, String>,
    }
    // The file is the signature's document followed by the payload's.
    let mut docs = serde_json::Deserializer::from_slice(body).into_iter::<Payload>();
    let p = docs
        .find(|d| d.as_ref().map_or(true, |d| !d.formulae.is_empty() || !d.casks.is_empty()))
        .ok_or_else(|| anyhow!("Homebrew's packages file lists no packages"))?
        .context("parsing Homebrew's packages file")?;
    let mut aliases: std::collections::HashMap<&str, Vec<String>> = Default::default();
    for (alias, name) in &p.formula_aliases {
        aliases.entry(name.as_str()).or_default().push(alias.clone());
    }
    let mut hits: Vec<Hit> = p
        .formulae
        .iter()
        .map(|(n, f)| Hit {
            keys: aliases.get(n.as_str()).cloned().unwrap_or_default(),
            ..Hit::new(Origin::Brew, n, f.desc.as_deref())
        })
        .collect();
    hits.extend(
        p.casks.into_iter().map(|(n, c)| Hit { keys: c.names, ..Hit::new(Origin::Cask, n, c.desc.as_deref()) }),
    );
    Ok(hits)
}

/// The distro's own search, which matches substrings only; typos are the other sources' to catch.
fn distro_hits(query: &str, cx: &Cx) -> Result<Vec<Hit>> {
    let distro = cx.local.1.as_str();
    let re = regex_escape(query);
    let (origin, cmd) = if distro.contains("suse") {
        (Origin::Zypper, Cmd::new("zypper").args(["--no-refresh", "-x", "-q", "search", "-t", "package", "--", query]))
    } else if matches!(distro, "arch" | "archarm" | "cachyos" | "endeavouros" | "manjaro") {
        (Origin::Pacman, Cmd::new("pacman").args(["-Ss", "--", &re]))
    } else if matches!(distro, "debian" | "ubuntu" | "linuxmint" | "pop" | "raspbian") {
        (Origin::Apt, Cmd::new("apt-cache").args(["search", "--names-only", "--", &re]))
    } else {
        return Ok(vec![]);
    };
    let out = cx.runner.run(&cmd)?;
    // zypper exits 104 and pacman 1 when nothing matched.
    if out.status != 0 && out.stdout.trim().is_empty() {
        return Ok(vec![]);
    }
    Ok(match origin {
        Origin::Zypper => parse_zypper(&out.stdout),
        Origin::Pacman => parse_pacman(&out.stdout),
        _ => parse_apt(&out.stdout),
    })
}

fn regex_escape(s: &str) -> String {
    s.chars().flat_map(|c| if "\\.+*?()|[]{}^$".contains(c) { vec!['\\', c] } else { vec![c] }).collect()
}

fn parse_zypper(xml: &str) -> Vec<Hit> {
    let attr = |line: &str, key: &str| {
        let start = line.find(&format!(" {key}=\""))? + key.len() + 3;
        let end = line[start..].find('"')? + start;
        Some(unescape(&line[start..end]))
    };
    xml.lines()
        .filter(|l| l.trim_start().starts_with("<solvable "))
        .filter_map(|l| Some(Hit::new(Origin::Zypper, attr(l, "name")?, attr(l, "summary").as_deref())))
        .collect()
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// `extra/firefox 144.0-1 [installed]` then an indented description line.
fn parse_pacman(out: &str) -> Vec<Hit> {
    let mut hits = vec![];
    let mut lines = out.lines().peekable();
    while let Some(l) = lines.next() {
        if l.starts_with(char::is_whitespace) {
            continue;
        }
        let Some(name) = l.split_whitespace().next().and_then(|r| r.split('/').nth(1)) else { continue };
        let desc = lines.next_if(|n| n.starts_with(char::is_whitespace));
        hits.push(Hit::new(Origin::Pacman, name, desc));
    }
    hits
}

/// `firefox-esr - Mozilla Firefox web browser - Extended Support Release (ESR)`.
fn parse_apt(out: &str) -> Vec<Hit> {
    out.lines()
        .filter_map(|l| {
            let (name, desc) = l.split_once(" - ")?;
            Some(Hit::new(Origin::Apt, name.trim(), Some(desc)))
        })
        .collect()
}

fn npm_hits(query: &str, net: &dyn Fetch) -> Result<Vec<Hit>> {
    let doc = net.get_json(&format!("https://registry.npmjs.org/-/v1/search?size=10&text={}", encode(query)))?;
    let objects =
        doc.get("objects").and_then(Value::as_array).ok_or_else(|| anyhow!("npm search answered no objects"))?;
    Ok(objects
        .iter()
        .filter_map(|o| {
            let p = o.get("package")?;
            Some(Hit::new(Origin::Npm, p.get("name")?.as_str()?, p.get("description").and_then(Value::as_str)))
        })
        .collect())
}

fn cargo_hits(query: &str, net: &dyn Fetch) -> Result<Vec<Hit>> {
    let doc = net.get_json(&format!("https://crates.io/api/v1/crates?per_page=10&q={}", encode(query)))?;
    let crates = doc.get("crates").and_then(Value::as_array).ok_or_else(|| anyhow!("crates.io answered no crates"))?;
    Ok(crates
        .iter()
        .filter_map(|c| {
            Some(Hit::new(Origin::Cargo, c.get("name")?.as_str()?, c.get("description").and_then(Value::as_str)))
        })
        .collect())
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// `url`'s body, kept at `path` for a day. A stale copy is fetched again, and still used when
/// that fails; with no `fetch` (offline) any copy is used.
fn cached(path: &Path, url: &str, fetch: Option<&dyn Fetch>) -> Result<Vec<u8>> {
    let age =
        std::fs::metadata(path).and_then(|m| m.modified()).ok().and_then(|t| SystemTime::now().duration_since(t).ok());
    if age.is_some_and(|a| a < FRESH) {
        return std::fs::read(path).with_context(|| format!("reading {}", path.display()));
    }
    let Some(fetch) = fetch else {
        return std::fs::read(path)
            .with_context(|| format!("no copy of {url} at {} to search offline", path.display()));
    };
    match fetch.get(url) {
        Ok(body) => {
            // A cache that cannot be written only costs the next search a download.
            if std::fs::create_dir_all(path.parent().unwrap_or(path)).is_ok() {
                let tmp = path.with_extension("part");
                if std::fs::write(&tmp, &body).is_ok() {
                    let _ = std::fs::rename(&tmp, path);
                }
            }
            Ok(body)
        }
        Err(e) if age.is_some() => std::fs::read(path).map_err(|_| e),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(origin: Origin, name: &str, keys: &[&str]) -> Hit {
        Hit { keys: keys.iter().map(ToString::to_string).collect(), ..Hit::new(origin, name, None) }
    }

    fn ranked(query: &str, hits: Vec<Hit>) -> Vec<String> {
        rank(query, hits).into_iter().map(|(_, h)| h.id()).collect()
    }

    #[test]
    fn exact_names_in_any_source_come_before_words_spellings_and_substrings() {
        let hits = vec![
            hit(Origin::Npm, "firefox-profile", &[]),
            hit(Origin::Npm, "is-firefox", &[]),
            hit(Origin::Zypper, "MozillaFirefox-translations-common", &[]),
            hit(Origin::Zypper, "MozillaFirefox", &[]),
            hit(Origin::Flathub, "org.mozilla.firefox", &["firefox"]),
            hit(Origin::Cask, "firefox", &["Mozilla Firefox"]),
            hit(Origin::Aqua, "someone/firefly", &["firefly"]),
            hit(Origin::Brew, "unrelated", &[]),
        ];
        assert_eq!(
            ranked("firefox", hits),
            [
                "brew:firefox",
                "flatpak:org.mozilla.firefox",
                "zypper:MozillaFirefox",
                "aqua:someone/firefly",
                "npm:firefox-profile",
                "npm:is-firefox",
                "zypper:MozillaFirefox-translations-common",
            ]
        );
    }

    fn found(query: &str, hits: Vec<Hit>) -> Found {
        Found { hits: rank(query, hits), skipped: vec![] }
    }

    #[test]
    fn a_bare_name_runs_the_one_app_of_that_name_over_npm_and_cargo_squats() {
        let firefox = found(
            "firefox",
            vec![
                hit(Origin::Npm, "firefox", &[]),
                hit(Origin::Cargo, "firefox", &[]),
                Hit::new(Origin::Zypper, "MozillaFirefox", Some("Mozilla Firefox Web Browser")),
                hit(Origin::Flathub, "org.mozilla.firefox", &["firefox"]),
                hit(Origin::Flathub, "org.mozilla.Thunderbird", &["Thunderbird"]),
            ],
        );
        assert_eq!(pick("firefox", &firefox).map(Hit::id).as_deref(), Some("flatpak:org.mozilla.firefox"));
        assert_eq!(pick("FireFox", &firefox).map(Hit::id).as_deref(), Some("flatpak:org.mozilla.firefox"));
        let mac = found(
            "firefox",
            vec![hit(Origin::Npm, "firefox", &[]), hit(Origin::Cask, "firefox", &["Mozilla Firefox"])],
        );
        assert_eq!(pick("firefox", &mac).map(Hit::id).as_deref(), Some("brew:firefox"));
        // A tool only npm has is npm's.
        let cowsay = found("cowsay", vec![hit(Origin::Npm, "cowsay", &[]), hit(Origin::Npm, "cowsay2", &[])]);
        assert_eq!(pick("cowsay", &cowsay).map(Hit::id).as_deref(), Some("npm:cowsay"));
        // Unless an app catalog could not be read, which might hold the app.
        let blind = Found {
            skipped: vec![("Homebrew".into(), "no cache".into())],
            ..found("firefox", vec![hit(Origin::Npm, "firefox", &[])])
        };
        assert_eq!(pick("firefox", &blind), None);
    }

    #[test]
    fn an_unclear_name_picks_nothing() {
        // The distro's htop cannot run uninstalled, and npm's is likely a squat.
        let htop = found("htop", vec![hit(Origin::Zypper, "htop", &[]), hit(Origin::Npm, "htop", &[])]);
        assert_eq!(pick("htop", &htop), None);
        // Two Flathub apps of one name.
        let two = found(
            "notes",
            vec![hit(Origin::Flathub, "a.b.notes", &["notes"]), hit(Origin::Flathub, "c.d.Notes", &["Notes"])],
        );
        assert_eq!(pick("notes", &two), None);
        // npm and crates.io both, with no app.
        let xh = found("xh", vec![hit(Origin::Npm, "xh", &[]), hit(Origin::Cargo, "xh", &[])]);
        assert_eq!(pick("xh", &xh), None);
        // A near name is not the name.
        let close = found("firefox", vec![hit(Origin::Flathub, "org.mozilla.firefox-esr", &["firefox-esr"])]);
        assert_eq!(pick("firefox", &close), None);
    }

    #[test]
    fn suggestions_list_what_runs_first() {
        let f = found(
            "htop",
            vec![hit(Origin::Zypper, "htop", &[]), hit(Origin::Npm, "htop", &[]), hit(Origin::Brew, "htop", &[])],
        );
        let s = suggest_from(&f).unwrap();
        let rows: Vec<&str> = s.lines().skip(1).map(|l| l.split_whitespace().next().unwrap()).collect();
        assert_eq!(rows, ["npm:htop", "brew:htop", "zypper:htop"], "{s}");
        assert!(s.contains("install only: pkg(\"htop\", source = zypper())"), "{s}");
    }

    #[test]
    fn a_swapped_pair_of_letters_is_one_edit() {
        assert_eq!(distance("rpigrep", "ripgrep"), 1);
        assert_eq!(distance("kitten", "sitting"), 3);
        let hits = vec![hit(Origin::Aqua, "BurntSushi/ripgrep", &["ripgrep"]), hit(Origin::Npm, "grep", &[])];
        assert_eq!(
            rank("rpigrep", hits).first().map(|(m, h)| (*m, h.name.as_str())),
            Some((Match::Close(1), "BurntSushi/ripgrep"))
        );
    }

    #[test]
    fn short_queries_are_never_fuzzy_and_descriptions_rank_last() {
        assert_eq!(ranked("jq", vec![hit(Origin::Aqua, "jo", &[])]), Vec::<String>::new());
        let described = Hit::new(Origin::Npm, "browser-launcher", Some("Launch firefox and chrome"));
        let named = hit(Origin::Npm, "firefox-launcher", &[]);
        assert_eq!(ranked("firefox", vec![described, named]), ["npm:firefox-launcher", "npm:browser-launcher"]);
    }

    #[test]
    fn words_split_on_case_and_punctuation() {
        assert_eq!(words("MozillaFirefox-devel"), ["mozilla", "firefox", "devel"]);
        assert_eq!(words("org.mozilla.firefox"), ["org", "mozilla", "firefox"]);
        assert_eq!(words("GTK4"), ["gtk4"]);
    }

    #[test]
    fn what_cannot_run_says_how_to_install_it() {
        let cask = hit(Origin::Cask, "firefox", &[]);
        assert_eq!(cask.run_arg().as_deref(), Some("brew:firefox"));
        assert_eq!(cask.spec(), r#"pkg("firefox", source = brew(cask = True))"#);
        assert_eq!(
            hit(Origin::Flathub, "org.mozilla.firefox", &[]).run_arg().as_deref(),
            Some("flatpak:org.mozilla.firefox")
        );
        let formula = hit(Origin::Brew, "htop", &[]);
        assert_eq!(formula.run_arg(), None);
        assert_eq!(hit(Origin::Aqua, "ripgrep", &[]).run_arg().as_deref(), Some("ripgrep"));
        assert_eq!(hit(Origin::Npm, "@scope/tool", &[]).run_arg().as_deref(), Some("npm:@scope/tool"));
        let t = table(&[&formula, &hit(Origin::Aqua, "ripgrep", &[])]);
        assert!(t.contains("install only: pkg(\"htop\", source = brew())"), "{t}");
        assert!(t.contains("fspec run ripgrep"), "{t}");
    }

    #[test]
    fn distro_searches_parse() {
        let z = parse_zypper(
            r#"<solvable status="not-installed" name="MozillaFirefox" summary="Mozilla Firefox Web Browser &amp; more" kind="package"/>"#,
        );
        assert_eq!(z, [Hit::new(Origin::Zypper, "MozillaFirefox", Some("Mozilla Firefox Web Browser & more"))]);
        let p = parse_pacman(
            "extra/firefox 144.0-1 [installed]\n    Fast, Private & Safe Web Browser\nextra/firefox-i18n-de 144.0-1\n",
        );
        assert_eq!(p.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), ["firefox", "firefox-i18n-de"]);
        assert_eq!(p[0].description.as_deref(), Some("Fast, Private & Safe Web Browser"));
        let a = parse_apt("firefox-esr - Mozilla Firefox web browser - Extended Support Release (ESR)\n");
        assert_eq!(a[0].description.as_deref(), Some("Mozilla Firefox web browser - Extended Support Release (ESR)"));
    }

    #[test]
    fn homebrew_payload_gives_formulae_casks_and_aliases() {
        let body = br#"{"protected":"x","signature":"y"}{"formulae":{"ripgrep":{"desc":"Search tool"},"the_silver_searcher":{"desc":"Ag"}},
            "casks":{"firefox":{"desc":"Web browser","names":["Mozilla Firefox"]}},
            "formula_aliases":{"ag":"the_silver_searcher"}}"#;
        let hits = brew_payload(body).unwrap();
        assert_eq!(ranked("ag", hits.clone()), ["brew:the_silver_searcher"]);
        assert_eq!(ranked("firefox", hits), ["brew:firefox"]);
    }
}
