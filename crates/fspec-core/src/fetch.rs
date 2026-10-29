use std::collections::HashMap;
use std::fmt;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result, anyhow};
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
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match &self.rate_limited {
            Some(note) => write!(f, "GET {}: GitHub API rate limit reached ({note})", self.url),
            None => write!(f, "GET {}: HTTP {}", self.url, self.status),
        }
    }
}

impl std::error::Error for HttpError {}

/// Whether `err` is an HTTP 404, the one failure that means "not there".
pub fn is_not_found(err: &anyhow::Error) -> bool {
    err.chain().any(|e| e.downcast_ref::<HttpError>().is_some_and(|h| h.status == 404))
}

/// HTTP(S) through ureq; `file://` URLs read the local file, for fixtures.
pub struct LiveFetch {
    agent: ureq::Agent,
    /// Sent as a bearer token to api.github.com: `GITHUB_TOKEN`, else what `gh auth token` prints,
    /// looked up on the first API request.
    github_token: OnceLock<Option<String>>,
}

impl LiveFetch {
    pub fn new() -> LiveFetch {
        LiveFetch::with_timeout(None)
    }

    /// One whose every request gives up after `timeout`, for answers a user is waiting on.
    pub fn with_timeout(timeout: Option<std::time::Duration>) -> LiveFetch {
        let config = ureq::Agent::config_builder().http_status_as_error(false).timeout_global(timeout).build();
        LiveFetch { agent: ureq::Agent::new_with_config(config), github_token: OnceLock::new() }
    }

    fn github_token(&self) -> Option<&str> {
        self.github_token
            .get_or_init(|| {
                std::env::var("GITHUB_TOKEN").ok().filter(|t| !t.is_empty()).or_else(|| {
                    let out = std::process::Command::new("gh").args(["auth", "token"]).output().ok()?;
                    let token = String::from_utf8(out.stdout).ok()?.trim().to_string();
                    (out.status.success() && !token.is_empty()).then_some(token)
                })
            })
            .as_deref()
    }
}

impl LiveFetch {
    /// Stream `url` into `dest`, calling `progress` with the bytes so far and the length the
    /// server gave, and return the body's sha256 in hex. Bodies too large to hold in memory (an
    /// app's disk image) come this way.
    pub fn download(&self, url: &str, dest: &Path, progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<String> {
        let resp = self.agent.get(url).header("User-Agent", "fspec").call().with_context(|| format!("GET {url}"))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(HttpError { url: url.into(), status, rate_limited: None }.into());
        }
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
        let github = url.starts_with("https://api.github.com/");
        let token = if github { self.github_token() } else { None };
        let mut req = self.agent.get(url).header("User-Agent", "fspec");
        if let Some(token) = token {
            req = req.header("Authorization", &format!("Bearer {token}"));
        }
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = req.call().with_context(|| format!("GET {url}"))?;
        let headers: Vec<(String, String)> = resp
            .headers()
            .iter()
            .filter_map(|(k, v)| Some((k.as_str().to_ascii_lowercase(), v.to_str().ok()?.to_string())))
            .collect();
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let header = |name: &str| headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
            let rate_limited = (github && matches!(status, 403 | 429) && header("x-ratelimit-remaining") == Some("0"))
                .then(|| rate_limit_note(header("x-ratelimit-reset"), token.is_some()));
            return Err(HttpError { url: url.into(), status, rate_limited }.into());
        }
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
            .ok_or_else(|| anyhow!(HttpError { url: url.into(), status: 404, rate_limited: None }))
    }

    fn request(&self, url: &str, headers: &[(&str, &str)]) -> Result<Response> {
        let sent = headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        self.requests.lock().unwrap().push((url.to_string(), sent));
        Ok(Response { headers: self.response_headers.get(url).cloned().unwrap_or_default(), body: self.get(url)? })
    }
}
