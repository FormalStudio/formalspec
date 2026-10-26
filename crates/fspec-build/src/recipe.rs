//! Which build system a checkout uses and the exact commands for it.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use fspec_core::runner::Cmd;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recipe {
    Cargo,
    Meson,
    Cmake,
    Go,
    Make,
    /// `build = [...]` in the spec.
    Explicit,
}

/// The checkout's build system, first match wins: `Cargo.toml`, `meson.build`,
/// `CMakeLists.txt`, `go.mod`, then a Makefile. A `build` in the spec overrides all of it.
pub fn select(src: &Path, build: &[String]) -> Result<Recipe> {
    if !build.is_empty() {
        return Ok(Recipe::Explicit);
    }
    let has = |names: &[&str]| names.iter().any(|n| src.join(n).is_file());
    Ok(if has(&["Cargo.toml"]) {
        Recipe::Cargo
    } else if has(&["meson.build"]) {
        Recipe::Meson
    } else if has(&["CMakeLists.txt"]) {
        Recipe::Cmake
    } else if has(&["go.mod"]) {
        Recipe::Go
    } else if has(&["Makefile", "makefile", "GNUmakefile"]) {
        Recipe::Make
    } else {
        bail!("no Cargo.toml, meson.build, CMakeLists.txt, go.mod or Makefile in the checkout, set build = [...]")
    })
}

/// Where a recipe reads and writes. `out` is the install prefix of everything but Cargo. `src`,
/// `build` and `cache` persist between builds of one package; `out` is new each time.
pub struct Dirs {
    pub src: PathBuf,
    pub build: PathBuf,
    pub out: PathBuf,
    /// `FSPEC_CACHE` for an explicit build: its toolchains, downloads and target directories.
    pub cache: PathBuf,
}

impl Recipe {
    /// Directories that can hold the finished executables, in order.
    pub fn bin_dirs(self, d: &Dirs) -> Vec<PathBuf> {
        match self {
            Recipe::Cargo => vec![d.build.join("release")],
            Recipe::Explicit => vec![d.out.join("bin"), d.out.clone()],
            _ => vec![d.out.join("bin")],
        }
    }

    pub fn commands(self, d: &Dirs, flags: &[String], build: &[String]) -> Vec<Cmd> {
        let path = |p: &Path| p.display().to_string();
        let (src, bld, out) = (path(&d.src), path(&d.build), path(&d.out));
        match self {
            Recipe::Cargo => vec![
                Cmd::new("cargo")
                    .args(["build", "--release", "--locked", "--manifest-path"])
                    .arg(path(&d.src.join("Cargo.toml")))
                    .args(["--target-dir", &bld])
                    .args(flags),
            ],
            Recipe::Meson => vec![
                Cmd::new("meson")
                    .args(["setup", &bld, &src, "--buildtype", "release", "--prefix", &out])
                    // `setup` refuses a directory the last build configured unless told to redo it.
                    .args(d.build.join("meson-private").is_dir().then_some("--reconfigure"))
                    .args(flags),
                Cmd::new("meson").args(["compile", "-C", &bld]),
                Cmd::new("meson").args(["install", "-C", &bld]),
            ],
            Recipe::Cmake => vec![
                Cmd::new("cmake")
                    .args(["-S", &src, "-B", &bld, "-DCMAKE_BUILD_TYPE=Release"])
                    .arg(format!("-DCMAKE_INSTALL_PREFIX={out}"))
                    .args(flags),
                Cmd::new("cmake").args(["--build", &bld, "--parallel"]),
                Cmd::new("cmake").args(["--install", &bld]),
            ],
            Recipe::Go => vec![
                Cmd::new("go").args(["build", "-C", &src, "-o"]).arg(format!("{out}/bin/")).args(flags).arg("./..."),
            ],
            Recipe::Make => vec![
                Cmd::new("make").args(["-C", &src]).args(flags),
                Cmd::new("make").args(["-C", &src, "install"]).arg(format!("PREFIX={out}")),
            ],
            Recipe::Explicit => {
                let (program, args) = build.split_first().expect("explicit builds have a command");
                vec![
                    Cmd::new(program)
                        .args(args)
                        .env("FSPEC_SRC", src)
                        .env("FSPEC_OUT", out)
                        .env("FSPEC_CACHE", path(&d.cache)),
                ]
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn dirs() -> Dirs {
        Dirs { src: "/w/src".into(), build: "/w/build".into(), out: "/w/out".into(), cache: "/w/cache".into() }
    }

    fn lines(r: Recipe, flags: &[&str]) -> Vec<String> {
        let flags: Vec<String> = flags.iter().map(ToString::to_string).collect();
        r.commands(&dirs(), &flags, &["sh".into(), "-c".into(), "make fancy".into()])
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn detection_order_and_override() {
        let pick = |files: &[&str], build: &[String]| {
            let dir = tempfile::tempdir().unwrap();
            for f in files {
                fs::write(dir.path().join(f), "").unwrap();
            }
            select(dir.path(), build)
        };
        assert_eq!(pick(&["Cargo.toml", "Makefile"], &[]).unwrap(), Recipe::Cargo);
        assert_eq!(pick(&["meson.build", "CMakeLists.txt"], &[]).unwrap(), Recipe::Meson);
        assert_eq!(pick(&["CMakeLists.txt", "Makefile"], &[]).unwrap(), Recipe::Cmake);
        assert_eq!(pick(&["go.mod", "Makefile"], &[]).unwrap(), Recipe::Go);
        assert_eq!(pick(&["GNUmakefile"], &[]).unwrap(), Recipe::Make);
        assert_eq!(pick(&["Cargo.toml"], &["sh".into()]).unwrap(), Recipe::Explicit);
        assert!(pick(&["README"], &[]).is_err());
    }

    #[test]
    fn command_sequences() {
        assert_eq!(
            lines(Recipe::Cargo, &["--features", "pipewire"]),
            [
                "cargo build --release --locked --manifest-path /w/src/Cargo.toml --target-dir /w/build --features pipewire"
            ]
        );
        assert_eq!(
            lines(Recipe::Meson, &["-Dx=1"]),
            [
                "meson setup /w/build /w/src --buildtype release --prefix /w/out -Dx=1",
                "meson compile -C /w/build",
                "meson install -C /w/build"
            ]
        );
        assert_eq!(
            lines(Recipe::Cmake, &["-DX=ON"]),
            [
                "cmake -S /w/src -B /w/build -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/w/out -DX=ON",
                "cmake --build /w/build --parallel",
                "cmake --install /w/build"
            ]
        );
        assert_eq!(lines(Recipe::Go, &["-tags", "x"]), ["go build -C /w/src -o /w/out/bin/ -tags x ./..."]);
        assert_eq!(lines(Recipe::Make, &["V=1"]), ["make -C /w/src V=1", "make -C /w/src install PREFIX=/w/out"]);
        assert_eq!(lines(Recipe::Explicit, &["ignored"]), ["sh -c 'make fancy'"]);
    }

    #[test]
    fn explicit_builds_get_the_checkout_and_the_output_directory() {
        let cmds = Recipe::Explicit.commands(&dirs(), &[], &["sh".into(), "-c".into(), "true".into()]);
        assert_eq!(
            cmds[0].env,
            [
                ("FSPEC_SRC".into(), "/w/src".into()),
                ("FSPEC_OUT".into(), "/w/out".into()),
                ("FSPEC_CACHE".into(), "/w/cache".into())
            ]
        );
    }
}
