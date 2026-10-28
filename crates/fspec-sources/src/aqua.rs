//! The aqua registry: package definitions as YAML with Go-style asset templates and an
//! expression language for version constraints. `github_release` and `http` packages are resolved.

use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow, bail};
use fspec_core::lock::{Artifact, LockedPkg};
use serde::Deserialize;

use crate::Cx;
use crate::github::{Platform, pick_release, pick_release_tagged, trim_v};
use crate::hash::hash_of;

const REGISTRY: &str = "https://raw.githubusercontent.com/aquaproj/aqua-registry/main/registry.yaml";

#[derive(Deserialize, Debug, Default)]
pub struct Registry {
    #[serde(default)]
    packages: Vec<Entry>,
}

#[derive(Deserialize, Debug, Default, Clone)]
struct Entry {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    repo_owner: Option<String>,
    #[serde(default)]
    repo_name: Option<String>,
    #[serde(default)]
    aliases: Vec<Alias>,
    #[serde(default)]
    version_overrides: Vec<Def>,
    #[serde(flatten)]
    def: Def,
}

#[derive(Deserialize, Debug, Default, Clone)]
struct Alias {
    name: String,
}

#[derive(Deserialize, Debug, Default, Clone)]
struct Def {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    version_constraint: Option<String>,
    #[serde(default)]
    version_prefix: Option<String>,
    #[serde(default)]
    asset: Option<String>,
    /// For `http` packages: the download URL template.
    #[serde(default)]
    url: Option<String>,
    /// `github_tag` takes versions from the repository's tags instead of its releases.
    #[serde(default)]
    version_source: Option<String>,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    format_overrides: Vec<FormatOverride>,
    #[serde(default, deserialize_with = "replacements")]
    replacements: Option<BTreeMap<String, String>>,
    #[serde(default)]
    overrides: Vec<Override>,
    #[serde(default)]
    files: Option<Vec<FileSpec>>,
    #[serde(default)]
    supported_envs: Vec<String>,
}

#[derive(Deserialize, Debug, Clone)]
struct FormatOverride {
    goos: String,
    format: String,
}

#[derive(Deserialize, Debug, Clone, Default)]
struct Override {
    #[serde(default)]
    goos: Option<String>,
    #[serde(default)]
    goarch: Option<String>,
    #[serde(default)]
    asset: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    format: Option<String>,
    #[serde(default, deserialize_with = "replacements")]
    replacements: Option<BTreeMap<String, String>>,
    #[serde(default)]
    files: Option<Vec<FileSpec>>,
}

/// Registry keys and values may be bare YAML numbers (`386: i386`).
#[derive(Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(untagged)]
enum Scalar {
    Str(String),
    Int(i64),
    Bool(bool),
}

impl std::fmt::Display for Scalar {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Scalar::Str(s) => f.write_str(s),
            Scalar::Int(i) => write!(f, "{i}"),
            Scalar::Bool(b) => write!(f, "{b}"),
        }
    }
}

fn replacements<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<BTreeMap<String, String>>, D::Error> {
    let raw: Option<BTreeMap<Scalar, Scalar>> = Option::deserialize(d)?;
    Ok(raw.map(|m| m.into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()))
}

#[derive(Deserialize, Debug, Clone)]
struct FileSpec {
    name: String,
    #[serde(default)]
    src: Option<String>,
}

/// A lock bin entry: the path inside the artifact, prefixed with `<command>=` when the command
/// is named differently (aqua's `link`, which tools that act on `$0` rely on).
fn bin_entry(name: &str, path: &str) -> String {
    if path.rsplit('/').next() == Some(name) { path.to_string() } else { format!("{name}={path}") }
}

impl Entry {
    fn repo(&self) -> Option<String> {
        Some(format!("{}/{}", self.repo_owner.as_deref()?, self.repo_name.as_deref()?))
    }

    fn full_name(&self) -> Option<String> {
        self.name.clone().or_else(|| self.repo())
    }
}

impl Registry {
    pub fn parse(yaml: &[u8]) -> Result<Registry> {
        serde_yaml::from_slice(yaml).context("parsing the aqua registry")
    }

    fn find(&self, query: &str) -> Result<&Entry> {
        let by = |f: &dyn Fn(&Entry) -> bool| self.packages.iter().filter(|e| f(e)).collect::<Vec<_>>();
        let exact = by(&|e| e.full_name().as_deref() == Some(query));
        // `tamasfe/taplo/full` shares its repo with `tamasfe/taplo`, so a repo match only counts
        // when no package carries the query as its name.
        let repo = by(&|e| e.repo().as_deref() == Some(query));
        let aliased = by(&|e| e.aliases.iter().any(|a| a.name == query));
        let named = by(&|e| e.repo_name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(query)));
        // `nextest-rs/nextest/cargo-nextest` is found as `cargo-nextest`.
        let last = by(&|e| e.full_name().is_some_and(|n| n.rsplit('/').next() == Some(query)));
        for pool in [exact, repo, aliased, named, last] {
            match pool.as_slice() {
                [] => continue,
                [one] => return Ok(one),
                many => {
                    let gh: Vec<_> = many.iter().filter(|e| e.def.kind.as_deref() == Some("github_release")).collect();
                    if let [one] = gh.as_slice() {
                        return Ok(one);
                    }
                    let names: Vec<_> = many.iter().filter_map(|e| e.full_name()).collect();
                    bail!("`{query}` matches several aqua packages ({}); name one of them", names.join(", "));
                }
            }
        }
        bail!("`{query}` is not in the aqua registry")
    }
}

/// Resolve aqua package `query`, installed as `pkg_name` (which names a bare downloaded binary).
pub fn resolve(
    query: &str,
    pkg_name: &str,
    version: Option<&str>,
    platforms: &[String],
    cutoff: Option<i64>,
    cx: &Cx,
    out: &mut LockedPkg,
) -> Result<()> {
    if cx.registry.get().is_none() {
        let reg = Registry::parse(&cx.fetch.get(REGISTRY)?)?;
        let _ = cx.registry.set(reg);
    }
    let entry = cx.registry.get().expect("set above").find(query)?;
    let repo = entry.repo().ok_or_else(|| anyhow!("aqua package `{query}` has no repo_owner/repo_name"))?;
    let kind = entry.def.kind.as_deref().unwrap_or("?");
    if !matches!(kind, "github_release" | "http") {
        bail!("aqua package type `{kind}` is not supported yet, only github_release and http");
    }
    let prefix = entry.def.version_prefix.as_deref();
    let tags = entry.def.version_source.as_deref() == Some("github_tag");
    let (tag, release) = match (version, kind, tags) {
        (Some(v), "http", true) => (format!("{}{v}", prefix.unwrap_or_default()), None),
        (None, "http", true) => {
            if cutoff.is_some() {
                bail!("{repo} takes versions from git tags, which carry no date for release_age; pin a version");
            }
            (newest_tag(&repo, prefix, cx)?, None)
        }
        (Some(v), ..) => {
            let r = pick_release_tagged(&repo, &format!("{}{v}", prefix.unwrap_or_default()), cx)?;
            (r.tag_name.clone(), Some(r))
        }
        (None, ..) => {
            let r = pick_release(&repo, cutoff, prefix, cx)?;
            (r.tag_name.clone(), Some(r))
        }
    };
    let def = effective(entry, &tag)?;
    let kind = def.kind.as_deref().unwrap_or(kind);
    out.version = Some(trim_v(prefix.and_then(|p| tag.strip_prefix(p)).unwrap_or(&tag)).to_string());
    for p in platforms {
        let plat = Platform::parse(p)?;
        if !supported(&def.supported_envs, plat) {
            bail!("{repo} does not support {p} (supported_envs: {})", def.supported_envs.join(", "));
        }
        let r = for_platform(&def, plat);
        let vars = Vars::new(&tag, prefix, plat, &r);
        let (url, digest, name) = if kind == "http" {
            let tpl = r.url.as_deref().ok_or_else(|| anyhow!("{repo} {tag}: the registry gives no url template"))?;
            let url = expand(tpl, &vars)?;
            let name = url.rsplit('/').next().unwrap_or_default().to_string();
            (url, None, name)
        } else {
            let release = release.as_ref().expect("github_release packages pick a release");
            let asset_tpl =
                r.asset.as_deref().ok_or_else(|| anyhow!("{repo} {tag}: the registry gives no asset template"))?;
            let name = expand(asset_tpl, &vars)?;
            let asset = release.assets.iter().find(|a| a.name == name).ok_or_else(|| {
                let names: Vec<_> = release.assets.iter().map(|a| a.name.as_str()).collect();
                anyhow!("{repo} {tag}: expected asset `{name}` for {p} but the release has: {}", names.join(", "))
            })?;
            (asset.browser_download_url.clone(), asset.digest.clone(), name)
        };
        let vars = vars.with_asset(&name);
        let single = matches!(r.format.as_str(), "raw" | "gz" | "xz" | "zst");
        let mut bins = Vec::new();
        for f in r.files.iter().flatten() {
            // A bare binary, or one compressed without a tar, is stored under the package's name,
            // whatever the download was called.
            let path = if single { pkg_name.to_string() } else { expand(f.src.as_deref().unwrap_or(&f.name), &vars)? };
            bins.push(bin_entry(&f.name, &path));
        }
        if single && bins.is_empty() {
            bins.push(pkg_name.to_string());
        }
        out.artifacts.insert(
            p.clone(),
            Artifact {
                url: Some(url.clone()),
                hash: Some(hash_of(&url, digest.as_deref(), cx)?),
                bins,
                ..Default::default()
            },
        );
    }
    Ok(())
}

/// The highest stable version among the repository's tags.
fn newest_tag(repo: &str, prefix: Option<&str>, cx: &Cx) -> Result<String> {
    let url = format!("https://api.github.com/repos/{repo}/tags?per_page=100");
    let tags = cx.fetch.get_json(&url)?;
    let names =
        tags.as_array().with_context(|| format!("{url} returned no list"))?.iter().filter_map(|t| t["name"].as_str());
    names
        .filter(|n| prefix.is_none_or(|p| n.starts_with(p)))
        .filter(|n| {
            let v = trim_v(prefix.and_then(|p| n.strip_prefix(p)).unwrap_or(n));
            !v.is_empty() && v.chars().all(|c| c.is_ascii_digit() || c == '.')
        })
        .max_by(|a, b| cmp_ver(&parse_ver(a), &parse_ver(b)))
        .map(str::to_string)
        .ok_or_else(|| anyhow!("{repo} has no stable version tag"))
}

/// The package definition for `tag`: the base, or the first `version_overrides` entry whose
/// constraint holds, laid over the base.
fn effective(entry: &Entry, tag: &str) -> Result<Def> {
    let base = &entry.def;
    let semver = base.version_prefix.as_deref().and_then(|p| tag.strip_prefix(p)).unwrap_or(tag);
    let use_base = match base.version_constraint.as_deref() {
        None | Some("") => entry.version_overrides.is_empty(),
        Some(c) => eval_constraint(c, tag, semver).with_context(|| format!("version_constraint `{c}`"))?,
    };
    if use_base {
        return Ok(base.clone());
    }
    for o in &entry.version_overrides {
        let c = o.version_constraint.as_deref().unwrap_or("true");
        if eval_constraint(c, tag, semver).with_context(|| format!("version_constraint `{c}`"))? {
            return Ok(layer(base, o));
        }
    }
    bail!("no version_overrides entry of the registry covers {tag}")
}

fn layer(base: &Def, o: &Def) -> Def {
    Def {
        kind: o.kind.clone().or(base.kind.clone()),
        version_constraint: o.version_constraint.clone(),
        version_prefix: o.version_prefix.clone().or(base.version_prefix.clone()),
        asset: o.asset.clone().or(base.asset.clone()),
        url: o.url.clone().or(base.url.clone()),
        version_source: o.version_source.clone().or(base.version_source.clone()),
        format: o.format.clone().or(base.format.clone()),
        format_overrides: if o.format_overrides.is_empty() {
            base.format_overrides.clone()
        } else {
            o.format_overrides.clone()
        },
        replacements: o.replacements.clone().or(base.replacements.clone()),
        overrides: if o.overrides.is_empty() { base.overrides.clone() } else { o.overrides.clone() },
        files: o.files.clone().or(base.files.clone()),
        supported_envs: if o.supported_envs.is_empty() {
            base.supported_envs.clone()
        } else {
            o.supported_envs.clone()
        },
    }
}

fn supported(envs: &[String], p: Platform) -> bool {
    envs.is_empty()
        || envs.iter().any(|e| e == "all" || *e == p.goos || *e == p.goarch || *e == format!("{}/{}", p.goos, p.goarch))
}

/// A definition with the platform's `overrides` and `format_overrides` applied.
struct Resolved {
    asset: Option<String>,
    url: Option<String>,
    format: String,
    replacements: BTreeMap<String, String>,
    files: Option<Vec<FileSpec>>,
}

fn for_platform(def: &Def, p: Platform) -> Resolved {
    let mut r = Resolved {
        asset: def.asset.clone(),
        url: def.url.clone(),
        format: def.format.clone().unwrap_or_default(),
        replacements: def.replacements.clone().unwrap_or_default(),
        files: def.files.clone(),
    };
    if let Some(f) = def.format_overrides.iter().find(|f| f.goos == p.goos) {
        r.format = f.format.clone();
    }
    for o in &def.overrides {
        if o.goos.as_deref().is_some_and(|g| g != p.goos) || o.goarch.as_deref().is_some_and(|g| g != p.goarch) {
            continue;
        }
        if o.goos.is_none() && o.goarch.is_none() {
            continue;
        }
        r.asset = o.asset.clone().or(r.asset);
        r.url = o.url.clone().or(r.url);
        r.format = o.format.clone().unwrap_or(r.format);
        r.replacements.extend(o.replacements.clone().unwrap_or_default());
        r.files = o.files.clone().or(r.files);
    }
    r
}

struct Vars {
    version: String,
    semver: String,
    os: String,
    arch: String,
    format: String,
    asset: Option<String>,
}

impl Vars {
    fn new(tag: &str, prefix: Option<&str>, p: Platform, r: &Resolved) -> Vars {
        let rep = |k: &str| r.replacements.get(k).cloned().unwrap_or_else(|| k.to_string());
        Vars {
            version: tag.to_string(),
            semver: prefix.and_then(|x| tag.strip_prefix(x)).unwrap_or(tag).to_string(),
            os: rep(p.goos),
            arch: rep(p.goarch),
            format: rep(&r.format),
            asset: None,
        }
    }

    fn with_asset(mut self, name: &str) -> Vars {
        self.asset = Some(name.to_string());
        self
    }
}

/// Go template subset: `{{.Field}}`, function calls (`trimV .Version`), and `|` pipelines.
fn expand(tpl: &str, v: &Vars) -> Result<String> {
    let mut out = String::new();
    let mut rest = tpl;
    while let Some(i) = rest.find("{{") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let j = after.find("}}").ok_or_else(|| anyhow!("unclosed `{{{{` in template `{tpl}`"))?;
        out.push_str(&pipeline(after[..j].trim().trim_start_matches('-').trim_end_matches('-').trim(), v)?);
        rest = &after[j + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

fn pipeline(expr: &str, v: &Vars) -> Result<String> {
    let mut piped: Option<String> = None;
    for cmd in split_top(expr, '|') {
        let words = words(cmd.trim())?;
        let (head, args) = words.split_first().ok_or_else(|| anyhow!("empty template action"))?;
        let mut vals = Vec::new();
        for a in args {
            vals.push(operand(a, v)?);
        }
        if let Some(p) = piped.take() {
            vals.push(p);
        }
        piped = Some(if head.starts_with('.') || head.starts_with('"') {
            if !vals.is_empty() {
                bail!("`{head}` takes no arguments in template `{expr}`");
            }
            operand(head, v)?
        } else {
            call(head, &vals)?
        });
    }
    piped.ok_or_else(|| anyhow!("empty template action"))
}

fn split_top(s: &str, sep: char) -> Vec<&str> {
    let (mut parts, mut start, mut quoted) = (Vec::new(), 0, false);
    for (i, c) in s.char_indices() {
        if c == '"' {
            quoted = !quoted;
        } else if c == sep && !quoted {
            parts.push(&s[start..i]);
            start = i + 1;
        }
    }
    parts.push(&s[start..]);
    parts
}

fn words(s: &str) -> Result<Vec<String>> {
    let (mut out, mut cur, mut quoted) = (Vec::new(), String::new(), false);
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                cur.push(c);
            }
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if quoted {
        bail!("unterminated string in template action `{s}`");
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    Ok(out)
}

fn operand(a: &str, v: &Vars) -> Result<String> {
    if let Some(lit) = a.strip_prefix('"').and_then(|x| x.strip_suffix('"')) {
        return Ok(lit.to_string());
    }
    match a {
        ".Version" => Ok(v.version.clone()),
        ".SemVer" => Ok(v.semver.clone()),
        ".OS" => Ok(v.os.clone()),
        ".Arch" => Ok(v.arch.clone()),
        ".Format" => Ok(v.format.clone()),
        ".Asset" => v.asset.clone().ok_or_else(|| anyhow!(".Asset is not available here")),
        ".AssetWithoutExt" => {
            let a = v.asset.as_deref().ok_or_else(|| anyhow!(".AssetWithoutExt is not available here"))?;
            let stem = [".tar.gz", ".tar.xz", ".tar.zst", ".tar.bz2", ".tgz", ".zip", ".gz", ".xz", ".bz2"]
                .iter()
                .find_map(|e| a.strip_suffix(e))
                .unwrap_or(a);
            Ok(stem.to_string())
        }
        other => bail!("unsupported template operand `{other}`"),
    }
}

fn call(f: &str, a: &[String]) -> Result<String> {
    let arity =
        |n: usize| if a.len() == n { Ok(()) } else { Err(anyhow!("`{f}` takes {n} arguments, got {}", a.len())) };
    Ok(match f {
        "trimV" => {
            arity(1)?;
            a[0].strip_prefix('v').unwrap_or(&a[0]).to_string()
        }
        "trimPrefix" => {
            arity(2)?;
            a[1].strip_prefix(a[0].as_str()).unwrap_or(&a[1]).to_string()
        }
        "trimSuffix" => {
            arity(2)?;
            a[1].strip_suffix(a[0].as_str()).unwrap_or(&a[1]).to_string()
        }
        "replace" => {
            arity(3)?;
            a[2].replace(a[0].as_str(), &a[1])
        }
        "title" => {
            arity(1)?;
            let mut c = a[0].chars();
            c.next().map(|h| h.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
        }
        "toLower" => {
            arity(1)?;
            a[0].to_lowercase()
        }
        "toUpper" => {
            arity(1)?;
            a[0].to_uppercase()
        }
        other => bail!("unsupported template function `{other}`"),
    })
}

// Version constraints: `true`, `false`, `semver("<= 1.2, > 0.5")`, `Version == "v1"`,
// `Version != "v1"`, `Version startsWith "v1"`, `Version in ["v1", "v2"]`, the same on `SemVer`
// (the version without its `version_prefix`), with `or` and `and` as words too, and `!`, `&&`, `||` and parentheses.

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Str(String),
    Ident(String),
    Op(&'static str),
}

fn lex(s: &str) -> Result<Vec<Tok>> {
    let cs: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < cs.len() {
        let c = cs[i];
        let two: String = cs[i..(i + 2).min(cs.len())].iter().collect();
        if c.is_whitespace() {
            i += 1;
        } else if c == '"' || c == '\'' {
            let end = cs[i + 1..].iter().position(|x| *x == c).ok_or_else(|| anyhow!("unterminated string"))?;
            out.push(Tok::Str(cs[i + 1..i + 1 + end].iter().collect()));
            i += end + 2;
        } else if let Some(op) = ["&&", "||", "==", "!="].into_iter().find(|o| *o == two) {
            out.push(Tok::Op(op));
            i += 2;
        } else if let Some(op) = ["(", ")", "!", "[", "]", ","].into_iter().find(|o| o.starts_with(c)) {
            out.push(Tok::Op(op));
            i += 1;
        } else if c.is_alphanumeric() || c == '_' {
            let end = cs[i..].iter().position(|x| !(x.is_alphanumeric() || *x == '_')).unwrap_or(cs.len() - i);
            out.push(Tok::Ident(cs[i..i + end].iter().collect()));
            i += end;
        } else {
            bail!("unexpected `{c}`");
        }
    }
    Ok(out)
}

struct Parser<'a> {
    toks: Vec<Tok>,
    at: usize,
    version: &'a str,
    semver: &'a str,
}

pub fn eval_constraint(expr: &str, version: &str, semver: &str) -> Result<bool> {
    let mut p = Parser { toks: lex(expr)?, at: 0, version, semver };
    let v = p.or()?;
    if p.at != p.toks.len() {
        bail!("trailing input");
    }
    Ok(v)
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.at)
    }

    fn eat(&mut self, op: &str) -> bool {
        let word = match op {
            "||" => Some("or"),
            "&&" => Some("and"),
            "!" => Some("not"),
            _ => None,
        };
        let hit = match self.peek() {
            Some(Tok::Op(o)) => *o == op,
            Some(Tok::Ident(w)) => Some(w.as_str()) == word,
            _ => false,
        };
        if hit {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn or(&mut self) -> Result<bool> {
        let mut v = self.and()?;
        while self.eat("||") {
            let r = self.and()?;
            v = v || r;
        }
        Ok(v)
    }

    fn and(&mut self) -> Result<bool> {
        let mut v = self.unary()?;
        while self.eat("&&") {
            let r = self.unary()?;
            v = v && r;
        }
        Ok(v)
    }

    fn unary(&mut self) -> Result<bool> {
        if self.eat("!") {
            return Ok(!self.unary()?);
        }
        if self.eat("(") {
            let v = self.or()?;
            if !self.eat(")") {
                bail!("missing `)`");
            }
            return Ok(v);
        }
        self.atom()
    }

    fn atom(&mut self) -> Result<bool> {
        let Some(Tok::Ident(id)) = self.peek().cloned() else { bail!("expected a term") };
        self.at += 1;
        match id.as_str() {
            "true" => Ok(true),
            "false" => Ok(false),
            "semver" => {
                if !self.eat("(") {
                    bail!("semver needs `(`");
                }
                let Some(Tok::Str(c)) = self.peek().cloned() else { bail!("semver needs a string") };
                self.at += 1;
                if !self.eat(")") {
                    bail!("missing `)`");
                }
                semver_matches(&c, self.semver)
            }
            "Version" | "SemVer" => {
                let subject = if id == "Version" { self.version } else { self.semver };
                let op = self.peek().cloned();
                self.at += 1;
                if op == Some(Tok::Ident("in".into())) {
                    return self.list().map(|l| l.iter().any(|v| v == subject));
                }
                let Some(Tok::Str(rhs)) = self.peek().cloned() else { bail!("expected a string after Version") };
                self.at += 1;
                match op {
                    Some(Tok::Op("==")) => Ok(subject == rhs),
                    Some(Tok::Op("!=")) => Ok(subject != rhs),
                    Some(Tok::Ident(w)) if w == "startsWith" => Ok(subject.starts_with(&rhs)),
                    Some(Tok::Ident(w)) if w == "endsWith" => Ok(subject.ends_with(&rhs)),
                    Some(Tok::Ident(w)) if w == "contains" => Ok(subject.contains(&rhs)),
                    _ => bail!("unsupported operator after Version"),
                }
            }
            other => bail!("unsupported term `{other}`"),
        }
    }

    fn list(&mut self) -> Result<Vec<String>> {
        if !self.eat("[") {
            bail!("`in` needs a list");
        }
        let mut out = Vec::new();
        while !self.eat("]") {
            let Some(Tok::Str(v)) = self.peek().cloned() else { bail!("expected a string in the list") };
            self.at += 1;
            out.push(v);
            if !self.eat(",") && !matches!(self.peek(), Some(Tok::Op("]"))) {
                bail!("expected `,` or `]`");
            }
        }
        Ok(out)
    }
}

fn parse_ver(v: &str) -> Vec<u64> {
    trim_v(v)
        .split(|c: char| !c.is_ascii_digit())
        .take_while(|p| !p.is_empty())
        .filter_map(|p| p.parse().ok())
        .collect()
}

fn cmp_ver(a: &[u64], b: &[u64]) -> std::cmp::Ordering {
    let n = a.len().max(b.len());
    (0..n).map(|i| a.get(i).unwrap_or(&0)).cmp((0..n).map(|i| b.get(i).unwrap_or(&0)))
}

fn semver_matches(constraint: &str, version: &str) -> Result<bool> {
    let have = parse_ver(version);
    for part in constraint.split(',') {
        let part = part.trim();
        let (op, rhs) = ["<=", ">=", "!=", "==", "<", ">", "="]
            .iter()
            .find_map(|o| part.strip_prefix(o).map(|r| (*o, r.trim())))
            .unwrap_or(("=", part));
        if rhs.starts_with(['~', '^']) {
            bail!("unsupported semver operator in `{part}`");
        }
        let ord = cmp_ver(&have, &parse_ver(rhs));
        let ok = match op {
            "<=" => ord.is_le(),
            ">=" => ord.is_ge(),
            "<" => ord.is_lt(),
            ">" => ord.is_gt(),
            "!=" => ord.is_ne(),
            _ => ord.is_eq(),
        };
        if !ok {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RG: &str = include_str!("../tests/fixtures/aqua-registry.yaml");

    fn vars(tag: &str, plat: &str, def: &Def) -> (Vars, Resolved) {
        let r = for_platform(def, Platform::parse(plat).unwrap());
        (Vars::new(tag, None, Platform::parse(plat).unwrap(), &r), r)
    }

    #[test]
    fn ripgrep_like_entry() {
        let reg = Registry::parse(RG.as_bytes()).unwrap();
        let e = reg.find("ripgrep").unwrap();
        assert_eq!(e.repo().unwrap(), "BurntSushi/ripgrep");
        let def = effective(e, "15.1.0").unwrap();
        let (v, r) = vars("15.1.0", "aarch64-darwin", &def);
        assert_eq!(expand(r.asset.as_deref().unwrap(), &v).unwrap(), "ripgrep-15.1.0-aarch64-apple-darwin.tar.gz");
        let (v, r) = vars("15.1.0", "x86_64-linux", &def);
        assert_eq!(expand(r.asset.as_deref().unwrap(), &v).unwrap(), "ripgrep-15.1.0-x86_64-unknown-linux-musl.tar.gz");
        let (v, r) = vars("15.1.0", "aarch64-linux", &def);
        let name = expand(r.asset.as_deref().unwrap(), &v).unwrap();
        assert_eq!(name, "ripgrep-15.1.0-aarch64-unknown-linux-gnu.tar.gz");
        let src = r.files.as_ref().unwrap()[0].src.as_deref().unwrap();
        assert_eq!(expand(src, &v.with_asset(&name)).unwrap(), "ripgrep-15.1.0-aarch64-unknown-linux-gnu/rg");
        let old = effective(e, "0.8.0").unwrap();
        assert!(old.files.is_none());
    }

    #[test]
    fn gh_like_entry_with_aliases_and_format_overrides() {
        let reg = Registry::parse(RG.as_bytes()).unwrap();
        let e = reg.find("github/gh").unwrap();
        let def = effective(e, "v2.65.0").unwrap();
        let (v, r) = vars("v2.65.0", "aarch64-darwin", &def);
        assert_eq!(expand(r.asset.as_deref().unwrap(), &v).unwrap(), "gh_2.65.0_macOS_arm64.zip");
        let (v, r) = vars("v2.65.0", "x86_64-linux", &def);
        assert_eq!(expand(r.asset.as_deref().unwrap(), &v).unwrap(), "gh_2.65.0_linux_amd64.tar.gz");
        assert!(reg.find("cli/cli").is_ok());
        assert!(reg.find("nope").is_err());
        assert_eq!(reg.find("fd").unwrap().repo().unwrap(), "sharkdp/fd");
    }

    #[test]
    fn a_name_beats_a_shared_repo() {
        let yaml = r#"
packages:
  - type: github_release
    repo_owner: tamasfe
    repo_name: taplo
    asset: taplo-{{.OS}}-{{.Arch}}.gz
  - type: github_release
    repo_owner: tamasfe
    repo_name: taplo
    name: tamasfe/taplo/full
    asset: taplo-full-{{.OS}}-{{.Arch}}.gz
"#;
        let reg = Registry::parse(yaml.as_bytes()).unwrap();
        assert_eq!(reg.find("tamasfe/taplo").unwrap().def.asset.as_deref(), Some("taplo-{{.OS}}-{{.Arch}}.gz"));
        assert_eq!(reg.find("tamasfe/taplo/full").unwrap().name.as_deref(), Some("tamasfe/taplo/full"));
    }

    #[test]
    fn constraints_and_pipelines() {
        assert!(eval_constraint(r#"semver(">= 1.0, < 2")"#, "v1.5.0", "v1.5.0").unwrap());
        assert!(!eval_constraint(r#"semver(">= 1.0, < 2")"#, "2.0.0", "2.0.0").unwrap());
        assert!(eval_constraint(r#"Version == "v1" || Version startsWith "v2""#, "v2.3", "v2.3").unwrap());
        assert!(eval_constraint(r#"!(Version == "v1") && true"#, "v2", "v2").unwrap());
        assert!(eval_constraint(r#"Version in ["v0.5.1", "v0.6.0"]"#, "v0.6.0", "v0.6.0").unwrap());
        assert!(!eval_constraint(r#"Version in ["v0.5.1"] && true"#, "v0.6.0", "v0.6.0").unwrap());
        let nextest = r#"SemVer in ["0.9.65", "0.9.69"] or semver("> 0.9.59, <= 0.9.61-rc.1")"#;
        assert!(eval_constraint(nextest, "cargo-nextest-0.9.60", "0.9.60").unwrap());
        assert!(!eval_constraint(nextest, "cargo-nextest-0.9.100", "0.9.100").unwrap());
        assert!(eval_constraint(r#"true and not_a_word"#, "v1", "1").is_err());
        let taplo = r#"not (Version startsWith "release-taplo-cli-") and not (Version startsWith "release-cli-") and semver(">= 0.8.0")"#;
        assert!(eval_constraint(taplo, "0.10.0", "0.10.0").unwrap());
        assert!(!eval_constraint(taplo, "release-taplo-cli-0.7.0", "0.7.0").unwrap());
        assert!(!eval_constraint(r#"not Version startsWith "v1""#, "v1.2", "1.2").unwrap());
        let v = Vars {
            version: "v1.2.3".into(),
            semver: "1.2.3".into(),
            os: "linux".into(),
            arch: "x".into(),
            format: String::new(),
            asset: None,
        };
        assert_eq!(
            expand("{{.Version | trimV}}-{{title .OS}}-{{trimPrefix \"v\" .Version}}", &v).unwrap(),
            "1.2.3-Linux-1.2.3"
        );
        assert!(expand("{{if .OS}}x{{end}}", &v).is_err());
    }
}
