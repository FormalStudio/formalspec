use fspec_core::Facts;
use fspec_core::Lock;
use fspec_core::Paths;
use fspec_core::Scope;
use fspec_core::State;
use fspec_core::apply::{Cx, state_dir};
use fspec_core::fetch::MapFetch;
use fspec_core::resource::{Distro, System};
use fspec_core::runner::RecordingRunner;

/// A rooted temp host with a recording runner.
pub struct World {
    _dir: tempfile::TempDir,
    pub paths: Paths,
    pub facts: Facts,
    pub lock: Lock,
    pub runner: RecordingRunner,
    pub fetch: MapFetch,
}

impl World {
    pub fn linux(init: fspec_core::facts::Init) -> World {
        let system = System::Distro(Distro { distro: "fedora".into(), init, packages: vec![], kernel_args: vec![] });
        World::new("x86_64-linux", Some(system))
    }

    pub fn darwin() -> World {
        World::new("aarch64-darwin", None)
    }

    fn new(platform: &str, system: Option<System>) -> World {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().into(), dir.path().join("home/u"));
        let facts = Facts::declared("h", platform, system.as_ref()).unwrap();
        World {
            _dir: dir,
            paths,
            facts,
            lock: Lock::default(),
            runner: RecordingRunner::default(),
            fetch: MapFetch::default(),
        }
    }

    pub fn cx(&self, scope: Scope) -> Cx<'_> {
        let generation = State::load(&state_dir(&self.paths, scope).join("state.json")).unwrap().generation + 1;
        Cx {
            paths: &self.paths,
            facts: &self.facts,
            spec_dir: &self.paths.root,
            lock: &self.lock,
            runner: &self.runner,
            fetch: &self.fetch,
            generation,
            scope,
            protect: &[],
            builders: &[],
            build_cache: None,
            checkout: None,
        }
    }
}

/// Build a resource from the JSON a spec would evaluate to.
pub fn resource(json: serde_json::Value) -> fspec_core::Resource {
    serde_json::from_value(json).unwrap()
}

pub fn desired(r: fspec_core::Resource) -> fspec_core::Desired {
    fspec_core::Desired { resource: r, owner: None }
}
