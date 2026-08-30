//! The admin console: how it is served, and the two invariants that keep it from
//! rotting.
//!
//! A console is a client of the API sitting in the same repository as the API, which
//! means the usual way it breaks is silent: an endpoint is renamed, or an asset is
//! renamed, and nothing fails until someone opens a page. The two scanning tests below
//! turn both of those into a failing `cargo test`.

use std::collections::BTreeSet;
use std::str;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use hekla::server;
use hekla::ui;
use serde_json::Value;
use tower::ServiceExt;

mod support;

use support::{Boot, Harness};

/// A response's status, content type, and body bytes. The shared helpers in `support`
/// parse every body as JSON, which is exactly what an HTML route must not assume.
async fn fetch(app: &Router, uri: &str, accept: Option<&str>) -> (StatusCode, String, Vec<u8>) {
    let mut request = Request::builder().method(Method::GET).uri(uri);
    if let Some(accept) = accept {
        request = request.header(header::ACCEPT, accept);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, content_type, bytes)
}

fn boot() -> Harness {
    Boot::example().start()
}

fn source(name: &str) -> &'static str {
    str::from_utf8(ui::asset(name).expect("the asset exists").bytes).expect("utf-8")
}

/// Substitute a real value for every `{...}` in a route template, so the negotiation
/// tests can drive every registered path rather than a hand-picked few.
///
/// An unmapped parameter panics rather than passing through. Both callers still go
/// green on a literal `{param}` in the path (the middleware serves the console for
/// any `/admin` path, and the handler 404s with a JSON envelope), so a new route
/// would be exercised without its real representation ever being driven.
fn concrete(route: &str) -> String {
    route
        .split('/')
        .map(|segment| match segment {
            "{position}" => "1",
            "{correlation_id}" => "3f2504e0-4f89-41d3-9a0c-0305e82c3301",
            "{name}" => "SendWelcome",
            "{field}" => "user_id",
            "{value}" => "nobody",
            other if other.starts_with('{') => {
                panic!("`{other}` in `{route}` has no test value; add one to `concrete`")
            }
            other => other,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Every `/admin` route except the asset route, which is not negotiated.
fn negotiated_routes() -> Vec<&'static str> {
    server::routes()
        .into_iter()
        .filter(|route| route.starts_with(server::ADMIN_ROUTE))
        .filter(|route| *route != server::ADMIN_ASSETS_ROUTE)
        .collect()
}

// --- the two that keep the console honest ----------------------------------

/// Every URL the console builds resolves to a route the router serves.
///
/// This is the test that makes renaming an endpoint safe: without it, the rename
/// compiles, every Rust test passes, and the console 404s at runtime on a page nobody
/// opened during review.
///
/// It covers the whole surface the console touches, not just `/admin`. The two write
/// endpoints it drives live outside that prefix, and they are the ones whose silent
/// breakage costs most: `replay` and `skip` are the only actions here that cannot be
/// undone, so a Skip button that 404s is the worst of the failures this prevents.
#[test]
fn every_url_the_console_builds_is_a_route_the_router_serves() {
    let routes = server::routes();
    // Derived from the router rather than written down, so a new top-level prefix is
    // scanned the moment it is served.
    let prefixes: Vec<&str> = {
        let mut seen: Vec<&str> = routes
            .iter()
            .filter_map(|route| route.split('/').nth(1))
            .filter(|segment| !segment.is_empty() && !segment.starts_with('{'))
            .map(route_prefix)
            .collect();
        seen.sort_unstable();
        seen.dedup();
        seen
    };
    assert!(prefixes.len() >= 6, "prefixes look wrong: {prefixes:?}");

    let mut checked = 0;
    for asset in ui::ASSETS {
        let Ok(text) = str::from_utf8(asset.bytes) else {
            continue;
        };
        for url in ui::routed_urls(text, &prefixes) {
            let covered = routes.iter().any(|route| covers(route, &url));
            assert!(
                covered,
                "`{}` builds `{url}`, which no route serves. Routes: {routes:?}",
                asset.name
            );
            checked += 1;
        }
    }
    // Without this the test passes vacuously the moment the console changes how it
    // spells a URL, which is precisely when it is most needed.
    assert!(
        checked > 20,
        "only {checked} routed urls found in the console's own source, which is fewer \
         than it has views: the scan has stopped matching how the URLs are written"
    );
}

/// `"events"` to `"/events"`, leaked so the prefixes can be borrowed as `&[&str]`.
/// A handful of short strings for the life of one test process.
fn route_prefix(segment: &str) -> &'static str {
    Box::leak(format!("/{segment}").into_boxed_str())
}

/// Whether a route template covers a concrete or interpolated URL. The mirror of
/// `describes` in `tests/openapi.rs`: there the concrete segment comes from the
/// document, here it comes from the JavaScript.
fn covers(route: &str, url: &str) -> bool {
    let route: Vec<&str> = route.split('/').collect();
    let url: Vec<&str> = url.split('/').collect();
    route.len() == url.len()
        && route
            .iter()
            .zip(&url)
            .all(|(expected, actual)| expected == actual || expected.starts_with('{'))
}

/// Every asset the console references exists, and every asset that exists is reached.
#[test]
fn the_asset_graph_has_no_dangling_reference_and_no_dead_file() {
    let shell = source("index.html");
    let mut referenced: BTreeSet<String> = BTreeSet::new();

    // What the shell pulls in directly, by absolute URL.
    for (index, _) in shell.match_indices("/admin/assets/") {
        let rest = &shell[index + "/admin/assets/".len()..];
        let end = rest.find(['"', '\'', ' ', '>']).unwrap_or(rest.len());
        referenced.insert(rest[..end].to_owned());
    }
    assert!(
        !referenced.is_empty(),
        "a shell that references nothing proves nothing"
    );

    // Then the module graph the browser walks from there.
    for asset in ui::ASSETS {
        if let Ok(text) = str::from_utf8(asset.bytes) {
            referenced.extend(ui::imports(text));
        }
    }

    for name in &referenced {
        assert!(
            ui::asset(name).is_some(),
            "the console references `{name}`, which the binary does not carry"
        );
    }
    for asset in ui::ASSETS {
        // The shell is the entry point, so nothing imports it.
        if asset.name == "index.html" {
            continue;
        }
        assert!(
            referenced.contains(asset.name),
            "`{}` is compiled into every binary and nothing references it",
            asset.name
        );
    }
}

// --- negotiation -----------------------------------------------------------

#[tokio::test]
async fn a_browser_gets_the_console_from_every_admin_path() {
    let harness = boot();
    let app = harness.app();
    let shell = ui::asset(ui::SHELL).unwrap().bytes;

    for route in negotiated_routes() {
        let uri = concrete(route);
        let (status, content_type, body) = await_html(&app, &uri).await;
        assert_eq!(status, StatusCode::OK, "{uri} should serve the console");
        assert!(
            content_type.starts_with("text/html"),
            "{uri} served `{content_type}`"
        );
        assert_eq!(body, shell, "{uri} served something other than the shell");
    }

    harness.shutdown();
}

async fn await_html(app: &Router, uri: &str) -> (StatusCode, String, Vec<u8>) {
    fetch(
        app,
        uri,
        Some("text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"),
    )
    .await
}

/// The failure this guards against would break every existing client at once: `*/*` is
/// what curl sends by default, and what a bare `fetch()` sends.
#[tokio::test]
async fn a_client_that_accepts_anything_still_gets_json() {
    let harness = boot();
    let app = harness.app();

    for accept in [None, Some("*/*"), Some("application/json")] {
        for route in negotiated_routes() {
            let uri = concrete(route);
            let (_, content_type, body) = fetch(&app, &uri, accept).await;
            assert!(
                content_type.starts_with("application/json"),
                "{uri} with Accept={accept:?} served `{content_type}`"
            );
            serde_json::from_slice::<Value>(&body)
                .unwrap_or_else(|err| panic!("{uri} with Accept={accept:?} is not json: {err}"));
        }
    }

    harness.shutdown();
}

/// The index lists every `/admin` route, derived from the router rather than counted.
///
/// A literal here would be a second hand-maintained copy of the route table, which is
/// the thing `admin_index` already is: adding a route and forgetting the index entry
/// has to fail with the route's own name, not with a number that moved.
#[tokio::test]
async fn the_json_index_lists_every_admin_route_and_points_at_the_console() {
    let harness = boot();
    let app = harness.app();

    let (status, _, body) = fetch(&app, "/admin", Some("application/json")).await;
    assert_eq!(status, StatusCode::OK);
    let index: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(index["console"], "/admin");

    let listed: BTreeSet<&str> = index["endpoints"]
        .as_array()
        .expect("endpoints is an array")
        .iter()
        .map(|entry| entry["path"].as_str().expect("every entry has a path"))
        .collect();
    // The index does not list itself, and the asset route is the console's own
    // plumbing rather than an endpoint a client would call.
    let expected: BTreeSet<&str> = server::routes()
        .into_iter()
        .filter(|route| route.starts_with(server::ADMIN_ROUTE))
        .filter(|route| *route != server::ADMIN_ROUTE && *route != server::ADMIN_ASSETS_ROUTE)
        .collect();
    assert_eq!(
        listed, expected,
        "the index and the router disagree about what is under /admin"
    );

    harness.shutdown();
}

#[tokio::test]
async fn a_deep_link_to_a_name_that_does_not_exist_still_opens_the_console() {
    let harness = boot();
    let app = harness.app();

    // Short-circuiting before the handler is what makes this work. The console then
    // fetches the same URL as JSON and renders the 404 itself, which is a better
    // answer than a browser showing a bare error document.
    let (status, content_type, _) = await_html(&app, "/admin/effects/no-such-effect").await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.starts_with("text/html"));

    let (status, _, _) = fetch(
        &app,
        "/admin/effects/no-such-effect",
        Some("application/json"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "the data answer is unchanged"
    );

    harness.shutdown();
}

/// The negotiation must not answer for a method the route does not serve.
///
/// `MethodRouter::layer` wraps the method-not-allowed fallback along with the handlers,
/// so a layer attached that way short-circuits before the method is ever checked and
/// turns a 405 into a 200 page. `route_layer` is the axum API that exists to exclude
/// the fallback, and this is what tells the two apart.
#[tokio::test]
async fn a_method_the_route_does_not_serve_is_405_even_for_a_browser() {
    let harness = boot();
    let app = harness.app();

    for method in [Method::POST, Method::PUT, Method::DELETE] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method.clone())
                    .uri("/admin/events")
                    .header(header::ACCEPT, "text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} /admin/events answered {} instead of 405",
            response.status()
        );
    }

    harness.shutdown();
}

#[tokio::test]
async fn a_path_outside_admin_is_never_negotiated() {
    let harness = boot();
    let app = harness.app();

    for uri in ["/status", "/health", "/openapi.json"] {
        let (_, content_type, _) = await_html(&app, uri).await;
        assert!(
            content_type.starts_with("application/json"),
            "{uri} is not part of the console and must answer as itself: `{content_type}`"
        );
    }

    // `/docs` is HTML, but its own, and must not become the console.
    let (_, _, body) = await_html(&app, "/docs").await;
    assert!(
        String::from_utf8_lossy(&body).contains("scalar"),
        "the api reference must not be replaced by the console"
    );

    harness.shutdown();
}

#[tokio::test]
async fn every_admin_response_says_it_varies_by_accept() {
    let harness = boot();
    let app = harness.app();

    // One URL with two representations chosen by a request header. Without this a
    // proxy in front of `/admin` caches whichever it saw first and serves it to
    // everyone, which is the deployment this surface is designed for.
    for accept in [Some("text/html"), Some("application/json")] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/events")
                    .header(header::ACCEPT, accept.unwrap())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let vary = response
            .headers()
            .get(header::VARY)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        assert!(
            vary.contains("accept"),
            "Accept={accept:?} carried Vary: {vary}"
        );
    }

    harness.shutdown();
}

// --- assets ----------------------------------------------------------------

#[tokio::test]
async fn every_asset_serves_with_its_declared_type_and_is_not_negotiated() {
    let harness = boot();
    let app = harness.app();

    for asset in ui::ASSETS {
        let uri = format!("/admin/assets/{}", asset.name);
        // Asking for HTML must still get the file: this route delivers the console
        // rather than being a view of it, so negotiating it would make the shell
        // import itself.
        let (status, content_type, body) = await_html(&app, &uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(content_type, asset.content_type, "{uri}");
        // Against whatever the server would serve, which is the on-disk copy when a
        // developer has `HEKLA_UI_DIR` exported: the documented dev workflow must not
        // make `cargo test` fail in the shell it is documented for.
        assert_eq!(
            body,
            ui::bytes(asset, ui::override_dir()).into_owned(),
            "{uri} served the wrong bytes"
        );
    }

    harness.shutdown();
}

#[tokio::test]
async fn an_unknown_asset_is_a_404_and_a_traversal_is_not_a_file() {
    let harness = boot();
    let app = harness.app();

    for uri in [
        "/admin/assets/nope.js",
        "/admin/assets/..%2f..%2fetc%2fpasswd",
        "/admin/assets/%2e%2e",
        "/admin/assets/Cargo.toml",
    ] {
        let (status, content_type, body) = fetch(&app, uri, None).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{uri} resolved to something, but the asset table is the whole namespace"
        );
        // The generated document declares this 404 as the shared `Error` envelope, so
        // a client that deserializes what was promised must not receive an empty body.
        assert!(content_type.starts_with("application/json"), "{uri}");
        let error: Value = serde_json::from_slice(&body)
            .unwrap_or_else(|err| panic!("{uri} answered a body that is not json: {err}"));
        assert_eq!(error["error"]["code"], "not_found", "{uri}");
    }

    harness.shutdown();
}

#[tokio::test]
async fn the_console_is_served_with_a_revalidating_cache_header() {
    let harness = boot();
    let app = harness.app();

    // The bytes change with the binary and not with the URL, so a cached copy must
    // never shadow a newly deployed one. Under a development override they change
    // under a fixed binary, so nothing may be cached at all.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/assets/app.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let expected = if ui::override_dir().is_some() {
        "no-store"
    } else {
        "no-cache"
    };
    assert_eq!(
        response
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|value| value.to_str().ok()),
        Some(expected),
    );

    harness.shutdown();
}

/// `no-cache` without a validator is a full re-download of the whole console on every
/// page load, which is the opposite of what the header is there to arrange. This is
/// the half that makes it true.
#[tokio::test]
async fn a_revalidated_asset_is_a_304_with_no_body() {
    let harness = boot();
    let app = harness.app();

    let asset = ui::asset("app.js").unwrap();
    let Some(etag) = ui::etag(asset) else {
        // A development override deliberately has no validator: its bytes change under
        // a fixed binary, which is what `no-store` says.
        harness.shutdown();
        return;
    };

    let (status, _, body) = fetch(&app, "/admin/assets/app.js", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.is_empty());

    // Both spellings a cache may send back: the tag it was given, and the same tag
    // weakened, which a shared cache is allowed to do.
    for candidate in [etag.to_owned(), format!("W/{etag}"), "*".to_owned()] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/assets/app.js")
                    .header(header::IF_NONE_MATCH, candidate.as_str())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::NOT_MODIFIED,
            "If-None-Match: {candidate} re-sent the body"
        );
        // A 304 updates the stored response's headers, so it has to carry both again.
        assert_eq!(
            response
                .headers()
                .get(header::ETAG)
                .and_then(|value| value.to_str().ok()),
            Some(etag)
        );
        assert!(response.headers().get(header::CACHE_CONTROL).is_some());
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(body.is_empty(), "a 304 must not carry a body");
    }

    // A stale validator is a miss, not a match.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/assets/app.js")
                .header(header::IF_NONE_MATCH, "\"not-the-current-digest\"")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // The validator is over the bytes, so two different files never share one.
    let other = ui::etag(ui::asset("router.js").unwrap()).unwrap();
    assert_ne!(etag, other);

    harness.shutdown();
}

/// The shell sets the stored theme before the first paint, which a module script
/// cannot do: `type="module"` is deferred, so it runs after the document is painted
/// and an explicit choice that disagrees with the system one flashes.
///
/// The storage key is therefore spelled twice, once in each language. This is what
/// keeps the two in step.
#[test]
fn the_shell_applies_the_stored_theme_before_the_first_paint() {
    let shell = source("index.html");
    let theme = source("theme.js");

    let key = theme
        .lines()
        .find_map(|line| line.trim().strip_prefix("const KEY = "))
        .map(|value| value.trim().trim_matches(['\'', '"']))
        .expect("theme.js declares its storage key as `const KEY = '...'`");

    let inline = shell
        .split("<script")
        // The first chunk is everything before any script tag.
        .skip(1)
        .find(|block| !block.starts_with(" type=\"module\""))
        .expect("the shell carries a non-module script");
    assert!(
        inline.contains(key),
        "the shell's inline script does not read `{key}`, so an explicit theme flashes \
         until app.js loads"
    );
    assert!(
        inline.contains("data-theme") || inline.contains("dataset.theme"),
        "the shell's inline script reads the key but never applies it"
    );
}

// --- the document ----------------------------------------------------------

#[test]
fn the_document_lists_exactly_the_files_the_binary_carries() {
    let project = support::load_ok(&support::example_dir("users"));
    let doc = hekla::openapi::build(&hekla::openapi::Surface::from_project(&project));
    let path = &doc["paths"][server::ADMIN_ASSETS_ROUTE]["get"];

    let declared: Vec<&str> = path["parameters"][0]["schema"]["enum"]
        .as_array()
        .expect("the file parameter is a closed set")
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    assert_eq!(
        declared,
        ui::asset_names(),
        "adding a file without the document noticing leaves the spec lying"
    );

    let media: Vec<&String> = path["responses"]["200"]["content"]
        .as_object()
        .unwrap()
        .keys()
        .collect();
    assert!(media.iter().any(|key| *key == "text/javascript"));
    assert!(
        !media.iter().any(|key| key.contains(';')),
        "a charset belongs on the wire, not in a media type key: {media:?}"
    );
}
