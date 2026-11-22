//! Passing a miss on to another server (`docs/technical-design/0020-fallback-on-miss.md`).
use std::time::Duration;

use axum::body::Body;
use axum::extract::{OriginalUri, Request, State};
use axum::http::header::{
    CONNECTION, CONTENT_LENGTH, HOST, HeaderMap, HeaderName, HeaderValue, PROXY_AUTHENTICATE,
    PROXY_AUTHORIZATION, TE, TRAILER, TRANSFER_ENCODING, UPGRADE,
};
use axum::http::{Method, Uri};
use axum::middleware::Next;
use axum::response::Response;
use bytes::Bytes;
use reqwest::{Client, Url, redirect};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::error::Miss;
use super::state::AppState;
use crate::config::FallbackSettings;
use crate::error::{Error, Result};

/// Chunks buffered between the fallback and the viewer, so a slow viewer slows the fallback down
/// instead of filling memory.
const BODY_CHUNKS_IN_FLIGHT: usize = 4;

/// Headers that describe one connection, not the message, and so are never passed along.
const HOP_BY_HOP: [HeaderName; 7] = [
    CONNECTION,
    PROXY_AUTHENTICATE,
    PROXY_AUTHORIZATION,
    TE,
    TRAILER,
    TRANSFER_ENCODING,
    UPGRADE,
];

#[derive(Debug, Clone)]
pub(crate) struct Fallback {
    client: Client,
    base: String,
    on_missing_file: bool,
    loop_header: HeaderName,
}

impl Fallback {
    pub(crate) fn new(settings: &FallbackSettings) -> Result<Self> {
        let timeout = Duration::from_millis(settings.timeout_ms);
        let client = Client::builder()
            .connect_timeout(timeout)
            .read_timeout(timeout)
            // A redirect from the fallback is the fallback's answer, passed on as it is.
            .redirect(redirect::Policy::none())
            .build()
            .map_err(|error| Error::Configuration(format!("fallback client: {error}")))?;
        Ok(Self {
            client,
            base: settings.base_url.as_str().trim_end_matches('/').to_owned(),
            on_missing_file: settings.on_missing_file,
            loop_header: HeaderName::from_bytes(settings.loop_header.as_bytes())
                .map_err(|error| Error::Configuration(format!("fallback.loop_header: {error}")))?,
        })
    }

    fn takes(&self, miss: Miss) -> bool {
        match miss {
            Miss::Asset => true,
            Miss::File => self.on_missing_file,
        }
    }

    /// The URL to ask: the fallback's base, then the path and query the viewer asked this server
    /// for, which is the original one even when `[url_compat]` has rewritten it for routing.
    fn target(&self, uri: &Uri) -> Option<Url> {
        let path_and_query = uri.path_and_query()?.as_str();
        Url::parse(&format!("{}{path_and_query}", self.base)).ok()
    }

    /// Asks the fallback. `None` when it cannot be reached, so the viewer gets this server's own
    /// answer rather than a new kind of failure.
    async fn forward(&self, method: Method, uri: &Uri, headers: &HeaderMap) -> Option<Response> {
        let url = self.target(uri)?;
        let mut request = self.client.request(method, url);
        for (name, value) in headers {
            if name != HOST && name != CONTENT_LENGTH && !HOP_BY_HOP.contains(name) {
                request = request.header(name, value);
            }
        }
        request = request.header(&self.loop_header, HeaderValue::from_static("1"));
        let mut answer = match request.send().await {
            Ok(answer) => answer,
            Err(error) => {
                tracing::warn!(
                    event = "fallback_unreachable",
                    error = %error.without_url(),
                );
                return None;
            }
        };
        let status = answer.status();
        let mut response = Response::builder().status(status);
        for (name, value) in answer.headers() {
            if !HOP_BY_HOP.contains(name) {
                response = response.header(name, value);
            }
        }
        let (sender, receiver) = mpsc::channel::<std::io::Result<Bytes>>(BODY_CHUNKS_IN_FLIGHT);
        tokio::spawn(async move {
            loop {
                let chunk = match answer.chunk().await {
                    Ok(Some(chunk)) => Ok(chunk),
                    Ok(None) => return,
                    Err(error) => Err(std::io::Error::other(error.without_url())),
                };
                let failed = chunk.is_err();
                if sender.send(chunk).await.is_err() || failed {
                    return;
                }
            }
        });
        tracing::info!(event = "fallback_served", http.status = status.as_u16());
        response
            .body(Body::from_stream(ReceiverStream::new(receiver)))
            .ok()
    }
}

/// Runs the request here first and, when it ends as a miss the fallback takes, asks the fallback
/// for the same URL. A request that already came through a fallback is never passed on again.
pub(crate) async fn fallback_on_miss(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let Some(fallback) = state.fallback.clone() else {
        return next.run(request).await;
    };
    let forwardable = matches!(*request.method(), Method::GET | Method::HEAD)
        && !request.headers().contains_key(&fallback.loop_header);
    if !forwardable {
        return next.run(request).await;
    }
    let method = request.method().clone();
    let headers = request.headers().clone();
    let uri = request
        .extensions()
        .get::<OriginalUri>()
        .map_or_else(|| request.uri().clone(), |original| original.0.clone());
    let response = next.run(request).await;
    match response.extensions().get::<Miss>() {
        Some(miss) if fallback.takes(*miss) => fallback
            .forward(method, &uri, &headers)
            .await
            .unwrap_or(response),
        _ => response,
    }
}
