//! Evaluates `spec.star` into hosts, and a project's `dev.star` into its environment. Builtins return opaque `resource` values carrying their JSON
//! form, validated against the `fspec-core` types as soon as they are constructed, so a typo
//! fails at the line that made it.

pub mod remote;

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};

use allocative::Allocative;
use anyhow::{Context, anyhow, bail};
use fspec_core::dev::Dev;
use fspec_core::engine;
use fspec_core::facts::{Facts, Os};
use fspec_core::fetch::{Fetch, LiveFetch};
use fspec_core::lock::{Lock, LockedLoad, SHIPPED_LOADS, loads_cache};
use fspec_core::resource::{Host, Resource, Source, System};
use fspec_core::runner::Runner;
use serde::de::DeserializeOwned;
use serde::{Serialize, Serializer};
use serde_json::{Map, Value as Json};
use starlark::environment::{FrozenModule, Globals, GlobalsBuilder, LibraryExtension, Module};
use starlark::eval::{Evaluator, FileLoader};
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::syntax::{AstModule, Dialect};
use starlark::values::dict::DictRef;
use starlark::values::list::ListRef;
use starlark::values::structs::AllocStruct;
use starlark::values::tuple::{TupleRef, UnpackTuple};
use starlark::values::{Heap, ProvidesStaticType, StarlarkValue, Value, starlark_value};

/// The result of evaluating a spec: every host it declares, modules expanded.
#[derive(Debug)]
pub struct Spec {
    pub hosts: Vec<Host>,
    /// Each `gh:` load source the evaluation read, at the commit it read it at.
    pub loads: BTreeMap<String, LockedLoad>,
}

impl Spec {
    pub fn host(&self, name: &str) -> anyhow::Result<&Host> {
        self.hosts.iter().find(|h| h.name == name).ok_or_else(|| {
            let names: Vec<_> = self.hosts.iter().map(|h| h.name.as_str()).collect();
            anyhow!("no host named `{name}` in the spec (hosts: {})", names.join(", "))
        })
    }
}

/// Evaluate `spec_path`. `load("//x.star", ...)` resolves against the spec's directory, and
/// `load("gh:owner/repo@ref//x.star", ...)` at the commit `fspec.lock` pins for it.
pub fn evaluate(spec_path: &Path) -> anyhow::Result<Spec> {
    let root = spec_path.parent().map(Path::to_path_buf).unwrap_or_default();
    let lock = Lock::load(&root.join("fspec.lock"))?;
    let fetch = LiveFetch::new();
    evaluate_with(spec_path, remote::Mode::Locked(&lock.loads), &fetch, loads_cache())
}

/// Evaluate for `fspec update`: each `gh:` ref resolves to its current commit, or with `keep`
/// stays at the commit `locked` has for it.
pub fn evaluate_resolving(
    spec_path: &Path,
    runner: &dyn Runner,
    fetch: &dyn Fetch,
    locked: &BTreeMap<String, LockedLoad>,
    keep: bool,
) -> anyhow::Result<Spec> {
    evaluate_with(spec_path, remote::Mode::Resolve { runner, locked, keep }, fetch, loads_cache())
}

/// `cache` is where fetched `gh:` load files are kept between runs.
pub fn evaluate_with(
    spec_path: &Path,
    mode: remote::Mode,
    fetch: &dyn Fetch,
    cache: Option<PathBuf>,
) -> anyhow::Result<Spec> {
    let root = spec_path.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut dirs = vec![root.join(SHIPPED_LOADS)];
    dirs.extend(cache);
    let remote = remote::Remote { mode, fetch, dirs, loaded: RefCell::default() };
    let globals = globals();
    let (cache, loading) = (RefCell::default(), RefCell::default());
    let loader = Loader {
        root: SpecRoot { dir: root, dev: None, origin: None },
        globals: &globals,
        cache: &cache,
        loading: &loading,
        remote: Some(&remote),
    };
    let text = std::fs::read_to_string(spec_path).with_context(|| format!("reading {}", spec_path.display()))?;
    let ast = AstModule::parse(&spec_path.display().to_string(), text, &dialect()).map_err(|e| e.into_anyhow())?;

    Module::with_temp_heap(|module| {
        let mut eval = Evaluator::new(&module);
        eval.set_loader(&loader);
        eval.extra = Some(&loader.root);
        eval.eval_module(ast, &globals).map_err(|e| e.into_anyhow())?;
        let hosts = module.get("hosts").context("spec.star must set `hosts` to a list of host(...)")?;
        let list = ListRef::from_value(hosts).context("`hosts` must be a list")?;
        let hosts = list
            .iter()
            .map(|v| match Resource_::from_value(v) {
                Some(r) if r.kind == "host" => Ok(serde_json::from_value(r.data.clone())?),
                _ => bail!("`hosts` entries must be host(...), got {}", v.get_type()),
            })
            .collect::<anyhow::Result<Vec<Host>>>()?;
        let mut seen = std::collections::HashSet::new();
        for h in &hosts {
            if !seen.insert(&h.name) {
                bail!("host `{}` is declared more than once", h.name);
            }
        }
        Ok(Spec { hosts, loads: remote.loaded.take() })
    })
}

/// Evaluate `secrets.star`, the counterpart of agenix's `secrets.nix`: it sets `secrets` to a dict
/// from each `.age` file, relative to its own directory, to the public keys it is encrypted to.
pub fn evaluate_recipients(path: &Path) -> anyhow::Result<std::collections::BTreeMap<String, Vec<String>>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let ast = AstModule::parse(&path.display().to_string(), text, &dialect()).map_err(|e| e.into_anyhow())?;
    let globals = GlobalsBuilder::extended_by(&[LibraryExtension::StructType, LibraryExtension::Print]).build();
    Module::with_temp_heap(|module| {
        Evaluator::new(&module).eval_module(ast, &globals).map_err(|e| e.into_anyhow())?;
        let secrets = module.get("secrets").with_context(|| format!("{} must set `secrets`", path.display()))?;
        serde_json::from_value(secrets.to_json_value()?)
            .with_context(|| format!("`secrets` in {} must map file names to lists of public keys", path.display()))
    })
}

/// Evaluate a project's `dev.star` as seen from `platform` (`aarch64-darwin`), which the file
/// reads as the global `platform`. The file calls `dev(...)` exactly once.
pub fn evaluate_dev(path: &Path, platform: &str) -> anyhow::Result<Dev> {
    let root = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let (arch, os) = match platform.split_once('-') {
        Some((arch @ ("x86_64" | "aarch64"), os @ ("linux" | "darwin"))) => (arch, os),
        _ => bail!("platform `{platform}` is not one of {}", fspec_core::dev::PLATFORMS.join(", ")),
    };
    let globals = globals();
    let (cache, loading) = (RefCell::default(), RefCell::default());
    let loader = Loader {
        root: SpecRoot { dir: root, dev: Some(RefCell::default()), origin: None },
        globals: &globals,
        cache: &cache,
        loading: &loading,
        remote: None,
    };
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let ast = AstModule::parse(&path.display().to_string(), text, &dialect()).map_err(|e| e.into_anyhow())?;
    Module::with_temp_heap(|module| {
        module.set("platform", platform_struct(module.heap(), platform, os, arch));
        let mut eval = Evaluator::new(&module);
        eval.set_loader(&loader);
        eval.extra = Some(&loader.root);
        eval.eval_module(ast, &globals).map_err(|e| e.into_anyhow())?;
        anyhow::Ok(())
    })?;
    let declared = loader.root.dev.and_then(RefCell::into_inner);
    declared.with_context(|| format!("{} must call dev(...)", path.display()))
}

fn dialect() -> Dialect {
    Dialect::Extended
}

fn globals() -> Globals {
    GlobalsBuilder::extended_by(&[LibraryExtension::StructType, LibraryExtension::Json, LibraryExtension::Print])
        .with(builtins)
        .build()
}

/// The spec's directory, which `glob` reads and `load` resolves against. For a `dev.star`,
/// `dev` receives what its `dev(...)` call declared.
#[derive(ProvidesStaticType)]
struct SpecRoot {
    dir: PathBuf,
    dev: Option<RefCell<Option<Dev>>>,
    /// Set inside a file loaded from GitHub: its `//` loads resolve in the same repo, at the
    /// commit the lock has for the same ref.
    origin: Option<Origin>,
}

struct Origin {
    repo: String,
    reference: Option<String>,
}

struct Loader<'a> {
    root: SpecRoot,
    globals: &'a Globals,
    cache: &'a RefCell<HashMap<String, FrozenModule>>,
    loading: &'a RefCell<Vec<String>>,
    remote: Option<&'a remote::Remote<'a>>,
}

impl Loader<'_> {
    /// A `gh:` load, or a `//` load inside a file that came from GitHub: the file at the
    /// locked commit, evaluated with that repo as its origin.
    fn load_remote(&self, path: &str, t: &remote::Target) -> anyhow::Result<(String, String, SpecRoot)> {
        let remote = self.remote.with_context(|| format!("load(\"{path}\"): GitHub loads belong in spec.star"))?;
        let rev = remote.rev(t)?;
        let text = remote.read(t, &rev, t.path)?;
        let origin = Origin { repo: t.repo.into(), reference: t.reference.map(Into::into) };
        let root = SpecRoot { dir: self.root.dir.clone(), dev: None, origin: Some(origin) };
        Ok((format!("gh:{}@{rev}//{}", t.repo, t.path), text, root))
    }
}

impl FileLoader for Loader<'_> {
    fn load(&self, path: &str) -> starlark::Result<FrozenModule> {
        if let Some(m) = self.cache.borrow().get(path) {
            return Ok(m.clone());
        }
        let gh = remote::parse(path).map_err(other)?;
        let inner = match (&self.root.origin, path.strip_prefix("//")) {
            (Some(o), Some(rel)) => {
                remote::parse(&format!("gh:{}//{rel}", o.repo)).map_err(other)?;
                Some(remote::Target { repo: &o.repo, reference: o.reference.as_deref(), path: rel })
            }
            _ => None,
        };
        let (name, text, child) = match gh.or(inner) {
            Some(t) => self.load_remote(path, &t).map_err(other)?,
            None => {
                let Some(rel) = path.strip_prefix("//") else {
                    return Err(other(anyhow!(
                        "load(\"{path}\"): paths start with // and are relative to the spec's directory, or gh:owner/repo@ref//path"
                    )));
                };
                let file = self.root.dir.join(rel);
                let text =
                    std::fs::read_to_string(&file).with_context(|| format!("load(\"{path}\")")).map_err(other)?;
                (path.to_string(), text, SpecRoot { dir: self.root.dir.clone(), dev: None, origin: None })
            }
        };
        if let Some(m) = self.cache.borrow().get(&name) {
            return Ok(m.clone());
        }
        if self.loading.borrow().contains(&name) {
            return Err(other(anyhow!("load cycle: {} -> {name}", self.loading.borrow().join(" -> "))));
        }
        let ast = AstModule::parse(&name, text, &dialect())?;
        let loader = Loader {
            root: child,
            globals: self.globals,
            cache: self.cache,
            loading: self.loading,
            remote: self.remote,
        };
        self.loading.borrow_mut().push(name.clone());
        let frozen = Module::with_temp_heap(|module| {
            let mut eval = Evaluator::new(&module);
            eval.set_loader(&loader);
            eval.extra = Some(&loader.root);
            eval.eval_module(ast, self.globals)?;
            drop(eval);
            module.freeze().map_err(|e| other(anyhow!("{e:?}")))
        });
        self.loading.borrow_mut().pop();
        let frozen = frozen?;
        self.cache.borrow_mut().insert(name, frozen.clone());
        Ok(frozen)
    }
}

fn other(e: anyhow::Error) -> starlark::Error {
    starlark::Error::new_other(e)
}

/// What every builtin returns. `data` is the validated JSON form, `kind` included.
#[derive(Debug, Clone, ProvidesStaticType, Allocative)]
struct Resource_ {
    kind: String,
    #[allocative(skip)]
    data: Json,
}

starlark_simple_value!(Resource_);

impl fmt::Display for Resource_ {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}({})", self.kind, self.data)
    }
}

#[starlark_value(type = "resource")]
impl<'v> StarlarkValue<'v> for Resource_ {
    fn collect_repr(&self, collector: &mut String) {
        collector.push_str(&self.to_string());
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        self.data.get(attribute).map(|v| heap.alloc(v.clone()))
    }

    fn has_attr(&self, attribute: &str, _heap: Heap<'v>) -> bool {
        self.data.get(attribute).is_some()
    }

    fn write_hash(&self, hasher: &mut starlark::collections::StarlarkHasher) -> starlark::Result<()> {
        use std::hash::Hash;
        self.data.to_string().hash(hasher);
        Ok(())
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(Resource_::from_value(other).is_some_and(|o| o.data == self.data))
    }
}

/// Lets `to_json_value` descend into lists and dicts that hold resources.
impl Serialize for Resource_ {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.data.serialize(s)
    }
}

/// Map positional and keyword arguments onto field names, as JSON.
fn fields<'v>(what: &str, names: &[&str], args: &[Value<'v>], kwargs: Value<'v>) -> anyhow::Result<Map<String, Json>> {
    if args.len() > names.len() {
        bail!("{what}() takes at most {} positional argument(s), got {}", names.len(), args.len());
    }
    let mut map = Map::new();
    for (name, v) in names.iter().zip(args) {
        map.insert((*name).into(), to_json(what, name, *v)?);
    }
    if let Some(kw) = DictRef::from_value(kwargs) {
        for (k, v) in kw.iter() {
            let k = k.unpack_str().context("keyword names are strings")?.to_string();
            if map.contains_key(&k) {
                bail!("{what}() got `{k}` both positionally and by keyword");
            }
            let json = to_json(what, &k, v)?;
            map.insert(k, json);
        }
    }
    map.retain(|_, v| !v.is_null());
    Ok(map)
}

fn to_json(what: &str, field: &str, v: Value) -> anyhow::Result<Json> {
    v.to_json_value().map_err(|e| anyhow!("{what}({field} = ...): {e}"))
}

/// Validate `map` as `T`, then keep the normalized form (defaults filled in).
fn validated<T: DeserializeOwned + Serialize>(what: &str, map: Map<String, Json>) -> anyhow::Result<Json> {
    let typed: T = serde_json::from_value(Json::Object(map)).map_err(|e| anyhow!("{what}(): {e}"))?;
    Ok(serde_json::to_value(typed)?)
}

fn make<'v, T: DeserializeOwned + Serialize>(
    kind: &str,
    names: &[&str],
    args: UnpackTuple<Value<'v>>,
    kwargs: Value<'v>,
) -> anyhow::Result<Resource_> {
    let mut map = fields(kind, names, &args.items, kwargs)?;
    map.insert("kind".into(), kind.into());
    Ok(Resource_ { kind: kind.into(), data: validated::<T>(kind, map)? })
}

/// Like [`make`] for a type without a `kind` tag: the tag is added after validation, for
/// [`untag`] to check.
fn make_untagged<'v, T: DeserializeOwned + Serialize>(
    kind: &str,
    names: &[&str],
    args: UnpackTuple<Value<'v>>,
    kwargs: Value<'v>,
) -> anyhow::Result<Resource_> {
    let mut data = validated::<T>(kind, fields(kind, names, &args.items, kwargs)?)?;
    if let Some(obj) = data.as_object_mut() {
        obj.insert("kind".into(), kind.into());
    }
    Ok(Resource_ { kind: kind.into(), data })
}

fn sync_<'v>(args: UnpackTuple<Value<'v>>, kwargs: Value<'v>) -> anyhow::Result<Resource_> {
    let mut map = fields("sync", &["name"], &args.items, kwargs)?;
    let name = map.get("name").and_then(Json::as_str).context("sync() needs a name")?.to_string();
    let what = format!("sync({name})");
    if let Some(keyring) = map.get_mut("keyring") {
        untag(&what, "keyring", "keyring", keyring)?;
    }
    if let Some(keys) = map.get_mut("keys") {
        let mut list = Json::Array(vec![keys.take()]);
        untag(&what, "keys", "json_keys", &mut list)?;
        *keys = list.as_array_mut().and_then(|l| l.pop()).unwrap_or_default();
    }
    let mut data = validated::<fspec_core::resource::SyncItem>(&what, map)?;
    serde_json::from_value::<fspec_core::resource::SyncItem>(data.clone())?.check()?;
    if let Some(obj) = data.as_object_mut() {
        obj.insert("kind".into(), "sync".into());
    }
    Ok(Resource_ { kind: "sync".into(), data })
}

/// `lib(...)` and `bin(...)`: a name plus a package name per manager the lookup cannot resolve.
fn native_<'v>(kind: &str, args: UnpackTuple<Value<'v>>, kwargs: Value<'v>) -> anyhow::Result<Resource_> {
    let map = fields(kind, &["name"], &args.items, kwargs)?;
    for (k, v) in &map {
        let known = k == "name" || fspec_core::dev::MANAGERS.contains(&k.as_str());
        if !known {
            bail!("{kind}(): unknown field `{k}`, expected `name` or one of {}", fspec_core::dev::MANAGERS.join(", "));
        }
        if !v.is_string() {
            bail!("{kind}({k} = ...) must be a string");
        }
    }
    make_untagged::<fspec_core::dev::Native>(kind, &["name"], args, kwargs)
}

/// A struct-typed field (pkg files, host users) holds `kind`-tagged JSON; check and drop the tag.
fn untag(what: &str, field: &str, kind: &str, v: &mut Json) -> anyhow::Result<()> {
    let items: Vec<&mut Json> = match v {
        Json::Array(items) => items.iter_mut().collect(),
        _ => bail!("{what}({field} = ...) must be a list"),
    };
    for item in items {
        let obj = item.as_object_mut().with_context(|| format!("{what}({field} = ...) takes {kind}(...) values"))?;
        match obj.remove("kind") {
            Some(Json::String(k)) if k == kind => {}
            Some(k) => bail!("{what}({field} = ...) takes {kind}(...) values, got {k}"),
            None => bail!("{what}({field} = ...) takes {kind}(...) values"),
        }
    }
    Ok(())
}

fn pkg_<'v>(args: UnpackTuple<Value<'v>>, kwargs: Value<'v>) -> anyhow::Result<Resource_> {
    let mut map = fields("pkg", &["name"], &args.items, kwargs)?;
    let name = map.get("name").and_then(Json::as_str).context("pkg() needs a name")?.to_string();
    // The aqua registry has no CPython, so python comes from python-build-standalone.
    let default = if name == "python" { Source::Python } else { Source::Aqua { name } };
    map.entry("source").or_insert_with(|| serde_json::to_value(default).unwrap());
    pkg_from(map)
}

/// `pypi("yt-dlp")`: a pkg whose source is the PyPI distribution of the same name.
fn pypi_<'v>(args: UnpackTuple<Value<'v>>, kwargs: Value<'v>) -> anyhow::Result<Resource_> {
    let mut map = fields("pypi", &["name"], &args.items, kwargs)?;
    let name = map.get("name").and_then(Json::as_str).context("pypi() needs a name")?.to_string();
    if map.contains_key("source") {
        bail!("pypi() takes no source");
    }
    map.insert("source".into(), serde_json::to_value(Source::Pypi { name }).unwrap());
    pkg_from(map)
}

fn pkg_from(mut map: serde_json::Map<String, Json>) -> anyhow::Result<Resource_> {
    for (field, kind) in [("files", "file"), ("services", "service"), ("timers", "timer")] {
        if let Some(v) = map.get_mut(field) {
            untag("pkg", field, kind, v)?;
        }
    }
    map.insert("kind".into(), "pkg".into());
    Ok(Resource_ { kind: "pkg".into(), data: validated::<Resource>("pkg", map)? })
}

fn distro_(distro: &str, default_init: &str, kwargs: Value) -> anyhow::Result<Resource_> {
    let mut map = fields(distro, &[], &[], kwargs)?;
    map.insert("kind".into(), "distro".into());
    map.insert("distro".into(), distro.into());
    map.entry("init").or_insert_with(|| default_init.into());
    Ok(Resource_ { kind: "distro".into(), data: validated::<System>(distro, map)? })
}

fn host_<'v>(
    args: UnpackTuple<Value<'v>>,
    kwargs: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> anyhow::Result<Resource_> {
    let mut modules = Value::new_none();
    let mut plain = Vec::new();
    if let Some(kw) = DictRef::from_value(kwargs) {
        for (k, v) in kw.iter() {
            if k.unpack_str() == Some("modules") {
                modules = v;
            } else {
                plain.push((k, v));
            }
        }
    }
    let heap = eval.heap();
    let rest = heap.alloc(starlark::values::dict::AllocDict(plain));
    let mut map = fields("host", &["name"], &args.items, rest)?;
    let name = map.get("name").and_then(Json::as_str).context("host() needs a name")?.to_string();
    let what = format!("host({name})");
    let platform = map
        .get("platform")
        .and_then(Json::as_str)
        .with_context(|| format!("{what}: platform is required, e.g. platform = \"x86_64-linux\""))?
        .to_string();
    let system: Option<System> = match map.get("system") {
        Some(s) => Some(serde_json::from_value(s.clone()).map_err(|e| anyhow!("{what}: system: {e}"))?),
        None => None,
    };
    let facts = Facts::declared(&name, &platform, system.as_ref())?;
    if let Some(users) = map.get_mut("users") {
        untag(&what, "users", "user", users)?;
    }
    if let Some(builders) = map.get_mut("builders") {
        untag(&what, "builders", "builder", builders)?;
    }
    if let Some(sync) = map.get_mut("sync") {
        untag(&what, "sync", "sync", sync)?;
    }
    if let Some(vm) = map.get_mut("vm") {
        match vm.as_object_mut().and_then(|o| o.remove("kind")) {
            Some(Json::String(k)) if k == "vm" => {}
            _ => bail!("{what}(vm = ...) takes a vm(...) value"),
        }
    }

    let facts_value = facts_struct(heap, &facts);
    let mut resources = Vec::new();
    expand(&what, modules, facts_value, eval, &mut resources)?;
    // macOS hosts take fspec from their own package manager for now.
    let declares_engine = resources.iter().any(|r| {
        r.get("kind").and_then(Json::as_str) == Some("pkg")
            && r.get("name").and_then(Json::as_str) == Some(engine::NAME)
    });
    if facts.os == Os::Linux && !declares_engine {
        resources.push(serde_json::to_value(Resource::Pkg(engine::pkg()))?);
    }
    let (mut synced, resources): (Vec<Json>, Vec<Json>) =
        resources.into_iter().partition(|r| r.get("kind").and_then(Json::as_str) == Some("sync"));
    if !synced.is_empty() {
        for s in &mut synced {
            s.as_object_mut().map(|o| o.remove("kind"));
        }
        match map.entry("sync").or_insert_with(|| Json::Array(vec![])) {
            Json::Array(list) => list.extend(synced),
            _ => bail!("{what}(sync = ...) must be a list"),
        }
    }
    map.insert("resources".into(), Json::Array(resources));
    let data = validated::<Host>(&what, map)?;
    Ok(Resource_ { kind: "host".into(), data })
}

fn platform_struct<'v>(heap: Heap<'v>, name: &str, os: &str, arch: &str) -> Value<'v> {
    let is_darwin = os == "darwin";
    heap.alloc(AllocStruct([
        ("name", heap.alloc(name)),
        ("os", heap.alloc(os)),
        ("arch", heap.alloc(arch)),
        ("is_darwin", Value::new_bool(is_darwin)),
        ("is_linux", Value::new_bool(!is_darwin)),
    ]))
}

fn facts_struct<'v>(heap: Heap<'v>, f: &Facts) -> Value<'v> {
    let platform = platform_struct(heap, &f.platform(), &f.os.to_string(), &f.arch);
    heap.alloc(AllocStruct([
        ("name", heap.alloc(f.name.as_str())),
        ("platform", platform),
        ("distro", heap.alloc(f.distro.as_str())),
        ("init", heap.alloc(f.init.to_string())),
    ]))
}

/// Flatten a `modules` value: resources, lists of them, `None`, and functions called with the host.
fn expand<'v>(
    what: &str,
    v: Value<'v>,
    facts: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
    out: &mut Vec<Json>,
) -> anyhow::Result<()> {
    if v.is_none() {
        return Ok(());
    }
    if let Some(r) = Resource_::from_value(v) {
        match r.kind.as_str() {
            "pkg" | "file" | "service" | "timer" | "user" | "secret" | "defaults" | "setting" | "repo" | "sync" => {}
            "image" | "distro" => bail!("{what}: {}(...) goes in host(system = ...), not in modules", r.kind),
            kind => bail!("{what}: modules hold resources, got {kind}(...)"),
        }
        out.push(r.data.clone());
        return Ok(());
    }
    let items = ListRef::from_value(v).map(|l| l.content()).or_else(|| TupleRef::from_value(v).map(|t| t.content()));
    if let Some(items) = items {
        for item in items.iter().copied() {
            expand(what, item, facts, eval, out)?;
        }
        return Ok(());
    }
    if v.get_type() == "function" {
        let result = eval.eval_function(v, &[facts], &[]).map_err(|e| e.into_anyhow())?;
        return expand(what, result, facts, eval, out);
    }
    bail!("{what}: modules hold resources, lists or functions, got {}", v.get_type())
}

/// The one place evaluation reads the filesystem besides `load`: files under the spec's own
/// directory tree, so a spec stays a function of the repo. Sorted, `/`-separated, relative to
/// the spec directory; directories are skipped and links leading outside the tree are dropped.
fn glob_(root: &Path, pattern: &str) -> anyhow::Result<Vec<String>> {
    use std::path::Component;
    if Path::new(pattern).components().any(|c| !matches!(c, Component::Normal(_) | Component::CurDir)) {
        bail!(
            "glob(\"{pattern}\"): patterns are relative to the spec's directory and cannot use `..` or start with `/`"
        );
    }
    let canonical = root.canonicalize().with_context(|| format!("reading {}", root.display()))?;
    let full = format!("{}/{pattern}", glob::Pattern::escape(&root.to_string_lossy()));
    let mut found = Vec::new();
    for entry in glob::glob(&full).map_err(|e| anyhow!("glob(\"{pattern}\"): {e}"))? {
        let path = entry?;
        if !path.is_file() || !path.canonicalize().is_ok_and(|p| p.starts_with(&canonical)) {
            continue;
        }
        let rel = path.strip_prefix(root).context("glob matched outside the spec directory")?;
        found.push(rel.to_string_lossy().replace('\\', "/"));
    }
    found.sort();
    Ok(found)
}

#[starlark_module]
fn builtins(builder: &mut GlobalsBuilder) {
    /// Inside a file loaded from GitHub, its repo and the locked commit; `None` in the spec's own
    /// files. Read from the file name of the call site, `gh:<repo>@<rev>//<path>`, so it also
    /// holds inside a function that the user's spec calls.
    fn origin<'v>(eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<Value<'v>> {
        let site = eval.call_stack_top_location();
        let Some((repo, rev)) = site
            .as_ref()
            .and_then(|s| s.filename().strip_prefix("gh:"))
            .and_then(|f| f.split_once("//"))
            .and_then(|(repo_rev, _)| repo_rev.split_once('@'))
        else {
            return Ok(Value::new_none());
        };
        let heap = eval.heap();
        Ok(heap
            .alloc(AllocStruct([("url", heap.alloc(format!("https://github.com/{repo}"))), ("rev", heap.alloc(rev))])))
    }

    fn glob<'v>(pattern: &str, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<Vec<String>> {
        let root = eval.extra.and_then(|e| e.downcast_ref::<SpecRoot>()).context("glob needs a spec directory")?;
        glob_(&root.dir, pattern)
    }

    fn dev<'v>(
        #[starlark(kwargs)] kwargs: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<starlark::values::none::NoneType> {
        let root = eval.extra.and_then(|e| e.downcast_ref::<SpecRoot>()).context("dev() needs a file")?;
        let slot = root.dev.as_ref().context("dev() belongs in a project's dev.star, not in spec.star")?;
        let mut map = fields("dev", &[], &[], kwargs)?;
        for (field, kind) in [("packages", "pkg"), ("libs", "lib"), ("bins", "bin"), ("services", "service")] {
            if let Some(v) = map.get_mut(field) {
                untag("dev", field, kind, v)?;
            }
        }
        let declared: Dev = serde_json::from_value(Json::Object(map)).map_err(|e| anyhow!("dev(): {e}"))?;
        if slot.borrow().is_some() {
            bail!("dev() is called more than once");
        }
        *slot.borrow_mut() = Some(declared);
        Ok(starlark::values::none::NoneType)
    }

    fn lib<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        native_("lib", args, kwargs)
    }

    fn bin<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        native_("bin", args, kwargs)
    }

    fn host<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Resource_> {
        host_(args, kwargs, eval)
    }

    fn pkg<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        pkg_(args, kwargs)
    }

    fn file<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Resource>("file", &["path"], args, kwargs)
    }

    fn service<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Resource_> {
        let in_dev = eval.extra.and_then(|e| e.downcast_ref::<SpecRoot>()).is_some_and(|r| r.dev.is_some());
        if in_dev {
            return make_untagged::<fspec_core::dev::DevService>("service", &["name"], args, kwargs);
        }
        make::<Resource>("service", &["name"], args, kwargs)
    }

    fn timer<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Resource>("timer", &["name"], args, kwargs)
    }

    fn user<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Resource>("user", &["name"], args, kwargs)
    }

    fn builder<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make_untagged::<fspec_core::resource::Builder>("builder", &["name"], args, kwargs)
    }

    fn vm<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make_untagged::<fspec_core::resource::Vm>("vm", &["on"], args, kwargs)
    }

    fn sync<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        sync_(args, kwargs)
    }

    fn keyring<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make_untagged::<fspec_core::resource::KeyringItem>("keyring", &[], args, kwargs)
    }

    fn json_keys<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make_untagged::<fspec_core::resource::JsonKeys>("json_keys", &["path", "keys"], args, kwargs)
    }

    fn secret<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Resource>("secret", &["path"], args, kwargs)
    }

    fn defaults<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Resource>("defaults", &["domain", "key", "value"], args, kwargs)
    }

    fn setting<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Resource>("setting", &["tool", "key", "value"], args, kwargs)
    }

    fn image<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<System>("image", &["base"], args, kwargs)
    }

    fn fedora<'v>(#[starlark(kwargs)] kwargs: Value<'v>) -> anyhow::Result<Resource_> {
        distro_("fedora", "systemd", kwargs)
    }

    fn arch<'v>(#[starlark(kwargs)] kwargs: Value<'v>) -> anyhow::Result<Resource_> {
        distro_("arch", "systemd", kwargs)
    }

    fn debian<'v>(#[starlark(kwargs)] kwargs: Value<'v>) -> anyhow::Result<Resource_> {
        distro_("debian", "systemd", kwargs)
    }

    fn ubuntu<'v>(#[starlark(kwargs)] kwargs: Value<'v>) -> anyhow::Result<Resource_> {
        distro_("ubuntu", "systemd", kwargs)
    }

    fn void<'v>(#[starlark(kwargs)] kwargs: Value<'v>) -> anyhow::Result<Resource_> {
        distro_("void", "runit", kwargs)
    }

    fn aerynos<'v>(#[starlark(kwargs)] kwargs: Value<'v>) -> anyhow::Result<Resource_> {
        distro_("aerynos", "systemd", kwargs)
    }

    fn tumbleweed<'v>(#[starlark(kwargs)] kwargs: Value<'v>) -> anyhow::Result<Resource_> {
        distro_("opensuse-tumbleweed", "systemd", kwargs)
    }

    fn repo<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Resource>("repo", &["name"], args, kwargs)
    }

    fn aqua<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Source>("aqua", &["name"], args, kwargs)
    }

    fn github<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Source>("github", &["repo"], args, kwargs)
    }

    fn npm<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Source>("npm", &["name"], args, kwargs)
    }

    fn cargo<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Source>("cargo", &["name"], args, kwargs)
    }

    fn flatpak<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Source>("flatpak", &["id"], args, kwargs)
    }

    fn brew<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Source>("brew", &["name"], args, kwargs)
    }

    fn dnf<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Source>("dnf", &["names"], args, kwargs)
    }

    fn pacman<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Source>("pacman", &["names"], args, kwargs)
    }

    fn apt<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Source>("apt", &["names"], args, kwargs)
    }

    fn xbps<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Source>("xbps", &["names"], args, kwargs)
    }

    fn moss<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Source>("moss", &["names"], args, kwargs)
    }

    fn zypper<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Source>("zypper", &["names"], args, kwargs)
    }

    fn git<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Source>("git", &["url", "rev"], args, kwargs)
    }

    fn pypi<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        pypi_(args, kwargs)
    }

    fn url<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Source>("url", &["url"], args, kwargs)
    }

    fn nix<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<Source>("nix", &["attr"], args, kwargs)
    }

    fn list_union<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<fspec_core::resource::ListStrategy>("list_union", &["items"], args, kwargs)
    }

    fn list_replace<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<fspec_core::resource::ListStrategy>("list_replace", &["items"], args, kwargs)
    }

    fn list_keyed<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: Value<'v>,
    ) -> anyhow::Result<Resource_> {
        make::<fspec_core::resource::ListStrategy>("list_keyed", &["field", "items"], args, kwargs)
    }
}

#[cfg(test)]
mod tests;
