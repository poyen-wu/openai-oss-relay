//! A tiny reverse‑proxy that rewrites OpenAI‑style chat completions.
//!
//! * Non‑chat endpoints are proxied unchanged.
//! * `/v1/chat/completions` responses are inspected:
//!     - If the response is JSON, every `choices[*].message.content` string is
//!       scanned for the configured open/close patterns and those patterns are
//!       replaced by the configured tags.
//!     - If the response is an SSE stream (`text/event-stream`) each event chunk
//!       undergoes the same rewrite while preserving streaming semantics.
//!
//! ## Configuration (environment variables)
//!
//! | Variable        | Default            |
//! |-----------------|--------------------|
//! | `LISTEN_ADDR`   | `0.0.0.0`          |
//! | `LISTEN_PORT`   | `8080`             |
//! | `UPSTREAM_HOST` | `127.0.0.1`        |
//! | `UPSTREAM_PORT` | `1234`             |
//!
//! The open/close patterns and their replacements can be customised by editing
//! `ReplacementConfig::default()`. Connection settings (listen address, port,
//! upstream host, upstream port) can be customized via `ConnectionConfig::default()`.
//!
//! Editing `EnvConfig::default()` to change the environment variable names to read from.

use std::{
    convert::Infallible,
    env,
    net::SocketAddr,
    sync::Arc,
};

use bytes::{Bytes, BytesMut};
use futures::{ready, stream::StreamExt, Stream};
use hyper::{
    body::to_bytes,
    client::HttpConnector,
    header::{HeaderValue, CONTENT_LENGTH, CONTENT_TYPE, HOST},
    server::Server,
    service::{make_service_fn, service_fn},
    Body, Client, Request, Response, StatusCode, Uri,
};
use serde_json::Value;
use tracing::{error, info};

/// ---------------------------------------------------------------------------
/// Default values
/// ---------------------------------------------------------------------------

/// The default configuration for tag replacement rules.
///
/// Edit these values to change the defaults.
impl Default for ReplacementConfig {
    fn default() -> Self {
        Self {
            // Pattern that marks the start of a block to be replaced.
            open_pattern: "<|channel|>analysis<|message|>",
            // Pattern that marks the end of a block to be replaced.
            close_pattern: "<|end|><|start|>assistant<|channel|>final<|message|>",
            // New open pattern to use.
            open_tag: "<think>",
            // New close pattern to use.
            close_tag: "</think>",
        }
    }
}

/// The default configuration for connections.
///
/// Edit these values to change the defaults.
impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0".into(),
            listen_port: 8080,
            upstream_host: "127.0.0.1".into(),
            upstream_port: 1234,
        }
    }
}

/// Environment variable name configuration.
///
/// Allows customizing the names of environment variables.
/// Edit these values to change the defaults.
impl Default for EnvConfig {
    fn default() -> Self {
        Self {
            // Name of the env var for listen address.
            listen_addr: "LISTEN_ADDR",
            // Name of the env var for listen port.
            listen_port: "LISTEN_PORT",
            // Name of the env var for upstream host.
            upstream_host: "UPSTREAM_HOST",
            // Name of the env var for upstream port.
            upstream_port: "UPSTREAM_PORT",
        }
    }
}

/// ---------------------------------------------------------------------------
/// Configuration
/// ---------------------------------------------------------------------------
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// Configuration for connections.
    pub connect_cfg: ConnectionConfig,
    /// Replacement rules for chat completions.
    pub replace_cfg: ReplacementConfig,
}

impl ProxyConfig {
    /// Build a configuration from environment variables.
    ///
    /// Use default values if the environment variable is not defined.
    pub fn from_env() -> Self {
        let conn_defaults = ConnectionConfig::default();
        let env_cfg = EnvConfig::default();

        // Override with environment variables if set.
        let listen_addr = env::var(env_cfg.listen_addr)
            .unwrap_or_else(|_| conn_defaults.listen_addr.clone());
        let listen_port = env::var(env_cfg.listen_port)
            .ok()
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(conn_defaults.listen_port);

        let upstream_host = env::var(env_cfg.upstream_host)
            .unwrap_or_else(|_| conn_defaults.upstream_host.clone());
        let upstream_port = env::var(env_cfg.upstream_port)
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(conn_defaults.upstream_port);

        Self {
            connect_cfg: ConnectionConfig {
                listen_addr,
                listen_port,
                upstream_host,
                upstream_port,
            },
            replace_cfg: ReplacementConfig::default(),
        }
    }

    /// Authority string used for the `Host` header (e.g. `"127.0.0.1:1234"`).
    fn upstream_authority(&self) -> String {
        format!("{}:{}",
            self.connect_cfg.upstream_host,
            self.connect_cfg.upstream_port)
    }

    /// Construct a full URI that points to the upstream service while preserving
    /// path and query string from the original request.
    fn build_upstream_uri(&self, orig_uri: &Uri) -> Uri {
        let pq = orig_uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
        let uri_string = format!("http://{}{}", self.upstream_authority(), pq);
        uri_string.parse().expect("failed to parse upstream URI")
    }
}

/// ---------------------------------------------------------------------------
/// Replacement rules
/// ---------------------------------------------------------------------------
#[derive(Debug, Clone)]
pub struct ReplacementConfig {
    pub open_pattern: &'static str,
    pub close_pattern: &'static str,
    pub open_tag: &'static str,
    pub close_tag: &'static str,
}

/// ---------------------------------------------------------------------------
/// Connection configuration
/// ---------------------------------------------------------------------------
#[derive(Debug, Clone)]
pub struct ConnectionConfig {
    /// Listening address for the proxy.
    pub listen_addr: String,
    /// Port on which this proxy listens.
    pub listen_port: u16,
    /// Host of the upstream service.
    pub upstream_host: String,
    /// Port of the upstream service.
    pub upstream_port: u16,
}

/// ---------------------------------------------------------------------------
/// Environment variable configuration
/// ---------------------------------------------------------------------------
#[derive(Debug, Clone)]
pub struct EnvConfig {
    pub listen_addr: &'static str,
    pub listen_port: &'static str,
    pub upstream_host: &'static str,
    pub upstream_port: &'static str,
}

/// ---------------------------------------------------------------------------
/// Stateful pattern replacer – used by JSON and SSE rewrites.
/// ---------------------------------------------------------------------------
#[derive(Debug, Clone)]
struct PatternReplacer {
    cfg: ReplacementConfig,
    opened: bool,
}

impl PatternReplacer {
    fn new(cfg: ReplacementConfig) -> Self {
        Self { cfg, opened: false }
    }

    /// Rewrite `input` according to the configured patterns.
    ///
    /// * The first occurrence of `open_pattern` (while not already inside a block)
    ///   is replaced by `open_tag`.
    /// * While inside a block, any occurrence of `close_pattern` is replaced
    ///   by `close_tag`.  The block then closes.
    fn rewrite(&mut self, input: &str) -> String {
        let mut out = input.to_owned();

        if !self.opened && out.contains(self.cfg.open_pattern) {
            out = out.replacen(self.cfg.open_pattern, self.cfg.open_tag, 1);
            self.opened = true;
        }

        if self.opened && out.contains(self.cfg.close_pattern) {
            out = out.replace(self.cfg.close_pattern, self.cfg.close_tag);
            self.opened = false;
        }

        out
    }
}

/// ---------------------------------------------------------------------------
/// Helper utilities
/// ---------------------------------------------------------------------------
fn error_response(status: StatusCode, msg: impl Into<String>) -> Response<Body> {
    let mut resp = Response::new(Body::from(msg.into()));
    *resp.status_mut() = status;
    resp
}

/* ------------------------------------------------------------------------
   Main request forwarding logic
   ------------------------------------------------------------------------ */
async fn forward_request(
    req: Request<Body>,
    cfg: Arc<ProxyConfig>,
    client: Client<HttpConnector>,
) -> Result<Response<Body>, Infallible> {
    // Remember whether this request needs the special chat‑completion handling.
    let is_chat = req.uri().path() == "/v1/chat/completions";

    // Build the upstream URI (preserve path & query).
    let upstream_uri = cfg.build_upstream_uri(req.uri());

    // Assemble a request that points at the upstream server.
    let mut builder = Request::builder()
        .method(req.method())
        .uri(upstream_uri)
        .version(req.version());

    // Copy all incoming headers except `Host`.
    for (name, value) in req.headers().iter() {
        if name != HOST {
            builder = builder.header(name.clone(), value.clone());
        }
    }

    // Set the correct Host header for the upstream.
    builder = builder.header(
        HOST,
        HeaderValue::from_str(&cfg.upstream_authority())
            .expect("upstream authority is a valid header value"),
    );

    // Move the original body into the new request.
    let upstream_req = builder
        .body(req.into_body())
        .expect("failed to build upstream request");

    // Dispatch.
    match client.request(upstream_req).await {
        Ok(res) => {
            if is_chat {
                handle_chat_completions(res, cfg.replace_cfg.clone()).await
            } else {
                Ok(res)
            }
        }
        Err(e) => {
            error!("upstream request failed: {}", e);
            Ok(error_response(
                StatusCode::BAD_GATEWAY,
                format!("upstream error: {e}"),
            ))
        }
    }
}

/* ------------------------------------------------------------------------
   `/v1/chat/completions` response handling
   ------------------------------------------------------------------------ */
async fn handle_chat_completions(
    resp: Response<Body>,
    replace_cfg: ReplacementConfig,
) -> Result<Response<Body>, Infallible> {
    let is_sse = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.starts_with("text/event-stream"))
        .unwrap_or(false);

    if is_sse {
        rewrite_streaming(resp, replace_cfg).await
    } else {
        rewrite_full_json(resp, replace_cfg).await
    }
}

/* ------------------------------------------------------------------------
   Non‑streaming JSON handling
   ------------------------------------------------------------------------ */
async fn rewrite_full_json(
    resp: Response<Body>,
    replace_cfg: ReplacementConfig,
) -> Result<Response<Body>, Infallible> {
    let (parts, body) = resp.into_parts();

    // Read the whole payload.
    let raw_bytes = match to_bytes(body).await {
        Ok(b) => b,
        Err(e) => {
            return Ok(error_response(
                StatusCode::BAD_GATEWAY,
                format!("failed to read upstream body: {e}"),
            ));
        }
    };

    // Try to parse as JSON; if parsing fails we forward the body unchanged.
    let mut json: Value = match serde_json::from_slice(&raw_bytes) {
        Ok(j) => j,
        Err(_) => {
            // Not JSON – return untouched (but drop Content‑Length because size changed).
            let mut resp = Response::from_parts(parts, Body::from(raw_bytes));
            resp.headers_mut().remove(CONTENT_LENGTH);
            return Ok(resp);
        }
    };

    // Rewrite each `choices[*].message.content` string.
    let mut replacer = PatternReplacer::new(replace_cfg);
    if let Some(choices) = json.get_mut("choices").and_then(|c| c.as_array_mut()) {
        for choice in choices.iter_mut() {
            if let Some(content_val) = choice.pointer_mut("/message/content") {
                if let Some(orig) = content_val.as_str() {
                    let rewritten = replacer.rewrite(orig);
                    *content_val = Value::String(rewritten);
                }
            }
        }
    }

    // Serialize the (possibly) modified JSON back to bytes.
    let new_body = match serde_json::to_vec(&json) {
        Ok(v) => v,
        Err(e) => {
            error!("failed to serialize rewritten JSON: {}", e);
            return Ok(error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal rewrite error",
            ));
        }
    };

    // Build the final response – let hyper recompute Content‑Length.
    let mut out_resp = Response::from_parts(parts, Body::from(new_body));
    out_resp.headers_mut().remove(CONTENT_LENGTH);
    Ok(out_resp)
}

/* ------------------------------------------------------------------------
   Streaming SSE handling
   ------------------------------------------------------------------------ */
async fn rewrite_streaming(
    resp: Response<Body>,
    replace_cfg: ReplacementConfig,
) -> Result<Response<Body>, Infallible> {
    // Preserve all upstream headers except Content‑Length (size will change).
    let mut builder = Response::builder().status(resp.status());
    for (k, v) in resp.headers().iter() {
        if k != CONTENT_LENGTH {
            builder = builder.header(k.clone(), v.clone());
        }
    }

    // The body is a stream of `Bytes`.
    let src_body = resp.into_body();

    // Transform the stream while keeping a mutable `PatternReplacer`.
    let stream = SseTransformer::new(src_body, replace_cfg);

    Ok(builder
        .body(Body::wrap_stream(stream))
        .expect("building streaming response"))
}

/* ------------------------------------------------------------------------
   SSE transformer – buffers until a double newline (`\n\n`) is seen,
   then rewrites the event using `PatternReplacer`.
   ------------------------------------------------------------------------ */
struct SseTransformer<B> {
    inner: B,
    buf: BytesMut,
    replacer: PatternReplacer,
}

impl<B> SseTransformer<B>
where
    B: Stream<Item = Result<Bytes, hyper::Error>> + Unpin,
{
    fn new(inner: B, replace_cfg: ReplacementConfig) -> Self {
        Self {
            inner,
            buf: BytesMut::new(),
            replacer: PatternReplacer::new(replace_cfg),
        }
    }

    /// Locate the first `\n\n` delimiter. Returns the index of the *first* newline.
    fn find_event_boundary(buf: &BytesMut) -> Option<usize> {
        buf.as_ref().windows(2).position(|w| w == b"\n\n")
    }

    /// Rewrite a single SSE event (including its trailing newlines).
    fn rewrite_event(&mut self, raw_event: &[u8]) -> Vec<u8> {
        // The upstream always sends UTF‑8; if it doesn't we forward unchanged.
        let s = match std::str::from_utf8(raw_event) {
            Ok(v) => v,
            Err(_) => return raw_event.to_vec(),
        };

        // Preserve the special `[DONE]` sentinel – it must not be altered.
        if s.trim_start().starts_with("data: [DONE]") {
            return raw_event.to_vec();
        }

        let rewritten = self.replacer.rewrite(s);
        rewritten.into_bytes()
    }
}

impl<B> Stream for SseTransformer<B>
where
    B: Stream<Item = Result<Bytes, hyper::Error>> + Unpin,
{
    type Item = Result<Bytes, hyper::Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        loop {
            // If we have a complete event buffered, emit it.
            if let Some(boundary) = Self::find_event_boundary(&self.buf) {
                let raw_event = self.buf.split_to(boundary + 2);
                let out = self.rewrite_event(&raw_event);
                return std::task::Poll::Ready(Some(Ok(Bytes::from(out))));
            }

            // Pull the next chunk from upstream.
            match ready!(self.inner.poll_next_unpin(cx)) {
                Some(Ok(chunk)) => {
                    self.buf.extend_from_slice(&chunk);
                }
                Some(Err(e)) => return std::task::Poll::Ready(Some(Err(e))),
                None => {
                    // Upstream closed – flush any remaining partial event.
                    if !self.buf.is_empty() {
                        let len = self.buf.len();
                        let raw_event = self.buf.split_to(len);
                        let out = self.rewrite_event(&raw_event);
                        return std::task::Poll::Ready(Some(Ok(Bytes::from(out))));
                    }
                    // No more data.
                    return std::task::Poll::Ready(None);
                }
            };
        }
    }
}

/* ------------------------------------------------------------------------
   Application entry point
   ------------------------------------------------------------------------ */
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Initialise a simple stdout logger.
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    // Load config from environment.
    let cfg = Arc::new(ProxyConfig::from_env());

    // The address is needed *before* we move `cfg` into the service factory.
    let listen_addr_str = format!("{}:{}",
        cfg.connect_cfg.listen_addr,
        cfg.connect_cfg.listen_port);
    let listen_addr: SocketAddr = listen_addr_str.parse().expect("invalid LISTEN_ADDR");

    info!("proxy listening on http://{}", listen_addr);
    info!(
        "forwarding requests to http://{}:{}",
        cfg.connect_cfg.upstream_host,
        cfg.connect_cfg.upstream_port
    );

    // Shared hyper client – cheap to clone for each request.
    let client: Client<HttpConnector> = Client::new();

    // Build the service factory. `cfg` is moved into the closure, but the
    // listening address has already been extracted.
    let make_svc = make_service_fn(move |_conn| {
        let cfg = cfg.clone();
        let client = client.clone();
        async move {
            Ok::<_, Infallible>(service_fn(move |req| {
                forward_request(req, cfg.clone(), client.clone())
            }))
        }
    });

    // Start listening.
    Server::bind(&listen_addr).serve(make_svc).await?;
    Ok(())
}
