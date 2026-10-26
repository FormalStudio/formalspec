//! `.github/workflows/fspec-packages.yml`: builds every `git(...)` package on a runner of each
//! platform a host declares and pushes the result to `ghcr.io/<owner>/fspec-cache`.

use std::collections::BTreeSet;

use anyhow::{Result, bail};
use fspec_core::resource::{Host, Resource, Source};

/// `(package, platform)` for every `git(...)` package a host declares, sorted.
pub fn jobs(hosts: &[Host]) -> Vec<(String, String)> {
    let all: BTreeSet<_> = hosts
        .iter()
        .flat_map(|h| {
            h.resources.iter().filter_map(move |r| match r {
                Resource::Pkg(p) if matches!(p.source, Source::Git { .. }) => {
                    Some((p.name.clone(), h.platform.clone()))
                }
                _ => None,
            })
        })
        .collect();
    all.into_iter().collect()
}

fn runner(platform: &str) -> Result<&'static str> {
    Ok(match platform {
        "x86_64-linux" => "ubuntu-24.04",
        "aarch64-linux" => "ubuntu-24.04-arm",
        "aarch64-darwin" => "macos-26",
        other => {
            bail!("no CI runner for platform {other}, the cache builds x86_64-linux, aarch64-linux and aarch64-darwin")
        }
    })
}

pub const PATH: &str = ".github/workflows/fspec-packages.yml";

/// `spec` is the spec file relative to the repo root, which is where the workflow lives. `branch`
/// is the one whose pushes rebuild the cache: the spec can live on a branch other than main.
pub fn workflow(jobs: &[(String, String)], spec: &str, branch: &str) -> Result<String> {
    if jobs.is_empty() {
        bail!("no host declares a git(...) package, there is nothing to cache");
    }
    let mut matrix = String::new();
    for (pkg, platform) in jobs {
        matrix
            .push_str(&format!("          - {{ pkg: {pkg}, platform: {platform}, runner: {} }}\n", runner(platform)?));
    }
    let dir = spec.rsplit_once('/').map(|(d, _)| format!("{d}/")).unwrap_or_default();
    Ok(format!(
        r#"name: fspec packages

on:
  push:
    branches: [{branch}]
    paths: ["{dir}fspec.lock", "{dir}patches/**"]
  pull_request:
    paths: ["{dir}fspec.lock", "{dir}patches/**"]
  workflow_dispatch:

permissions:
  contents: read
  packages: write

jobs:
  build:
    name: ${{{{ matrix.pkg }}}} (${{{{ matrix.platform }}}})
    # Builds the nightly lock update before it merges, so the cache holds its artifacts. Other
    # pull requests run without package write access.
    if: github.event_name != 'pull_request' || github.head_ref == 'fspec/update-lock'
    runs-on: ${{{{ matrix.runner }}}}
    strategy:
      fail-fast: false
      matrix:
        include:
{matrix}    steps:
      - uses: actions/checkout@v4
      - uses: oras-project/setup-oras@v1
      - name: Install fspec
        run: cargo install --locked --git {repo} fspec
      - name: Build
        run: fspec --spec {spec} build --pkg ${{{{ matrix.pkg }}}} --platform ${{{{ matrix.platform }}}} --out out
      - name: Push
        env:
          TOKEN: ${{{{ secrets.GITHUB_TOKEN }}}}
        run: |
          owner=$(echo "${{{{ github.repository_owner }}}}" | tr '[:upper:]' '[:lower:]')
          echo "$TOKEN" | oras login ghcr.io -u "${{{{ github.actor }}}}" --password-stdin
          cd out
          oras push "ghcr.io/$owner/fspec-cache/${{{{ matrix.pkg }}}}:$(cat tag)" \
            "${{{{ matrix.pkg }}}}-${{{{ matrix.platform }}}}.tar.zst:application/vnd.fspec.package.v1.tar+zstd"
"#,
        repo = env!("CARGO_PKG_REPOSITORY"),
    ))
}

#[cfg(test)]
mod tests;
