//! **A code-search site over a wire client.**
//!
//! Glean's demo is `glean-hyperlink`: a file list, a file rendered with `<a>` tags
//! spliced over the byte spans of its cross-references, and a jump to an offset. This
//! is that, plus the three things the demo implies and Glass actually serves — search,
//! find-references, and a symbol panel — against an Aperture database over the
//! ordinary wire protocol.
//!
//! [`docs/phase-11-code-search.md`](../../../docs/phase-11-code-search.md) is the
//! analysis this was built from: which query each screen needs, and which of them
//! seeks. Every one of them seeks now, and four schema predicates exist because of
//! it.
//!
//! # What it is not
//!
//! **It reaches nothing below [`aperture_client`].** No store, no engine, no plan: a
//! viewer is an ordinary consumer of the protocol, and one that reached past the
//! client would be a second server rather than a demonstration that one is enough.
//! Everything it knows about the data is in [`query`], which is one module for the
//! same reason `aperture_cli::workload` is one module — a query written where it is
//! used is a query nobody can cost.
//!
//! # Blocking client, async server
//!
//! [`aperture_client`] is synchronous by design. So every question runs on a blocking
//! thread with a connection checked out of a [`pool`], which is the ordinary shape for
//! a blocking client behind an async front end — and the shape `bench/FINDINGS.md` §7
//! named as the one to size RAM for. See [`pool`] for what that costs now.

pub mod pool;
pub mod query;
pub mod render;

use std::sync::Arc;

use axum::{
    Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::get,
};

use aperture_client::ClientError;
use aperture_schema::schema::Schema;

use crate::{pool::Pool, query::Paths};

/// How many rows a listing page carries.
const PAGE: usize = 200;

/// How many search hits to show.
///
/// `bench/FINDINGS.md` §11 measured page size as a dial rather than a free choice:
/// the dominant per-row cost is the framing and the client's decode, so 50 rows costs
/// 12% less throughput than 100 and 36% less than 256. Fifty is what that finding
/// recommends for an interactive box.
const HITS: u64 = 50;

/// Everything a request needs.
pub struct App {
    pool: Pool,
    paths: Paths,
}

impl App {
    /// Connect, load the file list, and hand back a router.
    ///
    /// The file list is loaded **once**, at startup, and the reason is in
    /// [`query::Paths`]: a reference answers a file *id*, and nothing in the language
    /// turns one into a path.
    ///
    /// # Errors
    ///
    /// If the server will not answer — no socket, no such database, or a query that
    /// does not compile, which for these queries means the database's schema is not
    /// the code index.
    pub fn open(
        socket: impl Into<std::path::PathBuf>,
        database: impl Into<String>,
        schema: Arc<Schema>,
        pool_size: usize,
    ) -> Result<App, ClientError> {
        let pool = Pool::new(socket, database, schema, pool_size);
        let paths = pool.with(Paths::load)?;

        Ok(App { pool, paths })
    }

    #[must_use]
    pub fn files(&self) -> usize {
        self.paths.len()
    }

    /// The routes, in the order a person meets them.
    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route("/", get(browse))
            .route("/file/{*path}", get(file))
            .route("/search", get(search))
            .route("/symbol/{*name}", get(symbol))
            .route("/health", get(health))
            .with_state(self)
    }
}

type Shared = State<Arc<App>>;

/// A query failure is the page's, not the process's.
///
/// Rendered rather than logged and swallowed: a viewer whose page went blank when the
/// server said no would send whoever is looking at it to the wrong place, and the
/// server's own wording is the thing that says what happened.
fn failed(what: &str, error: &ClientError) -> Html<String> {
    Html(render::page(
        "error",
        "",
        &format!(
            "<h1>{}</h1><p class=\"muted\">{}</p>",
            render::escape(what),
            render::escape(&error.to_string())
        ),
    ))
}

async fn health(State(app): Shared) -> impl IntoResponse {
    // Answered by asking the *database*, not by returning a constant: a health check
    // that does not touch the thing being checked is a health check for the process.
    let files = app.paths.len();
    match run(&app, |c| c.count("F where src.File F")).await {
        Ok(count) => (
            StatusCode::OK,
            format!("ok\nfiles indexed {files}\nfiles now {count}\n"),
        ),
        Err(error) => (StatusCode::BAD_GATEWAY, format!("unavailable: {error}\n")),
    }
}

/// Run a blocking client call off the reactor.
async fn run<T: Send + 'static>(
    app: &Arc<App>,
    f: impl FnOnce(&mut aperture_client::Connection) -> Result<T, ClientError> + Send + 'static,
) -> Result<T, ClientError> {
    let app = Arc::clone(app);

    tokio::task::spawn_blocking(move || app.pool.with(f))
        .await
        .map_err(|error| ClientError::Protocol(format!("the worker died: {error}")))?
}

#[derive(serde::Deserialize, Default)]
pub struct Browse {
    #[serde(default)]
    path: String,
}

/// The file list, filtered by a path prefix.
async fn browse(State(app): Shared, Query(args): Query<Browse>) -> impl IntoResponse {
    let (paths, more) = app.paths.under(&args.path, PAGE);

    let mut body = String::new();
    body.push_str(&format!(
        "<h1>{}</h1>",
        if args.path.is_empty() {
            "/".to_owned()
        } else {
            render::escape(&args.path)
        }
    ));

    body.push_str(&format!(
        "<p class=\"stat\">{} of {} files{}</p>",
        paths.len(),
        app.paths.len(),
        if more { ", more below the cut" } else { "" }
    ));

    body.push_str(
        "<form action=\"/\" method=\"get\" style=\"margin-bottom:12px\">\
         <input type=\"text\" name=\"path\" value=\"",
    );
    body.push_str(&render::escape(&args.path));
    body.push_str("\" placeholder=\"path prefix\"><button type=\"submit\">filter</button></form>");

    body.push_str("<ul class=\"list\">");
    for path in &paths {
        body.push_str(&format!(
            "<li><a href=\"/file/{}\">{}</a></li>",
            render::url(path),
            render::escape(path)
        ));
    }
    body.push_str("</ul>");

    Html(render::page("files", "", &body))
}

/// One file: its source, with a link over every cross-reference, and its outline.
///
/// **Jumping to a line is a fragment, not a query.** Glean's demo takes `?offset=N`
/// and injects a script to scroll to it, because it renders no line numbers to anchor
/// on. Every line here is a `<code id="Ln">`, so `#L42` is the browser's job and the
/// server does nothing for it.
async fn file(State(app): Shared, Path(path): Path<String>) -> axum::response::Response {
    let text_query = query::file_text(&path);
    let xref_query = query::file_xrefs(&path);
    let outline_query = query::file_outline(&path);

    let rows = run(&app, move |c| {
        // Three questions, one connection, in the order the page needs them. Not
        // batched, because the protocol has no batching — a symbol panel pays the
        // same round trips, which `docs/phase-11-code-search.md` §3 records.
        let text = query::drain(c, &text_query)?;
        let xrefs = query::drain(c, &xref_query)?;
        let outline = query::drain(c, &outline_query)?;
        Ok((text, xrefs, outline))
    })
    .await;

    let (text, xrefs, outline) = match rows {
        Ok(rows) => rows,
        Err(error) => {
            return (StatusCode::BAD_GATEWAY, failed(&path, &error)).into_response();
        }
    };

    if text.is_empty() {
        return (
            StatusCode::NOT_FOUND,
            Html(render::page(
                &path,
                "",
                &format!(
                    "<h1>{}</h1><p class=\"muted\">no source for this path — the \
                     indexer stores line text only for the files it read.</p>",
                    render::escape(&path)
                ),
            )),
        )
            .into_response();
    }

    let lines: Vec<(i64, String)> = text
        .iter()
        .map(|row| (row.int("line"), row.str("text").to_owned()))
        .collect();

    let links: Vec<(i64, i64, i64, String, String)> = xrefs
        .iter()
        .map(|row| {
            let name = row.str("name");
            let target_line = row.int("target_line");
            let target = row.id("target_file").and_then(|id| app.paths.path_of(id));

            let href = match target {
                Some(file) => format!("/file/{}#L{}", render::url(file), target_line),
                // A reference whose target is outside this index — the framework, a
                // package. There is nowhere to go, so it is still marked but only
                // links to the search for its name.
                None => format!("/symbol/{}", render::url(name)),
            };

            (
                row.int("line"),
                row.int("col"),
                row.int("length"),
                href,
                match target {
                    Some(file) => format!("{name} — {file}:{target_line}"),
                    None => format!("{name} — not in this index"),
                },
            )
        })
        .collect();

    let mut body = format!("<h1>{}</h1>", render::escape(&path));

    body.push_str(&format!(
        "<p class=\"stat\">{} lines · {} references · {} declarations</p>",
        lines.len(),
        links.len(),
        outline.len()
    ));

    if !outline.is_empty() {
        body.push_str(
            "<details style=\"margin-bottom:12px\"><summary>outline</summary><ul class=\"list\">",
        );
        for row in &outline {
            let name = row.str("name");
            let line = row.int("line");
            let kind = row.str("kind");
            body.push_str(&format!(
                "<li><a href=\"#L{line}\">{}</a> <span class=\"kind\">{}</span> \
                 <a class=\"muted\" href=\"/symbol/{}\">uses</a></li>",
                render::escape(name),
                render::escape(kind),
                render::url(name),
            ));
        }
        body.push_str("</ul></details>");
    }

    body.push_str(&render::source(&lines, &links));

    Html(render::page(&path, "", &body)).into_response()
}

#[derive(serde::Deserialize, Default)]
pub struct Term {
    #[serde(default)]
    q: String,
}

/// Symbol search: a case-insensitive prefix, and an honest total.
async fn search(State(app): Shared, Query(args): Query<Term>) -> impl IntoResponse {
    let term = args.q.trim().to_owned();

    if term.is_empty() {
        return Html(render::page(
            "search",
            "",
            "<p class=\"muted\">type a symbol name. Matching is a case-insensitive \
             prefix, which is a seek into <code>src.SearchByLowerName</code>.</p>",
        ));
    }

    let hits_query = query::search(&term);
    let count_query = hits_query.clone();
    let shown = term.clone();

    let found = run(&app, move |c| {
        // **Counted, then paged.** The count is the same plan with a different
        // accumulator and never encodes a row, so "1,234 results" costs the scan
        // rather than the scan plus the wire.
        let total = c.count(&count_query)?;
        let (rows, _) = query::page(c, &hits_query, HITS, None)?;
        Ok((total, rows))
    })
    .await;

    let (total, rows) = match found {
        Ok(found) => found,
        Err(error) => return failed(&shown, &error),
    };

    let mut body = format!("<h1>{}</h1>", render::escape(&shown));
    body.push_str(&format!(
        "<p class=\"stat\">{total} match{}{}</p>",
        if total == 1 { "" } else { "es" },
        if total > rows.len() as u64 {
            format!(", showing the first {}", rows.len())
        } else {
            String::new()
        }
    ));

    body.push_str("<table class=\"rows\">");
    for row in &rows {
        let name = row.str("name");
        let line = row.int("line");
        let kind = row.str("kind");
        let file = row.id("file").and_then(|id| app.paths.path_of(id));

        body.push_str(&format!(
            "<tr><td><a href=\"/symbol/{}\">{}</a></td><td class=\"kind\">{}</td><td>",
            render::url(name),
            render::escape(name),
            render::escape(kind),
        ));

        match file {
            Some(file) => body.push_str(&format!(
                "<a href=\"/file/{}#L{line}\">{}:{line}</a>",
                render::url(file),
                render::escape(file)
            )),
            None => body.push_str("<span class=\"muted\">—</span>"),
        }

        body.push_str("</td></tr>");
    }
    body.push_str("</table>");

    Html(render::page(&shown, &shown, &body))
}

/// One symbol: where it is defined, how far it runs, and everywhere it is used.
async fn symbol(State(app): Shared, Path(name): Path<String>) -> impl IntoResponse {
    let def_query = query::definition(&name);
    let span_query = query::definition_span(&name);
    let ref_query = query::references(&name);
    let count_query = ref_query.clone();
    let shown = name.clone();

    let found = run(&app, move |c| {
        let definitions = query::drain(c, &def_query)?;
        let spans = query::drain(c, &span_query)?;
        let total = c.count(&count_query)?;
        let (uses, _) = query::page(c, &ref_query, HITS, None)?;
        Ok((definitions, spans, total, uses))
    })
    .await;

    let (definitions, spans, total, uses) = match found {
        Ok(found) => found,
        Err(error) => return failed(&shown, &error),
    };

    let mut body = format!("<h1>{}</h1>", render::escape(&shown));

    if definitions.is_empty() {
        body.push_str("<p class=\"muted\">no declaration by that name in this index.</p>");
        return Html(render::page(&shown, &shown, &body));
    }

    body.push_str("<h2 style=\"font-size:13px\">declared</h2><table class=\"rows\">");
    for (at, row) in definitions.iter().enumerate() {
        let line = row.int("line");
        let kind = row.str("kind");
        let file = row.id("file").and_then(|id| app.paths.path_of(id));

        // The span is a sibling predicate keyed by the declaration, so the rows line
        // up one for one when every declaration has one — and a missing span is a
        // declaration a partial index never measured, not an error.
        let ends = spans.get(at).map_or(0, |s| s.int("end_line"));

        body.push_str("<tr><td class=\"kind\">");
        body.push_str(&render::escape(kind));
        body.push_str("</td><td>");

        match file {
            Some(file) => body.push_str(&format!(
                "<a href=\"/file/{}#L{line}\">{}:{line}</a>",
                render::url(file),
                render::escape(file)
            )),
            None => body.push_str("<span class=\"muted\">—</span>"),
        }

        if ends > line {
            body.push_str(&format!(
                "</td><td class=\"muted\">{} lines</td></tr>",
                ends - line + 1
            ));
        } else {
            body.push_str("</td><td></td></tr>");
        }
    }
    body.push_str("</table>");

    body.push_str(&format!(
        "<h2 style=\"font-size:13px;margin-top:20px\">used {total} time{}</h2>",
        if total == 1 { "" } else { "s" }
    ));

    if total > uses.len() as u64 {
        body.push_str(&format!(
            "<p class=\"stat\">showing the first {}</p>",
            uses.len()
        ));
    }

    body.push_str("<table class=\"rows\">");
    for row in &uses {
        let file = row.id("file").and_then(|id| app.paths.path_of(id));
        let line = row.int("line");
        let col = row.int("col");

        body.push_str("<tr><td>");
        match file {
            Some(file) => body.push_str(&format!(
                "<a href=\"/file/{}#L{line}\">{}</a>",
                render::url(file),
                render::escape(file)
            )),
            None => body.push_str("<span class=\"muted\">an unindexed file</span>"),
        }
        body.push_str(&format!(
            "</td><td class=\"n\">{line}</td><td class=\"n muted\">{col}</td></tr>"
        ));
    }
    body.push_str("</table>");

    Html(render::page(&shown, &shown, &body))
}
