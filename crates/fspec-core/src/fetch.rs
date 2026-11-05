use std::collections::HashMap;
use std::fmt;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result, anyhow};

use crate::runner::{Cmd, Output, Runner};
use sha2::Digest;

/// HTTP reads for resolvers and the store. Tests use [`MapFetch`].
pub trait Fetch: Sync {
    fn get(&self, url: &str) -> Result<Vec<u8>>;

    /// A GET with extra request headers that also returns the response headers (registry
    /// manifests carry their digest there). Implementations without headers fall back to
    /// [`Fetch::get`].
    fn request(&self, url: &str, _headers: &[(&str, &str)]) -> Result<Response> {
        Ok(Response { headers: vec![], body: self.get(url)? })
    }

    fn get_json(&self, url: &str) -> Result<serde_json::Value> {
        serde_json::from_slice(&self.get(url)?).with_context(|| format!("parsing JSON from {url}"))
    }
}

/// A response body with its headers, names lowercased.
#[derive(Clone, Debug, Default)]
pub struct Response {
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// A response outside 2xx. `rate_limited` is GitHub's API refusing for the hour, which a caller
/// must not read as "no such release".
#[derive(Debug)]
pub struct HttpError {
    pub url: String,
    pub status: u16,
    pub rate_limited: Option<String>,
    /// A GitHub 404 asked without a token, which is also how GitHub answers for a private
    /// repository.
    pub anonymous: bool,
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match &self.rate_limited {
            Some(note) => write!(f, "GET {}: GitHub API rate limit reached ({note})", self.url),
            None if self.anonymous => write!(f, "GET {}: HTTP {} ({PRIVATE})", self.url, self.status),
            None => write!(f, "GET {}: HTTP {}", self.url, self.status),
        }
    }
}

impl std::error::Error for HttpError {}

/// Whether `err` is an HTTP 404, the one failure that means "not there".
pub fn is_not_found(err: &anyhow::Error) -> bool {
    err.chain().any(|e| e.downcast_ref::<HttpError>().is_some_and(|h| h.status == 404))
}

/// `GITHUB_TOKEN`, else what `gh auth token` prints, looked up once on first use. Sent to
/// api.github.com, to git for a `git(...)` source on github.com, and past sudo to the system scope.
pub fn github_token() -> Option<&'static str> {
    static TOKEN: OnceLock<Option<String>> = OnceLock::new();
    TOKEN
        .get_or_init(|| {
            std::env::var("GITHUB_TOKEN").ok().filter(|t| !t.is_empty()).or_else(|| {
                let out = std::process::Command::new("gh").args(["auth", "token"]).output().ok()?;
                let token = String::from_utf8(out.stdout).ok()?.trim().to_string();
                (out.status.success() && !token.is_empty()).then_some(token)
            })
        })
        .as_deref()
}

/// How a GitHub request that needed credentials it did not have is explained.
pub const PRIVATE: &str = "a private repository? set GITHUB_TOKEN or run `gh auth login`";

/// Git configuration that sends `token` for one command against a github.com URL, as
/// `GIT_CONFIG_*` variables so it lands in neither the process list nor the checkout's config.
fn github_git_env(url: &str, token: Option<&str>) -> Vec<(String, String)> {
    use base64::Engine;
    match token {
        Some(t) if is_github(url) => {
            let basic = base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{t}"));
            vec![
                ("GIT_CONFIG_COUNT".into(), "1".into()),
                ("GIT_CONFIG_KEY_0".into(), "http.https://github.com/.extraHeader".into()),
                ("GIT_CONFIG_VALUE_0".into(), format!("Authorization: Basic {basic}")),
            ]
        }
        _ => vec![],
    }
}

fn is_github(url: &str) -> bool {
    url.starts_with("https://github.com/")
}

/// Runs `cmd`, a git command against `url`, with the runner's GitHub token for a github.com URL
/// and no credential prompt, so a private repository is read with the user's login or fails
/// saying it needs one.
pub fn git(runner: &dyn Runner, url: &str, cmd: Cmd) -> Result<Output> {
    let token = if is_github(url) { runner.github_token() } else { None };
    let cmd = github_git_env(url, token.as_deref())
        .into_iter()
        .fold(cmd.env("GIT_TERMINAL_PROMPT", "0"), |c, (k, v)| c.env(k, v));
    runner.check(&cmd).map_err(|e| {
        let text = format!("{e:#}");
        let denied = ["could not read Username", "Authentication failed", "Repository not found"]
            .iter()
            .any(|m| text.contains(m));
        match token {
            _ if !denied || !is_github(url) => e,
            None => e.context(format!("{url}: {PRIVATE}")),
            Some(_) => e.context(format!("{url}: not found, or the GitHub token cannot read it (`gh auth status`)")),
        }
    })
}

/// The API URL serving the release asset that `url`, a github.com release download URL, names.
/// A private repository's download URL answers 404 even to a token; the API asset URL, asked
/// for `application/octet-stream`, redirects to the file.
pub fn release_asset_api(fetch: &dyn Fetch, url: &str) -> Result<Option<String>> {
    let Some(rest) = url.strip_prefix("https://github.com/") else { return Ok(None) };
    let mut parts = rest.splitn(5, '/');
    let (Some(owner), Some(repo), Some("releases"), Some("download"), Some(tail)) =
        (parts.next(), parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Ok(None);
    };
    let Some((tag, _)) = tail.rsplit_once('/') else { return Ok(None) };
    let release = fetch.get_json(&format!("https://api.github.com/repos/{owner}/{repo}/releases/tags/{tag}"))?;
    Ok(release["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| a["browser_download_url"].as_str() == Some(url))
        .and_then(|a| a["url"].as_str())
        .map(str::to_string))
}

/// HTTP(S) through ureq; `file://` URLs read the local file, for fixtures.
pub struct LiveFetch {
    agent: ureq::Agent,
}

impl LiveFetch {
    pub fn new() -> LiveFetch {
        LiveFetch::with_timeout(None)
    }

    /// One whose every request gives up after `timeout`, for answers a user is waiting on.
    pub fn with_timeout(timeout: Option<std::time::Duration>) -> LiveFetch {
        let config = ureq::Agent::config_builder().http_status_as_error(false).timeout_global(timeout).build();
        LiveFetch { agent: ureq::Agent::new_with_config(config) }
    }
}

impl LiveFetch {
    /// Stream `url` into `dest`, calling `progress` with the bytes so far and the length the
    /// server gave, and return the body's sha256 in hex. Bodies too large to hold in memory (an
    /// app's disk image) come this way.
    pub fn download(&self, url: &str, dest: &Path, progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<String> {
        let resp = self.call(url, &[])?;
        let total = resp.headers().get("content-length").and_then(|v| v.to_str().ok()?.parse().ok());
        let mut reader = resp.into_body().into_reader();
        let mut file = std::fs::File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
        let mut sha = sha2::Sha256::new();
        let mut buf = vec![0u8; 1 << 16];
        let mut done = 0u64;
        loop {
            let n = reader.read(&mut buf).with_context(|| format!("reading {url}"))?;
            if n == 0 {
                break;
            }
            sha.update(&buf[..n]);
            file.write_all(&buf[..n]).with_context(|| format!("writing {}", dest.display()))?;
            done += n as u64;
            progress(done, total);
        }
        Ok(sha.finalize().iter().map(|b| format!("{b:02x}")).collect())
    }
}

impl LiveFetch {
    /// GET `url`, the GitHub token sent to api.github.com, as a 2xx response or an
    /// [`HttpError`]. A github.com release download answering 404 is asked again through the API
    /// with the token, the one way a private repository's asset is served.
    fn call(&self, url: &str, headers: &[(&str, &str)]) -> Result<ureq::http::Response<ureq::Body>> {
        let api = url.starts_with("https://api.github.com/");
        let token = if api { github_token() } else { None };
        let mut req = self.agent.get(url).header("User-Agent", "fspec");
        if let Some(token) = token {
            req = req.header("Authorization", &format!("Bearer {token}"));
        }
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = req.call().with_context(|| format!("GET {url}"))?;
        let status = resp.status().as_u16();
        if (200..300).contains(&status) {
            return Ok(resp);
        }
        let github = api || is_github(url);
        if status == 404 && is_github(url) && github_token().is_some() {
            // ureq drops the Authorization header on the redirect to the asset's storage host.
            if let Some(asset) = release_asset_api(self, url)? {
                return self.call(&asset, &[("Accept", "application/octet-stream")]);
            }
        }
        let got = lowered(&resp);
        let header = |name: &str| got.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
        let rate_limited = (api && matches!(status, 403 | 429) && header("x-ratelimit-remaining") == Some("0"))
            .then(|| rate_limit_note(header("x-ratelimit-reset"), token.is_some()));
        let anonymous = github && status == 404 && github_token().is_none();
        Err(HttpError { url: url.into(), status, rate_limited, anonymous }.into())
    }
}

fn lowered(resp: &ureq::http::Response<ureq::Body>) -> Vec<(String, String)> {
    resp.headers()
        .iter()
        .filter_map(|(k, v)| Some((k.as_str().to_ascii_lowercase(), v.to_str().ok()?.to_string())))
        .collect()
}

impl Default for LiveFetch {
    fn default() -> Self {
        LiveFetch::new()
    }
}

impl Fetch for LiveFetch {
    fn get(&self, url: &str) -> Result<Vec<u8>> {
        Ok(self.request(url, &[])?.body)
    }

    fn request(&self, url: &str, headers: &[(&str, &str)]) -> Result<Response> {
        if let Some(path) = url.strip_prefix("file://") {
            let body = std::fs::read(path).with_context(|| format!("reading {url}"))?;
            return Ok(Response { headers: vec![], body });
        }
        let resp = self.call(url, headers)?;
        let headers = lowered(&resp);
        let mut body = Vec::new();
        resp.into_body().into_reader().read_to_end(&mut body).with_context(|| format!("reading {url}"))?;
        Ok(Response { headers, body })
    }
}

fn rate_limit_note(reset: Option<&str>, authenticated: bool) -> String {
    let when = reset
        .and_then(|r| r.parse::<u64>().ok())
        .and_then(|r| r.checked_sub(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs()))
        .map(|secs| format!("resets in {} min", secs.div_ceil(60)))
        .unwrap_or_else(|| "resets within the hour".into());
    if authenticated { when } else { format!("{when}; set GITHUB_TOKEN or run `gh auth login` for a higher limit") }
}

/// A recorded request: the URL and the headers it carried.
pub type SentRequest = (String, Vec<(String, String)>);

/// Serves fixed bodies by exact URL.
#[derive(Default)]
pub struct MapFetch {
    pub bodies: HashMap<String, Vec<u8>>,
    /// Response headers per URL, for [`Fetch::request`].
    pub response_headers: HashMap<String, Vec<(String, String)>>,
    /// Every [`Fetch::request`] made.
    pub requests: Mutex<Vec<SentRequest>>,
}

impl MapFetch {
    pub fn with(mut self, url: &str, body: impl Into<Vec<u8>>) -> MapFetch {
        self.bodies.insert(url.into(), body.into());
        self
    }

    pub fn with_headers(mut self, url: &str, headers: &[(&str, &str)]) -> MapFetch {
        self.response_headers.insert(url.into(), headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect());
        self
    }
}

impl Fetch for MapFetch {
    fn get(&self, url: &str) -> Result<Vec<u8>> {
        self.bodies
            .get(url)
            .cloned()
            .ok_or_else(|| anyhow!(HttpError { url: url.into(), status: 404, rate_limited: None, anonymous: false }))
    }

    fn request(&self, url: &str, headers: &[(&str, &str)]) -> Result<Response> {
        let sent = headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        self.requests.lock().unwrap().push((url.to_string(), sent));
        Ok(Response { headers: self.response_headers.get(url).cloned().unwrap_or_default(), body: self.get(url)? })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::RecordingRunner;

    fn denied(stderr: &str) -> Output {
        Output { status: 128, stdout: String::new(), stderr: stderr.into() }
    }

    #[test]
    fn git_against_github_carries_the_token_and_never_prompts() {
        let runner = RecordingRunner { github_token: Some("t0k".into()), ..Default::default() };
        git(
            &runner,
            "https://github.com/a/private",
            Cmd::new("git").args(["ls-remote", "https://github.com/a/private"]),
        )
        .unwrap();
        git(&runner, "https://gitlab.com/a/b", Cmd::new("git").args(["ls-remote", "https://gitlab.com/a/b"])).unwrap();
        let calls = runner.calls.lock().unwrap();
        let keys = |c: &Cmd| c.env.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>();
        assert_eq!(
            keys(&calls[0]),
            ["GIT_TERMINAL_PROMPT", "GIT_CONFIG_COUNT", "GIT_CONFIG_KEY_0", "GIT_CONFIG_VALUE_0"]
        );
        assert_eq!(keys(&calls[1]), ["GIT_TERMINAL_PROMPT"]);
    }

    #[test]
    fn git_refused_by_github_says_it_needs_a_login() {
        let runner = RecordingRunner::default();
        runner.respond(
            "git ls-remote",
            denied("fatal: could not read Username for 'https://github.com': terminal prompts disabled"),
        );
        let err = git(&runner, "https://github.com/a/private", Cmd::new("git").args(["ls-remote", "x"])).unwrap_err();
        assert!(err.to_string().contains("set GITHUB_TOKEN or run `gh auth login`"), "{err:#}");

        let runner = RecordingRunner { github_token: Some("t0k".into()), ..Default::default() };
        runner.respond("git ls-remote", denied("remote: Repository not found."));
        let err = git(&runner, "https://github.com/a/private", Cmd::new("git").args(["ls-remote", "x"])).unwrap_err();
        assert!(err.to_string().contains("the GitHub token cannot read it"), "{err:#}");

        let runner = RecordingRunner::default();
        runner.respond("git ls-remote", denied("fatal: unable to access: Could not resolve host"));
        let err = git(&runner, "https://github.com/a/b", Cmd::new("git").args(["ls-remote", "x"])).unwrap_err();
        assert!(!format!("{err:#}").contains("GITHUB_TOKEN"), "{err:#}");
    }

    #[test]
    fn a_release_download_maps_to_its_api_asset() {
        let dl = "https://github.com/o/r/releases/download/v1.0/tool-linux.tar.gz";
        let release = serde_json::json!({"assets": [
            {"browser_download_url": "https://github.com/o/r/releases/download/v1.0/other", "url": "https://api.github.com/repos/o/r/releases/assets/1"},
            {"browser_download_url": dl, "url": "https://api.github.com/repos/o/r/releases/assets/2"},
        ]});
        let fetch =
            MapFetch::default().with("https://api.github.com/repos/o/r/releases/tags/v1.0", release.to_string());
        assert_eq!(
            release_asset_api(&fetch, dl).unwrap().as_deref(),
            Some("https://api.github.com/repos/o/r/releases/assets/2")
        );
        assert_eq!(release_asset_api(&fetch, "https://github.com/o/r/archive/v1.0.tar.gz").unwrap(), None);
        assert_eq!(release_asset_api(&fetch, "https://example.com/o/r/releases/download/v1.0/x").unwrap(), None);
    }

    #[test]
    fn an_anonymous_github_404_names_the_login() {
        let e = HttpError {
            url: "https://api.github.com/repos/a/b".into(),
            status: 404,
            rate_limited: None,
            anonymous: true,
        };
        assert!(e.to_string().ends_with("HTTP 404 (a private repository? set GITHUB_TOKEN or run `gh auth login`)"));
    }

    #[test]
    fn a_github_source_gets_the_token_as_a_header_for_that_host_alone() {
        let env = github_git_env("https://github.com/a/private", Some("t0k"));
        assert_eq!(env[0], ("GIT_CONFIG_COUNT".to_string(), "1".to_string()));
        assert_eq!(env[1].1, "http.https://github.com/.extraHeader");
        assert_eq!(env[2].1, "Authorization: Basic eC1hY2Nlc3MtdG9rZW46dDBr");
        assert!(github_git_env("https://gitlab.com/a/b", Some("t0k")).is_empty());
        assert!(github_git_env("https://github.com/a/b", None).is_empty());
    }
}
