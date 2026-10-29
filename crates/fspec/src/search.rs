//! `fspec search`: the catalog search the launcher's Install mode and an unresolved name's
//! suggestions share (`fspec_sources::catalog`).

use std::time::Duration;

use anyhow::Result;
use fspec_core::Paths;
use fspec_core::fetch::LiveFetch;
use fspec_core::runner::LiveRunner;
use fspec_sources::catalog::{self, Catalog};
use serde_json::json;

/// Per-query catalogs answer within this or are skipped.
const QUICK: Duration = Duration::from_secs(2);
/// The aqua registry is 3.5 MB, fetched at most once a day.
const INDEX: Duration = Duration::from_secs(15);

pub fn catalog(paths: &Paths, network: bool) -> Catalog {
    let net = network.then(|| Box::new(LiveFetch::with_timeout(Some(QUICK))) as Box<dyn fspec_core::fetch::Fetch>);
    Catalog::local(&paths.home, net)
}

pub fn run(paths: &Paths, query: &str, json: bool, limit: usize, offline: bool) -> Result<()> {
    let fetch = LiveFetch::with_timeout(Some(INDEX));
    let cx = fspec_sources::Cx::new(&fetch, &LiveRunner).with_catalog(catalog(paths, !offline));
    let found = catalog::search(query, &cx);
    let hits: Vec<_> = found.hits.iter().map(|(_, h)| h).take(limit).collect();
    if json {
        for h in &hits {
            let line = json!({
                "id": h.id(),
                "name": h.name,
                "source": h.origin.prefix(),
                "description": h.description,
                "run": h.origin.runs(),
                "spec": h.spec(),
            });
            println!("{line}");
        }
    } else if hits.is_empty() {
        println!("no package matches `{query}`");
    } else {
        print!("{}", catalog::table(&hits));
    }
    if catalog::VERBOSE.load(std::sync::atomic::Ordering::Relaxed) {
        for (name, e) in &found.skipped {
            eprintln!("searching {name} failed: {e}");
        }
    }
    Ok(())
}
