//! The admin console: static assets compiled into the binary, and the content
//! negotiation that serves them.
//!
//! There is no build step and no bundler. The console is plain ES modules and one
//! vendored 13KB runtime (`ui/VENDOR.md`), so `cargo build` stays hekla's only build
//! and the whole thing works with no network.
//!
//! **The console and the API share their URLs.** `GET /admin/events` returns JSON to
//! `curl` and the console's HTML shell to a browser, decided by `Accept`. Deep links
//! come free: `/admin/effects/send-welcome` in a browser opens that view, and the
//! same URL in a client returns that effect. The negotiation is attached per route
//! from `route_table` (see [`crate::server::app`]), never to the whole router, so a
//! path outside `/admin` is untouched and an unrouted `/admin/...` still 404s rather
//! than becoming a 200 page.
//!
//! **[`ASSETS`] is the namespace.** Lookup goes through the table and nothing else, so
//! a name that is not compiled in does not exist whatever sits on disk. That is what
//! makes the `HEKLA_UI_DIR` development override safe: it substitutes the *content* of
//! an already-resolved asset, and never joins a request-supplied path onto a
//! directory. Asset names are flat because the route captures one path segment.

use std::borrow::Cow;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::hash;

/// One file of the console, embedded at compile time.
pub struct Asset {
    pub name: &'static str,
    pub content_type: &'static str,
    pub bytes: &'static [u8],
}

/// The file every path serves to a browser. The console routes client-side from
/// there, so one document backs every view.
pub const SHELL: &str = "index.html";

const HTML: &str = "text/html; charset=utf-8";
const JS: &str = "text/javascript; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";
const SVG: &str = "image/svg+xml";

/// Every file the console is made of.
///
/// One table, for the same reason `route_table` is one table: the router serves from
/// it, the generated document enumerates it, and the development override may only
/// replace the content of a name that already appears here. A file added to `ui/`
/// and not listed is not served; a name listed with no file is a compile error.
pub const ASSETS: &[Asset] = &[
    Asset {
        name: "index.html",
        content_type: HTML,
        bytes: include_bytes!("../ui/index.html"),
    },
    Asset {
        name: "style.css",
        content_type: CSS,
        bytes: include_bytes!("../ui/style.css"),
    },
    Asset {
        name: "favicon.svg",
        content_type: SVG,
        bytes: include_bytes!("../ui/favicon.svg"),
    },
    Asset {
        name: "vendor-preact.js",
        content_type: JS,
        bytes: include_bytes!("../ui/vendor-preact.js"),
    },
    Asset {
        name: "app.js",
        content_type: JS,
        bytes: include_bytes!("../ui/app.js"),
    },
    Asset {
        name: "router.js",
        content_type: JS,
        bytes: include_bytes!("../ui/router.js"),
    },
    Asset {
        name: "api.js",
        content_type: JS,
        bytes: include_bytes!("../ui/api.js"),
    },
    Asset {
        name: "store.js",
        content_type: JS,
        bytes: include_bytes!("../ui/store.js"),
    },
    Asset {
        name: "format.js",
        content_type: JS,
        bytes: include_bytes!("../ui/format.js"),
    },
    Asset {
        name: "theme.js",
        content_type: JS,
        bytes: include_bytes!("../ui/theme.js"),
    },
    Asset {
        name: "ui-badge.js",
        content_type: JS,
        bytes: include_bytes!("../ui/ui-badge.js"),
    },
    Asset {
        name: "ui-chips.js",
        content_type: JS,
        bytes: include_bytes!("../ui/ui-chips.js"),
    },
    Asset {
        name: "ui-confirm.js",
        content_type: JS,
        bytes: include_bytes!("../ui/ui-confirm.js"),
    },
    Asset {
        name: "ui-copy.js",
        content_type: JS,
        bytes: include_bytes!("../ui/ui-copy.js"),
    },
    Asset {
        name: "ui-panel.js",
        content_type: JS,
        bytes: include_bytes!("../ui/ui-panel.js"),
    },
    Asset {
        name: "ui-json.js",
        content_type: JS,
        bytes: include_bytes!("../ui/ui-json.js"),
    },
    Asset {
        name: "ui-palette.js",
        content_type: JS,
        bytes: include_bytes!("../ui/ui-palette.js"),
    },
    Asset {
        name: "ui-sparkline.js",
        content_type: JS,
        bytes: include_bytes!("../ui/ui-sparkline.js"),
    },
    Asset {
        name: "ui-states.js",
        content_type: JS,
        bytes: include_bytes!("../ui/ui-states.js"),
    },
    Asset {
        name: "ui-table.js",
        content_type: JS,
        bytes: include_bytes!("../ui/ui-table.js"),
    },
    Asset {
        name: "view-effects.js",
        content_type: JS,
        bytes: include_bytes!("../ui/view-effects.js"),
    },
    Asset {
        name: "view-events.js",
        content_type: JS,
        bytes: include_bytes!("../ui/view-events.js"),
    },
    Asset {
        name: "view-overview.js",
        content_type: JS,
        bytes: include_bytes!("../ui/view-overview.js"),
    },
    Asset {
        name: "view-projectors.js",
        content_type: JS,
        bytes: include_bytes!("../ui/view-projectors.js"),
    },
    Asset {
        name: "view-schema.js",
        content_type: JS,
        bytes: include_bytes!("../ui/view-schema.js"),
    },
    Asset {
        name: "view-subjects.js",
        content_type: JS,
        bytes: include_bytes!("../ui/view-subjects.js"),
    },
    Asset {
        name: "view-system.js",
        content_type: JS,
        bytes: include_bytes!("../ui/view-system.js"),
    },
    Asset {
        name: "view-trace.js",
        content_type: JS,
        bytes: include_bytes!("../ui/view-trace.js"),
    },
];

/// The asset named `name`, or `None`.
///
/// Over the table and nothing else. A name carrying `..`, a leading `/`, or anything
/// else a caller might hope resolves elsewhere simply does not match an entry, so
/// path traversal is not defended against here so much as unrepresentable.
pub fn asset(name: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.name == name)
}

/// Every asset name, sorted.
///
/// Sorted rather than in table order because the generated document enumerates these:
/// reordering the table must not churn a committed `openapi.json`.
pub fn asset_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = ASSETS.iter().map(|asset| asset.name).collect();
    names.sort_unstable();
    names
}

/// Every distinct content type the asset route can return, sorted, as an OpenAPI
/// `content` object.
pub fn media_types() -> Value {
    let mut types: Vec<&'static str> = ASSETS.iter().map(|asset| asset.content_type).collect();
    types.sort_unstable();
    types.dedup();
    let mut content = serde_json::Map::new();
    for media in types {
        // The media type key carries its charset, which is right on the wire and
        // wrong as an OpenAPI key: the document names the type, the header carries
        // the parameters.
        let key = media.split(';').next().unwrap_or(media).to_owned();
        content.insert(key, json!({ "schema": { "type": "string" } }));
    }
    Value::Object(content)
}

/// The development override directory, read once from `HEKLA_UI_DIR`.
///
/// Unset in every deployment. When set, the console is served from that directory so
/// editing a file and reloading is enough, with no recompile.
pub fn override_dir() -> Option<&'static Path> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| env::var_os("HEKLA_UI_DIR").map(PathBuf::from))
        .as_deref()
}

/// The bytes to serve for `asset`: the copy in `dir` when a development override is
/// configured and holds the file, the compiled-in copy otherwise.
///
/// `dir` is joined with the *table's* own name and never with anything from the
/// request, which is what keeps the override from reaching a path the binary does not
/// already carry. It is a parameter rather than a call to [`override_dir`] so it stays
/// a pure function: `env::set_var` is unsafe in this edition and a test that set it
/// would race every other test in the process.
///
/// A missing or unreadable override falls back rather than failing, because the
/// common case is pointing at a tree that carries only the file being edited.
pub fn bytes(asset: &'static Asset, dir: Option<&Path>) -> Cow<'static, [u8]> {
    match dir.map(|dir| fs::read(dir.join(asset.name))) {
        Some(Ok(bytes)) => Cow::Owned(bytes),
        _ => Cow::Borrowed(asset.bytes),
    }
}

/// Whether the caller asked for the console rather than JSON.
///
/// A media *range* is not a request. `*/*` is what `curl` and a bare `fetch()` send,
/// and treating it as HTML would turn every existing client's JSON into a web page, so
/// only an explicit `text/html` (or its XHTML spelling) counts, and `;q=0` on it is an
/// explicit refusal rather than a request.
pub fn wants_html(headers: &HeaderMap) -> bool {
    let html = quality(headers, "text", "html").max(quality(headers, "application", "xhtml+xml"));
    let json = quality(headers, "application", "json");
    // Strictly greater, so a tie goes to the API. That is the load-bearing case rather
    // than a detail: `*/*` is what curl and a bare `fetch()` send, and it matches both
    // representations equally, so the default has to be JSON or every existing client
    // would start receiving a web page.
    html > 0.0 && html > json
}

/// The weight the `Accept` header gives one concrete media type, or `0.0` if it names
/// nothing that matches.
///
/// Scored against only the two representations these routes can actually serve, which
/// is what makes it right: a browser sends `image/avif` and `image/webp` at an implicit
/// `q=1` alongside `text/html`, so comparing HTML against *every* other type the
/// caller happened to mention would rank a real browser as not wanting HTML.
///
/// The most specific matching range wins, per RFC 9110: an exact `text/html` beats
/// `text/*`, which beats `*/*`. Ties in specificity take the higher weight.
fn quality(headers: &HeaderMap, media_type: &str, subtype: &str) -> f32 {
    let mut best: Option<(u8, f32)> = None;
    for range in headers
        // `Accept` may legally repeat rather than being one comma-joined value.
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
    {
        let mut parts = range.split(';').map(str::trim);
        let Some((range_type, range_subtype)) = parts
            .next()
            .filter(|media| !media.is_empty())
            .and_then(|media| media.split_once('/'))
        else {
            continue;
        };
        let specificity = match (range_type, range_subtype) {
            ("*", "*") => 0,
            (candidate, "*") if candidate.eq_ignore_ascii_case(media_type) => 1,
            (candidate, sub)
                if candidate.eq_ignore_ascii_case(media_type)
                    && sub.eq_ignore_ascii_case(subtype) =>
            {
                2
            }
            _ => continue,
        };
        let weight = parts
            .find_map(|param| {
                let (key, value) = param.split_once('=')?;
                key.trim().eq_ignore_ascii_case("q").then_some(value.trim())
            })
            // An unparseable `q` is the same as none: the range is still a request,
            // just not a weighted one. A well-formed `q=0` is a refusal and scores 0.
            .map_or(1.0, |raw| raw.parse::<f32>().unwrap_or(1.0));
        best = match best {
            Some((seen, _)) if seen > specificity => best,
            Some((seen, quality)) if seen == specificity => Some((seen, quality.max(weight))),
            _ => Some((specificity, weight)),
        };
    }
    best.map_or(0.0, |(_, weight)| weight)
}

/// The validator for an asset's bytes, or `None` while a development override is
/// active.
///
/// `no-cache` is revalidate-every-time, and revalidation needs something to revalidate
/// against: with no validator every conditional request degrades to a full 200, so a
/// page load re-transfers the whole console. The digest is over the compiled-in bytes,
/// which are fixed for the life of the binary and differ exactly when a deployed
/// console does, so it is a strong validator without a build step to produce one.
///
/// An override has none on purpose. Its bytes change under a fixed binary, which is
/// what `no-store` says, and a validator would be the one thing able to contradict it.
pub fn etag(asset: &'static Asset) -> Option<&'static str> {
    if override_dir().is_some() {
        return None;
    }
    static ETAGS: OnceLock<HashMap<&'static str, String>> = OnceLock::new();
    ETAGS
        .get_or_init(|| {
            ASSETS
                .iter()
                .map(|asset| (asset.name, format!("\"{}\"", hash::sha256_hex(asset.bytes))))
                .collect()
        })
        .get(asset.name)
        .map(String::as_str)
}

/// Whether `If-None-Match` already names `etag`.
///
/// The header is a comma list and may legally repeat, and a shared cache is allowed to
/// weaken a strong validator with a `W/` prefix, so neither a compare against the whole
/// header value nor an exact match on one entry is enough.
fn revalidates(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .any(|candidate| candidate == "*" || candidate.trim_start_matches("W/") == etag)
}

/// Serve the console's shell.
pub fn shell(headers: &HeaderMap) -> Response {
    match asset(SHELL) {
        Some(shell) => serve(shell, headers),
        // Unreachable: `SHELL` is in the table or the crate does not compile. Answered
        // rather than panicked because a request thread is the wrong place to abort.
        None => (StatusCode::INTERNAL_SERVER_ERROR, "no shell").into_response(),
    }
}

/// Serve one asset, honouring the development override and the caller's validator.
pub fn serve(asset: &'static Asset, headers: &HeaderMap) -> Response {
    let dir = override_dir();
    let caching = match dir {
        // The bytes change under a fixed version while an override is active, so
        // nothing about them may be cached.
        Some(_) => "no-store",
        // Otherwise the content is fixed for the life of the binary. `no-cache` is
        // revalidate-every-time, not do-not-cache: a new binary must never be shadowed
        // by an old file, and on loopback the whole console is a couple of hundred
        // kilobytes.
        None => "no-cache",
    };
    let tag = etag(asset);
    if let Some(tag) = tag
        && revalidates(headers, tag)
    {
        // The transfer `no-cache` was always meant to save. A 304 repeats the validator
        // and the caching rule because it updates the stored response's headers, and
        // carries no body or content type because it replaces neither.
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        let out = response.headers_mut();
        out.insert(header::CACHE_CONTROL, HeaderValue::from_static(caching));
        out.insert(header::ETAG, HeaderValue::from_static(tag));
        return response;
    }
    let body = match bytes(asset, dir) {
        // The compiled-in slice goes to the body as-is. This is the path every
        // deployment takes, ~25 assets per page load, and it used to copy each one on
        // to the heap on its way out.
        Cow::Borrowed(slice) => Bytes::from_static(slice),
        Cow::Owned(owned) => Bytes::from(owned),
    };
    let mut response = (
        [
            (header::CONTENT_TYPE, asset.content_type),
            (header::CACHE_CONTROL, caching),
        ],
        body,
    )
        .into_response();
    if let Some(tag) = tag {
        response
            .headers_mut()
            .insert(header::ETAG, HeaderValue::from_static(tag));
    }
    response
}

/// Serve the console to a browser and the JSON API to everything else.
///
/// Short-circuits *before* the handler, so an `Accept: text/html` request to any
/// `/admin` path is a 200 shell even when the name does not exist. That is deliberate:
/// `/admin/effects/nope` in a browser opens the console, which then fetches the same
/// URL as JSON and reports the 404 itself, rather than the browser showing a bare
/// error document.
pub async fn negotiate(request: Request, next: Next) -> Response {
    if wants_html(request.headers()) {
        // Only a development override touches the disk. With none configured this is a
        // table lookup and a refcount bump, so dispatching to the blocking pool would
        // cost more than the work it moves off the runtime.
        if override_dir().is_none() {
            return vary(shell(request.headers()));
        }
        let headers = request.headers().clone();
        return vary(
            tokio::task::spawn_blocking(move || shell(&headers))
                .await
                .unwrap_or_else(|_| empty(StatusCode::INTERNAL_SERVER_ERROR)),
        );
    }
    vary(next.run(request).await)
}

/// Mark a response as varying by `Accept`.
///
/// On both branches, and not optional: one URL has two representations chosen by a
/// request header, and the deployment this surface is designed for puts a proxy in
/// front of `/admin`. Without this, that proxy caches whichever representation it saw
/// first and serves it to everyone.
fn vary(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::VARY, HeaderValue::from_static("accept"));
    response
}

/// An empty body with a status, for the handler's 404.
pub fn empty(status: StatusCode) -> Response {
    (status, Body::empty()).into_response()
}

/// Every relative ES module specifier imported by `source`.
///
/// Used by the tests to walk the module graph, and deliberately not by anything that
/// serves: the browser resolves imports, hekla only hands over bytes.
pub fn imports(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    for (index, _) in source.match_indices("./") {
        // A specifier is always inside quotes; find the closing one.
        let rest = &source[index + 2..];
        let end = rest.find(['"', '\'', '`']);
        if let Some(end) = end {
            let name = &rest[..end];
            if !name.is_empty() && !name.contains('/') {
                found.push(name.to_owned());
            }
        }
    }
    found.sort_unstable();
    found.dedup();
    found
}

/// Every URL literal in `source` beginning with one of `prefixes`, with `${...}`
/// interpolations reduced to a single path segment.
///
/// For the test that pins every URL the console builds to a route the router actually
/// serves. It over-collects rather than under-collects on purpose, comments included:
/// a scan that missed one would pass vacuously, and prose naming a route that no
/// longer exists is worth hearing about too.
///
/// The one thing skipped is a path carrying `...`, which is prose by construction: no
/// URL the console builds contains an ellipsis, so excluding it cannot hide a real
/// mismatch.
pub fn routed_urls(source: &str, prefixes: &[&str]) -> Vec<String> {
    const STOP: [char; 10] = ['"', '\'', '`', '?', '#', ')', ',', ' ', '<', '\n'];
    let mut found = Vec::new();
    // A prefix has to *begin* a URL, not land in the middle of a longer one: `/effects`
    // occurs inside `/admin/effects`, and matching there would yield a path that is
    // real-looking, unroutable, and nowhere in the source. So the character before it
    // must be one that can end a token rather than continue a path.
    let begins_url = |index: usize| {
        source[..index]
            .chars()
            .next_back()
            .is_none_or(|before| !before.is_alphanumeric() && !"/-_.~%".contains(before))
    };
    // Deduplicated, because prefixes overlap: `/admin` and `/admin/events` both match
    // the same literal, and scanning it twice would only duplicate work.
    let mut starts: Vec<usize> = prefixes
        .iter()
        .flat_map(|prefix| source.match_indices(prefix).map(|(index, _)| index))
        .filter(|index| begins_url(*index))
        .collect();
    starts.sort_unstable();
    starts.dedup();
    for index in starts {
        let mut url = String::new();
        let mut rest = &source[index..];
        // Interpolations are consumed *before* looking for a terminator, not after: an
        // expression like `${encodeURIComponent(field)}` contains a `)`, and stopping
        // at it would silently truncate the path to something shorter that then fails
        // to match for a reason that has nothing to do with the route table.
        loop {
            let stop = rest.find(STOP);
            let open = rest.find("${");
            match (open, stop) {
                (Some(open), stop) if stop.is_none_or(|stop| open < stop) => {
                    url.push_str(&rest[..open]);
                    url.push_str("{x}");
                    match rest[open..].find('}') {
                        Some(close) => rest = &rest[open + close + 1..],
                        // An unterminated interpolation is not a URL to check.
                        None => {
                            url.clear();
                            break;
                        }
                    }
                }
                (_, Some(stop)) => {
                    url.push_str(&rest[..stop]);
                    break;
                }
                (_, None) => {
                    url.push_str(rest);
                    break;
                }
            }
        }
        let url = url.trim_end_matches('/').to_owned();
        if !url.is_empty() && !url.contains("...") {
            found.push(url);
        }
    }
    found.sort();
    found.dedup();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accepting(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, HeaderValue::from_str(value).unwrap());
        headers
    }

    #[test]
    fn a_browser_asks_for_html_and_every_other_client_does_not() {
        assert!(wants_html(&accepting("text/html")));
        assert!(wants_html(&accepting(
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"
        )));
        assert!(wants_html(&accepting("TEXT/HTML")));
        assert!(
            wants_html(&accepting("text/html, application/json;q=0.9")),
            "an explicit preference for html is honoured"
        );

        // Every real browser outranks its fallbacks, which is what the check turns on.
        for browser in [
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        ] {
            assert!(wants_html(&accepting(browser)), "{browser}");
        }

        // The one that matters: `*/*` is curl's default and a bare `fetch()`'s. If a
        // media range counted as a request for HTML, every existing client's JSON
        // would silently become a web page.
        assert!(!wants_html(&accepting("*/*")));
        assert!(!wants_html(&accepting("application/json")));
        assert!(!wants_html(&HeaderMap::new()));
        assert!(
            wants_html(&accepting("text/*")),
            "`text/*` matches text/html and does not match application/json, so it is \
             a request for the console. Unlike `*/*`, which matches both equally and \
             therefore decides nothing."
        );
        // A refusal is a refusal however it is spelled, and `q=0` has several spellings.
        for refused in [
            "text/html;q=0, application/json",
            "text/html;q=0.0, application/json",
            "text/html;q=0.000, application/json",
            "text/html; q=0, application/json",
        ] {
            assert!(
                !wants_html(&accepting(refused)),
                "naming a type at zero weight refuses it: {refused}"
            );
        }

        assert!(
            !wants_html(&accepting("application/json, text/html;q=0.1")),
            "a stated preference for json is a preference, not a tiebreak to ignore"
        );
        assert!(
            !wants_html(&accepting("application/json, text/html")),
            "equal weights are a genuine tie, and a tie goes to the API: the JSON is \
             the contract and the console is the convenience"
        );
    }

    #[test]
    fn a_name_that_is_not_in_the_table_does_not_resolve() {
        assert!(asset("index.html").is_some());
        for hostile in [
            "..",
            "../index.html",
            "../../etc/passwd",
            "/etc/passwd",
            "index.html/../index.html",
            "",
            "nope.js",
        ] {
            assert!(
                asset(hostile).is_none(),
                "`{hostile}` is not an asset, so the override has nothing to join it onto"
            );
        }
    }

    #[test]
    fn the_override_replaces_a_known_asset_and_falls_back_when_it_cannot() {
        let shell = asset(SHELL).unwrap();
        assert_eq!(
            bytes(shell, None).as_ref(),
            shell.bytes,
            "with no override the compiled-in copy is served"
        );

        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            bytes(shell, Some(dir.path())).as_ref(),
            shell.bytes,
            "an override directory that does not carry the file falls back rather than \
             404s: the common case is a tree holding only the file being edited"
        );

        fs::write(dir.path().join(SHELL), b"<!doctype html>edited").unwrap();
        assert_eq!(
            bytes(shell, Some(dir.path())).as_ref(),
            b"<!doctype html>edited",
            "an override that does carry it wins"
        );
    }

    #[test]
    fn every_asset_is_named_once_and_typed_by_its_extension() {
        let names = asset_names();
        let mut unique = names.clone();
        unique.dedup();
        assert_eq!(names, unique, "a duplicate name would shadow an asset");

        for asset in ASSETS {
            assert!(
                !asset.bytes.is_empty(),
                "`{}` is empty, which is never intentional",
                asset.name
            );
            let expected = match asset.name.rsplit_once('.') {
                Some((_, "html")) => HTML,
                Some((_, "js")) => JS,
                Some((_, "css")) => CSS,
                Some((_, "svg")) => SVG,
                other => panic!("`{}` has an untyped extension: {other:?}", asset.name),
            };
            assert_eq!(
                asset.content_type, expected,
                "`{}` is served as the wrong type",
                asset.name
            );
            assert!(
                !asset.name.contains('/'),
                "`{}` is nested, but the route captures one segment",
                asset.name
            );
        }
    }

    #[test]
    fn the_documents_media_types_drop_the_charset_but_keep_every_type() {
        let content = media_types();
        let keys: Vec<&str> = content
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert!(keys.contains(&"text/html"));
        assert!(keys.contains(&"text/javascript"));
        assert!(
            !keys.iter().any(|key| key.contains(';')),
            "a charset belongs on the wire, not in the document's media type key: {keys:?}"
        );
        for value in content.as_object().unwrap().values() {
            assert!(value.get("schema").is_some(), "every media type needs one");
        }
    }

    #[test]
    fn a_url_scan_normalises_interpolations_to_one_segment() {
        const ADMIN: &[&str] = &["/admin"];
        assert_eq!(
            routed_urls("fetch(`/admin/effects/${name}/invocations`)", ADMIN),
            vec!["/admin/effects/{x}/invocations"]
        );
        assert_eq!(
            routed_urls("'/admin/events?limit=50'", ADMIN),
            vec!["/admin/events"],
            "a query string is not part of the path a route matches"
        );
        assert_eq!(
            routed_urls("`/admin/subjects/${f}/${v}`", ADMIN),
            vec!["/admin/subjects/{x}/{x}"]
        );
        assert_eq!(
            routed_urls(
                "`/admin/subjects/${encodeURIComponent(field)}/${enc(value)}`",
                ADMIN
            ),
            vec!["/admin/subjects/{x}/{x}"],
            "an interpolation may contain the characters that otherwise end a url, so \
             it has to be consumed before one is looked for"
        );
        assert!(
            routed_urls("// see /admin/... for the list", &["/admin"]).is_empty(),
            "an ellipsis is prose by construction: no url the console builds has one"
        );
        assert_eq!(
            routed_urls("`/admin/`", ADMIN),
            vec!["/admin"],
            "a trailing slash is the same route"
        );
    }

    #[test]
    fn an_import_scan_finds_relative_specifiers_only() {
        assert_eq!(
            imports("import { html } from './vendor-preact.js'\nimport './app.js'"),
            vec!["app.js", "vendor-preact.js"]
        );
        assert!(
            imports("import x from 'https://example.test/x.js'").is_empty(),
            "only relative specifiers name an asset"
        );
    }
}
