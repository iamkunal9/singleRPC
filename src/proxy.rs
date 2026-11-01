use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::AUTHORIZATION;
use hyper::http::HeaderMap;
use hyper::{Request, Response, StatusCode};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::convert::Infallible;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::time;
use url::form_urlencoded;

const HARD_FAILURE_THRESHOLD: u32 = 3;
const HARD_FAILURE_COOLDOWN: Duration = Duration::from_secs(24 * 60 * 60);
const RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(60);
/// How long an endpoint that reported pruned/missing historical state is
/// skipped for requests pinned at or below that block.
const MISSING_STATE_TTL: Duration = Duration::from_secs(30 * 60);
/// Retries on other endpoints for generic transient JSON-RPC errors
/// (e.g. -32603 "Internal error") before returning the error to the client.
const MAX_SOFT_RETRIES: u32 = 4;

#[derive(Debug, Clone)]
pub struct EndpointHealthStore {
    path: PathBuf,
    entries: Arc<Mutex<HashMap<String, PersistedEndpointHealth>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedEndpointHealth {
    chain_id: String,
    url: String,
    reason: String,
    unavailable_until: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedHealthFile {
    endpoints: HashMap<String, PersistedEndpointHealth>,
}

impl EndpointHealthStore {
    fn load(path: PathBuf) -> Self {
        let now = unix_now();
        let entries = fs::read_to_string(&path)
            .ok()
            .and_then(|contents| serde_json::from_str::<PersistedHealthFile>(&contents).ok())
            .map(|file| {
                file.endpoints
                    .into_iter()
                    .filter(|(_, entry)| entry.unavailable_until > now)
                    .collect()
            })
            .unwrap_or_default();

        let store = EndpointHealthStore {
            path,
            entries: Arc::new(Mutex::new(entries)),
        };
        store.persist();
        store
    }

    fn remaining_dead_duration(&self, chain_id: &str, url: &str) -> Option<Duration> {
        let key = endpoint_key(chain_id, url);
        let now = unix_now();
        let mut entries = self.entries.lock().unwrap();
        let entry = entries.get(&key)?;
        if entry.unavailable_until <= now {
            entries.remove(&key);
            drop(entries);
            self.persist();
            return None;
        }
        Some(Duration::from_secs(entry.unavailable_until - now))
    }

    fn mark_dead(&self, chain_id: &str, url: &str, cooldown: Duration) {
        let entry = PersistedEndpointHealth {
            chain_id: chain_id.to_string(),
            url: url.to_string(),
            reason: "dead".to_string(),
            unavailable_until: unix_now() + cooldown.as_secs(),
        };
        self.entries
            .lock()
            .unwrap()
            .insert(endpoint_key(chain_id, url), entry);
        self.persist();
    }

    fn clear(&self, chain_id: &str, url: &str) {
        if self
            .entries
            .lock()
            .unwrap()
            .remove(&endpoint_key(chain_id, url))
            .is_some()
        {
            self.persist();
        }
    }

    fn persist(&self) {
        if let Some(parent) = self.path.parent()
            && let Err(err) = fs::create_dir_all(parent)
        {
            eprintln!(
                "failed to create health store directory {}: {}",
                parent.display(),
                err
            );
            return;
        }

        let file = PersistedHealthFile {
            endpoints: self.entries.lock().unwrap().clone(),
        };
        match serde_json::to_string_pretty(&file) {
            Ok(contents) => {
                if let Err(err) = fs::write(&self.path, contents) {
                    eprintln!(
                        "failed to write health store {}: {}",
                        self.path.display(),
                        err
                    );
                }
            }
            Err(err) => eprintln!("failed to serialize health store: {}", err),
        }
    }
}

pub fn default_health_store_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("SINGLERPC_HEALTH_FILE") {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            return None;
        }
        return Some(PathBuf::from(trimmed));
    }

    std::env::var("HOME")
        .ok()
        .filter(|home| !home.trim().is_empty())
        .map(|home| Path::new(&home).join(".singlerpc").join("health.json"))
}

fn endpoint_key(chain_id: &str, url: &str) -> String {
    format!("{chain_id}\n{url}")
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Debug, Clone)]
pub struct RpcEndpoint {
    chain_id: String,
    pub url: String,
    failures: Arc<Mutex<u32>>,
    unavailable_until: Arc<Mutex<Option<Instant>>>,
    /// Highest block for which this endpoint reported missing (pruned) state.
    missing_state_upto: Arc<Mutex<Option<(u64, Instant)>>>,
}

impl RpcEndpoint {
    pub fn new(url: String) -> Self {
        Self::for_chain(String::new(), url)
    }

    fn for_chain(chain_id: String, url: String) -> Self {
        RpcEndpoint {
            chain_id,
            url,
            failures: Arc::new(Mutex::new(0)),
            unavailable_until: Arc::new(Mutex::new(None)),
            missing_state_upto: Arc::new(Mutex::new(None)),
        }
    }

    fn lacks_state_for(&self, block: u64) -> bool {
        let mut guard = self.missing_state_upto.lock().unwrap();
        match *guard {
            Some((upto, expires)) if Instant::now() < expires => block <= upto,
            Some(_) => {
                *guard = None;
                false
            }
            None => false,
        }
    }

    fn mark_missing_state(&self, block: u64) {
        let mut guard = self.missing_state_upto.lock().unwrap();
        let upto = match *guard {
            Some((existing, expires)) if Instant::now() < expires => existing.max(block),
            _ => block,
        };
        *guard = Some((upto, Instant::now() + MISSING_STATE_TTL));
    }

    fn clear_failures(&self) {
        *self.failures.lock().unwrap() = 0;
    }

    fn is_rate_limited_only(&self) -> bool {
        *self.failures.lock().unwrap() < HARD_FAILURE_THRESHOLD
    }

    fn cooldown_remaining(&self) -> Option<Duration> {
        let until = (*self.unavailable_until.lock().unwrap())?;
        Some(until.saturating_duration_since(Instant::now()))
    }

    fn is_healthy(&self) -> bool {
        let mut unavailable_until = self.unavailable_until.lock().unwrap();
        if let Some(until) = *unavailable_until {
            if Instant::now() < until {
                return false;
            }
            *unavailable_until = None;
            *self.failures.lock().unwrap() = 0;
        }
        true
    }

    fn mark_hard_failure(&self, health_store: &Option<EndpointHealthStore>) {
        let mut failures = self.failures.lock().unwrap();
        *failures += 1;
        if *failures >= HARD_FAILURE_THRESHOLD {
            *self.unavailable_until.lock().unwrap() = Some(Instant::now() + HARD_FAILURE_COOLDOWN);
            if let Some(store) = health_store {
                store.mark_dead(&self.chain_id, &self.url, HARD_FAILURE_COOLDOWN);
            }
        }
    }

    fn mark_rate_limited(&self) {
        *self.unavailable_until.lock().unwrap() = Some(Instant::now() + RATE_LIMIT_COOLDOWN);
    }

    fn reset(&self, health_store: &Option<EndpointHealthStore>) {
        *self.failures.lock().unwrap() = 0;
        *self.unavailable_until.lock().unwrap() = None;
        if let Some(store) = health_store {
            store.clear(&self.chain_id, &self.url);
        }
    }

    fn restore_dead_until(&self, unavailable_for: Duration) {
        *self.failures.lock().unwrap() = HARD_FAILURE_THRESHOLD;
        *self.unavailable_until.lock().unwrap() = Some(Instant::now() + unavailable_for);
    }
}

pub struct ChainState {
    pub endpoints: Vec<RpcEndpoint>,
    pub current_index: AtomicUsize,
}

pub struct RpcProxy {
    pub chains: Arc<Mutex<HashMap<String, Arc<ChainState>>>>,
    pub client: Client,
    verbose: u8,
    request_timeout: Duration,
    required_auth_token: Option<String>,
    health_store: Option<EndpointHealthStore>,
}

impl RpcProxy {
    pub fn with_timeout(
        config: HashMap<String, Vec<String>>,
        verbose: u8,
        request_timeout: Duration,
        required_auth_token: Option<String>,
    ) -> Self {
        Self::with_health_store(config, verbose, request_timeout, required_auth_token, None)
    }

    pub fn with_health_store(
        config: HashMap<String, Vec<String>>,
        verbose: u8,
        request_timeout: Duration,
        required_auth_token: Option<String>,
        health_store_path: Option<PathBuf>,
    ) -> Self {
        let health_store = health_store_path.map(EndpointHealthStore::load);
        let mut chains = HashMap::new();
        for (chain_id, urls) in config {
            let endpoints = urls
                .into_iter()
                .map(|url| {
                    let endpoint = RpcEndpoint::for_chain(chain_id.clone(), url);
                    if let Some(store) = &health_store
                        && let Some(unavailable_for) =
                            store.remaining_dead_duration(&chain_id, &endpoint.url)
                    {
                        endpoint.restore_dead_until(unavailable_for);
                    }
                    endpoint
                })
                .collect();
            chains.insert(
                chain_id,
                Arc::new(ChainState {
                    endpoints,
                    current_index: AtomicUsize::new(0),
                }),
            );
        }

        let client = reqwest::Client::builder()
            .timeout(request_timeout)
            .build()
            .expect("failed to build reqwest client");

        RpcProxy {
            chains: Arc::new(Mutex::new(chains)),
            client,
            verbose,
            request_timeout,
            required_auth_token,
            health_store,
        }
    }

    pub async fn handle_request(
        &self,
        req: Request<Incoming>,
    ) -> Result<Response<Full<Bytes>>, Infallible> {
        let (parts, body) = req.into_parts();

        if let Some(expected) = self.required_auth_token.as_deref()
            && !Self::is_authorized(&parts.headers, parts.uri.query(), expected)
        {
            return Ok(Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .body(Full::new(Bytes::from_static(
                    b"Missing or invalid auth token",
                )))
                .unwrap());
        }

        // Own the path string so we can still consume the request body later.
        let path = parts.uri.path().to_string();
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if segments.is_empty() {
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Full::new(Bytes::from_static(
                    b"Missing chain ID in path. Use /<chain-id>",
                )))
                .unwrap());
        }

        let body_bytes = match body.collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(_) => {
                return Ok(Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(Full::new(Bytes::from_static(b"Invalid request body")))
                    .unwrap());
            }
        };

        if segments[0] == "sr_contract_chains" {
            return Ok(self.handle_contract_chains(body_bytes).await);
        }

        let chain_id = segments[0];

        let chain_state = match self.get_chain_state(chain_id) {
            Some(cs) => cs,
            None => {
                return Ok(Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(Full::new(Bytes::from_static(b"Chain not supported")))
                    .unwrap());
            }
        };

        if body_bytes.is_empty() {
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Full::new(Bytes::from_static(
                    b"Request body cannot be empty",
                )))
                .unwrap());
        }
        let request_json: Value = match serde_json::from_slice(&body_bytes) {
            Ok(v) => v,
            Err(_) => {
                return Ok(Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(Full::new(Bytes::from_static(
                        b"Invalid JSON in request body",
                    )))
                    .unwrap());
            }
        };
        if self.verbose >= 1 {
            println!("Incoming JSON: {}", request_json);
        }

        match self
            .proxy_json_to_chain(chain_id, &chain_state, &request_json)
            .await
        {
            Ok(body) => Ok(Response::builder()
                .header("content-type", "application/json")
                .body(Full::new(body))
                .unwrap()),
            Err(err) => {
                let body = upstream_failure_body(&request_json, &err);
                Ok(Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .header("content-type", "application/json")
                    .body(Full::new(Bytes::from(body)))
                    .unwrap())
            }
        }
    }

    pub async fn request_json(&self, chain_id: &str, request_json: Value) -> Result<Value, String> {
        let chain_state = self
            .get_chain_state(chain_id)
            .ok_or_else(|| format!("chain {} not supported", chain_id))?;

        let bytes = self
            .proxy_json_to_chain(chain_id, &chain_state, &request_json)
            .await?;
        serde_json::from_slice(&bytes).map_err(|err| format!("invalid JSON-RPC response: {err}"))
    }

    async fn proxy_json_to_chain(
        &self,
        chain_id: &str,
        chain_state: &ChainState,
        request_json: &Value,
    ) -> Result<Bytes, String> {
        let total_endpoints = chain_state.endpoints.len();
        if total_endpoints == 0 {
            return Err(format!("chain {chain_id} has no endpoints configured"));
        }

        let deadline = Instant::now() + self.overall_deadline();
        let pinned_block = requested_block_number(request_json);
        // A node that answered a broadcast with an error may still have
        // propagated it; never resend on a JSON-RPC error.
        let submits_transaction = submits_transaction(request_json);
        let mut start_idx =
            chain_state.current_index.fetch_add(1, Ordering::Relaxed) % total_endpoints;
        // Endpoints that answered this request with a retryable JSON-RPC error.
        // They are not asked again for the same request.
        let mut answered_with_error = vec![false; total_endpoints];
        let mut soft_retries = 0u32;
        let mut last_error_body: Option<Bytes> = None;
        let mut last_failure = String::from("no healthy endpoints");
        // Once every other option is exhausted, give rate-limited (but not dead)
        // endpoints one more chance before returning an error.
        let mut last_resort = false;

        loop {
            let mut attempted = false;
            for offset in 0..total_endpoints {
                if Instant::now() >= deadline {
                    break;
                }
                let idx = (start_idx + offset) % total_endpoints;
                let endpoint = &chain_state.endpoints[idx];

                if answered_with_error[idx]
                    || !(endpoint.is_healthy() || (last_resort && endpoint.is_rate_limited_only()))
                {
                    continue;
                }
                if let Some(block) = pinned_block
                    && endpoint.lacks_state_for(block)
                {
                    continue;
                }
                attempted = true;
                if last_resort {
                    // One final attempt per endpoint, whatever the outcome.
                    answered_with_error[idx] = true;
                }

                if self.verbose >= 1 {
                    println!(
                        "-> Hitting endpoint: {} (timeout {:?})",
                        endpoint.url, self.request_timeout
                    );
                }

                let response = match self
                    .client
                    .post(&endpoint.url)
                    .json(request_json)
                    .send()
                    .await
                {
                    Ok(response) => response,
                    Err(e) => {
                        if self.verbose >= 1 {
                            if e.is_timeout() {
                                println!("timeout from {}", endpoint.url);
                            } else if e.is_connect() {
                                println!("connect error at {}", endpoint.url);
                            } else {
                                println!("request error at {}: {}", endpoint.url, e);
                            }
                        }
                        last_failure = format!("{}: {}", endpoint.url, e);
                        endpoint.mark_hard_failure(&self.health_store);
                        time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };

                let status = response.status();
                if self.verbose >= 1 {
                    println!("<- Endpoint: {} Status: {}", endpoint.url, status);
                }
                let body = match response.bytes().await {
                    Ok(b) => b,
                    Err(e) => {
                        if self.verbose >= 1 {
                            println!("read body error from {}: {}", endpoint.url, e);
                        }
                        last_failure = format!("{}: {}", endpoint.url, e);
                        endpoint.mark_hard_failure(&self.health_store);
                        continue;
                    }
                };
                if self.verbose >= 2 {
                    println!(
                        "<- Body from {}: {}",
                        endpoint.url,
                        String::from_utf8_lossy(&body)
                    );
                }

                let verdict = classify_response(request_json, &body);

                if status.is_success() {
                    match verdict {
                        ResponseVerdict::Usable => {
                            endpoint.reset(&self.health_store);
                            return Ok(body);
                        }
                        ResponseVerdict::NotJsonRpc => {
                            if self.verbose >= 1 {
                                println!("non JSON-RPC body from {}", endpoint.url);
                            }
                            last_failure = format!("{}: non JSON-RPC response body", endpoint.url);
                            endpoint.mark_hard_failure(&self.health_store);
                            continue;
                        }
                        ResponseVerdict::Retryable(_) if submits_transaction => {
                            endpoint.clear_failures();
                            return Ok(body);
                        }
                        ResponseVerdict::Retryable(kind) => {
                            // The node is reachable; it just cannot serve this request.
                            endpoint.clear_failures();
                            answered_with_error[idx] = true;
                            last_error_body = Some(body);
                            if self.verbose >= 1 {
                                println!(
                                    "retryable {:?} JSON-RPC error from {}",
                                    kind, endpoint.url
                                );
                            }
                            match kind {
                                RetryKind::MissingState => {
                                    if let Some(block) = pinned_block {
                                        endpoint.mark_missing_state(block);
                                    }
                                }
                                RetryKind::RateLimited => endpoint.mark_rate_limited(),
                                RetryKind::Transient => {
                                    soft_retries += 1;
                                    if soft_retries >= MAX_SOFT_RETRIES {
                                        return Ok(last_error_body.unwrap());
                                    }
                                }
                            }
                            continue;
                        }
                    }
                }

                // Non-2xx: a JSON-RPC body may still explain why (e.g. a 403 for
                // archive requests on a node that is fine for recent blocks).
                if let ResponseVerdict::Retryable(kind) = verdict
                    && status.as_u16() != 429
                    && kind != RetryKind::Transient
                {
                    answered_with_error[idx] = true;
                    last_error_body = Some(body);
                    if self.verbose >= 1 {
                        println!(
                            "retryable {:?} error (HTTP {}) from {}",
                            kind, status, endpoint.url
                        );
                    }
                    match kind {
                        RetryKind::MissingState => {
                            if let Some(block) = pinned_block {
                                endpoint.mark_missing_state(block);
                            }
                        }
                        _ => endpoint.mark_rate_limited(),
                    }
                    continue;
                }

                // Some nodes (e.g. drpc) answer request-specific errors such as
                // "method is not available" with HTTP 400 + a JSON-RPC error body.
                // The node is healthy; hand the error back like a 200 reply.
                if status.as_u16() == 400
                    && verdict == ResponseVerdict::Usable
                    && serde_json::from_slice::<Value>(&body)
                        .map(|v| v.get("error").is_some())
                        .unwrap_or(false)
                {
                    endpoint.clear_failures();
                    return Ok(body);
                }

                last_failure = format!("{}: HTTP {}", endpoint.url, status);
                if status.as_u16() == 429 {
                    if self.verbose >= 1 {
                        println!("rate limited by {}", endpoint.url);
                    }
                    endpoint.mark_rate_limited();
                } else {
                    if self.verbose >= 1 {
                        println!("unexpected status {} from {}", status, endpoint.url);
                    }
                    endpoint.mark_hard_failure(&self.health_store);
                }
                time::sleep(Duration::from_millis(50)).await;
            }

            if Instant::now() >= deadline {
                break;
            }
            if !attempted {
                // Every endpoint is cooling down, already answered with an error,
                // or lacks the requested state.
                if !last_resort {
                    last_resort = true;
                    continue;
                }
                if let Some(body) = last_error_body.take() {
                    return Ok(body);
                }
                let soonest = chain_state
                    .endpoints
                    .iter()
                    .filter_map(RpcEndpoint::cooldown_remaining)
                    .min();
                match soonest {
                    Some(wait) if Instant::now() + wait < deadline => {
                        time::sleep(wait.max(Duration::from_millis(200))).await;
                    }
                    _ => {
                        if let Some(block) = pinned_block {
                            last_failure = format!(
                                "{last_failure}; no available endpoint has state for block {block}"
                            );
                        }
                        break;
                    }
                }
            }
            start_idx = (start_idx + 1) % total_endpoints;
        }

        match last_error_body {
            Some(body) => Ok(body),
            None => Err(format!(
                "all upstream endpoints for chain {chain_id} failed: {last_failure}"
            )),
        }
    }

    fn overall_deadline(&self) -> Duration {
        std::cmp::max(self.request_timeout * 6, Duration::from_secs(10))
    }

    fn is_authorized(headers: &HeaderMap, query: Option<&str>, expected: &str) -> bool {
        Self::extract_auth_token(headers, query)
            .map(|token| token == expected)
            .unwrap_or(false)
    }

    fn extract_auth_token(headers: &HeaderMap, query: Option<&str>) -> Option<String> {
        if let Some(value) = headers
            .get("x-singlerpc-auth")
            .and_then(|v| v.to_str().ok())
        {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }

        if let Some(value) = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()) {
            let token = value
                .strip_prefix("Bearer ")
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .or_else(|| {
                    let trimmed = value.trim();
                    if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed.to_string())
                    }
                });
            if token.is_some() {
                return token;
            }
        }

        if let Some(query) = query {
            for (key, value) in form_urlencoded::parse(query.as_bytes()) {
                if key == "auth" {
                    let owned = value.into_owned();
                    if !owned.is_empty() {
                        return Some(owned);
                    }
                }
            }
        }

        None
    }

    fn get_chain_state(&self, chain_id: &str) -> Option<Arc<ChainState>> {
        self.chains.lock().unwrap().get(chain_id).cloned()
    }

    async fn handle_contract_chains(&self, body_bytes: Bytes) -> Response<Full<Bytes>> {
        if body_bytes.is_empty() {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Full::new(Bytes::from_static(
                    b"Request body cannot be empty",
                )))
                .unwrap();
        }

        let request_json: Value = match serde_json::from_slice(&body_bytes) {
            Ok(v) => v,
            Err(_) => {
                return Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(Full::new(Bytes::from_static(
                        b"Invalid JSON in request body",
                    )))
                    .unwrap();
            }
        };

        let request_id = request_json.get("id").cloned();
        let (address, requested_chains) = match extract_contract_params(&request_json) {
            Ok(v) => v,
            Err(msg) => {
                let body = json!({
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "error": { "code": -32602, "message": msg }
                })
                .to_string();
                return Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(Full::new(Bytes::from(body)))
                    .unwrap();
            }
        };

        let all_chains: Vec<String> = self.chains.lock().unwrap().keys().cloned().collect();
        let target_chains: Vec<String> = match requested_chains {
            Some(list) if !list.is_empty() => list,
            _ => all_chains,
        };

        let mut result_map = serde_json::Map::new();

        for chain_id in target_chains {
            if let Some(chain_state) = self.get_chain_state(&chain_id) {
                match self
                    .fetch_contract_code(&chain_id, &chain_state, &address)
                    .await
                {
                    Ok(code) => {
                        let exists = !code_is_empty(&code);
                        result_map.insert(chain_id, json!({ "exists": exists, "code": code }));
                    }
                    Err(err) => {
                        result_map.insert(chain_id, json!({ "exists": false, "error": err }));
                    }
                }
            } else {
                result_map.insert(
                    chain_id,
                    json!({ "exists": false, "error": "chain not configured" }),
                );
            }
        }

        let response_body = json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {
                "address": address,
                "chains": result_map
            }
        })
        .to_string();

        Response::builder()
            .status(StatusCode::OK)
            .body(Full::new(Bytes::from(response_body)))
            .unwrap()
    }

    async fn fetch_contract_code(
        &self,
        chain_id: &str,
        chain_state: &ChainState,
        address: &str,
    ) -> Result<String, String> {
        let total_endpoints = chain_state.endpoints.len();
        if total_endpoints == 0 {
            return Err("no endpoints configured".to_string());
        }

        let start_idx = chain_state.current_index.fetch_add(1, Ordering::Relaxed) % total_endpoints;

        let payload = json!({
            "jsonrpc": "2.0",
            "id": "sr_contract_chains",
            "method": "eth_getCode",
            "params": [address, "latest"]
        });

        for offset in 0..total_endpoints {
            let idx = (start_idx + offset) % total_endpoints;
            let endpoint = &chain_state.endpoints[idx];

            if !endpoint.is_healthy() {
                continue;
            }

            if self.verbose >= 1 {
                println!(
                    "sr_contract_chains -> {} via {} (timeout {:?})",
                    chain_id, endpoint.url, self.request_timeout
                );
            }

            match self.client.post(&endpoint.url).json(&payload).send().await {
                Ok(response) => {
                    if self.verbose >= 1 {
                        println!(
                            "<- sr_contract_chains {} status {}",
                            endpoint.url,
                            response.status()
                        );
                    }

                    if response.status().is_success() {
                        match response.bytes().await {
                            Ok(bytes) => {
                                let parsed: Value = match serde_json::from_slice(&bytes) {
                                    Ok(v) => v,
                                    Err(e) => {
                                        if self.verbose >= 1 {
                                            println!("parse error from {}: {}", endpoint.url, e);
                                        }
                                        endpoint.mark_hard_failure(&self.health_store);
                                        continue;
                                    }
                                };

                                if let Some(err) = parsed.get("error") {
                                    if self.verbose >= 1 {
                                        println!("json-rpc error from {}: {}", endpoint.url, err);
                                    }
                                    return Err(format!("json-rpc error: {err}"));
                                }

                                if let Some(code_str) =
                                    parsed.get("result").and_then(|v| v.as_str())
                                {
                                    endpoint.reset(&self.health_store);
                                    return Ok(code_str.to_string());
                                }

                                if self.verbose >= 1 {
                                    println!(
                                        "missing result field from {} response {:?}",
                                        endpoint.url, parsed
                                    );
                                }
                                endpoint.mark_hard_failure(&self.health_store);
                                continue;
                            }
                            Err(e) => {
                                if self.verbose >= 1 {
                                    println!("read body error from {}: {}", endpoint.url, e);
                                }
                                endpoint.mark_hard_failure(&self.health_store);
                                continue;
                            }
                        }
                    } else if response.status().as_u16() == 429 {
                        if self.verbose >= 1 {
                            println!("rate limited by {}", endpoint.url);
                        }
                        endpoint.mark_rate_limited();
                    } else {
                        if response.status().is_server_error() && self.verbose >= 1 {
                            println!("server error at {}", endpoint.url);
                        }
                        endpoint.mark_hard_failure(&self.health_store);
                    }
                }
                Err(e) => {
                    if self.verbose >= 1 {
                        if e.is_timeout() {
                            println!("timeout from {}", endpoint.url);
                        } else if e.is_connect() {
                            println!("connect error at {}", endpoint.url);
                        } else {
                            println!("request error at {}: {}", endpoint.url, e);
                        }
                    }
                    endpoint.mark_hard_failure(&self.health_store);
                }
            }

            time::sleep(Duration::from_millis(50)).await;
        }

        Err("all endpoints failed".to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryKind {
    /// Node is pruned / non-archive for the requested block.
    MissingState,
    /// Node is rate limiting or over quota.
    RateLimited,
    /// Generic node-side failure, lagging head, unknown block, internal error.
    Transient,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponseVerdict {
    Usable,
    NotJsonRpc,
    Retryable(RetryKind),
}

const MISSING_STATE_PATTERNS: &[&str] = &[
    "missing trie node",
    "historical state",
    "pruned",
    "archive",
    "history window",
    "state window",
    "insufficient changesets",
    "does not have the requested",
    "only serves recent blocks",
    "state is not available",
    "state not available",
    "state histories haven't been fully indexed",
];

const LAGGING_PATTERNS: &[&str] = &[
    "header not found",
    "unknown block",
    "block not found",
    "cannot query unfinalized data",
    "requested block is ahead",
    "beyond head block",
    "block is not yet available",
];

const RATE_LIMIT_PATTERNS: &[&str] = &[
    "rate limit",
    "too many requests",
    "limit exceeded",
    "quota",
    "pool is busy",
    "capacity",
];

const TRANSIENT_PATTERNS: &[&str] = &[
    "internal error",
    "cannot fulfill request",
    "timeout",
    "timed out",
    "temporarily unavailable",
    "service unavailable",
    "server unavailable",
    "upstream",
    "try again",
    "bad gateway",
];

fn classify_rpc_error(error: &Value) -> Option<RetryKind> {
    let code = error.get("code").and_then(Value::as_i64);
    // Some providers (e.g. "cu limit exceeded") send the error as a bare string.
    let message = error
        .as_str()
        .or_else(|| error.get("message").and_then(Value::as_str))
        .unwrap_or_default()
        .to_ascii_lowercase();

    // Execution results are deterministic: never fail over on them.
    if code == Some(3) || message.contains("reverted") || message.contains("invalid opcode") {
        return None;
    }
    let matches = |patterns: &[&str]| patterns.iter().any(|p| message.contains(p));
    if matches(MISSING_STATE_PATTERNS) {
        return Some(RetryKind::MissingState);
    }
    if matches(LAGGING_PATTERNS) {
        return Some(RetryKind::Transient);
    }
    if matches(RATE_LIMIT_PATTERNS) || matches!(code, Some(-32005) | Some(-32029) | Some(-32090)) {
        return Some(RetryKind::RateLimited);
    }
    if matches(TRANSIENT_PATTERNS) || matches!(code, Some(-32603) | Some(-32046)) {
        return Some(RetryKind::Transient);
    }
    None
}

fn classify_single(request: Option<&Value>, response: &Value) -> ResponseVerdict {
    let Some(obj) = response.as_object() else {
        return ResponseVerdict::NotJsonRpc;
    };
    if let Some(error) = obj.get("error") {
        return match classify_rpc_error(error) {
            Some(kind) => ResponseVerdict::Retryable(kind),
            None => ResponseVerdict::Usable,
        };
    }
    let Some(result) = obj.get("result") else {
        return ResponseVerdict::NotJsonRpc;
    };
    if let Some(text) = result.as_str()
        && !text.starts_with("0x")
        && let Some(kind) = classify_rpc_error(result)
    {
        return ResponseVerdict::Retryable(kind);
    }
    // A node that has not seen an explicitly requested block yet answers null.
    if result.is_null()
        && let Some(req) = request
        && req.get("method").and_then(Value::as_str) == Some("eth_getBlockByNumber")
        && req
            .get("params")
            .and_then(|p| p.get(0))
            .and_then(Value::as_str)
            .and_then(parse_hex_quantity)
            .is_some()
    {
        return ResponseVerdict::Retryable(RetryKind::Transient);
    }
    ResponseVerdict::Usable
}

/// Decide whether an upstream body can be returned to the client.
fn classify_response(request: &Value, body: &[u8]) -> ResponseVerdict {
    let Ok(response) = serde_json::from_slice::<Value>(body) else {
        return ResponseVerdict::NotJsonRpc;
    };
    match (request, &response) {
        (Value::Array(requests), Value::Array(responses)) => {
            let mut verdict = ResponseVerdict::Usable;
            for item in responses {
                let id = item.get("id");
                let req = requests.iter().find(|r| r.get("id") == id);
                match classify_single(req, item) {
                    ResponseVerdict::Usable => {}
                    ResponseVerdict::NotJsonRpc => return ResponseVerdict::NotJsonRpc,
                    retry @ ResponseVerdict::Retryable(RetryKind::MissingState) => return retry,
                    retry => verdict = retry,
                }
            }
            verdict
        }
        // Batch sent but a single object came back: usually "batch not supported"
        // or a rate-limit error. Try another endpoint without penalising this one.
        (Value::Array(_), Value::Object(_)) => match classify_single(None, &response) {
            ResponseVerdict::NotJsonRpc => ResponseVerdict::NotJsonRpc,
            ResponseVerdict::Retryable(kind) => ResponseVerdict::Retryable(kind),
            ResponseVerdict::Usable => ResponseVerdict::Retryable(RetryKind::Transient),
        },
        (Value::Array(_), _) => ResponseVerdict::NotJsonRpc,
        (_, _) => classify_single(Some(request), &response),
    }
}

fn submits_transaction(request: &Value) -> bool {
    let is_send = |item: &Value| {
        matches!(
            item.get("method").and_then(Value::as_str),
            Some("eth_sendRawTransaction")
                | Some("eth_sendTransaction")
                | Some("eth_sendRawTransactionConditional")
                | Some("eth_sendBundle")
        )
    };
    match request {
        Value::Array(items) => items.iter().any(is_send),
        other => is_send(other),
    }
}

fn parse_hex_quantity(value: &str) -> Option<u64> {
    let hex = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))?;
    if hex.is_empty() || hex.len() > 16 {
        return None;
    }
    u64::from_str_radix(hex, 16).ok()
}

/// Block number a single state-reading request is pinned to, if explicit.
fn requested_block_number(request: &Value) -> Option<u64> {
    let method = request.get("method")?.as_str()?;
    let position = match method {
        "eth_getBlockByNumber" => 0,
        "eth_getBalance"
        | "eth_getCode"
        | "eth_getTransactionCount"
        | "eth_call"
        | "eth_estimateGas"
        | "eth_getAccountInfo"
        | "eth_createAccessList" => 1,
        "eth_getStorageAt" | "eth_getProof" => 2,
        _ => return None,
    };
    let param = request.get("params")?.get(position)?;
    let tag = match param {
        Value::String(s) => s.as_str(),
        Value::Object(map) => map.get("blockNumber")?.as_str()?,
        _ => return None,
    };
    parse_hex_quantity(tag)
}

fn upstream_failure_body(request: &Value, message: &str) -> String {
    let error = |id: Option<&Value>| {
        json!({
            "jsonrpc": "2.0",
            "id": id.cloned().unwrap_or(Value::Null),
            "error": { "code": -32603, "message": format!("singlerpc: {message}") }
        })
    };
    match request {
        Value::Array(items) => {
            Value::Array(items.iter().map(|item| error(item.get("id"))).collect()).to_string()
        }
        _ => error(request.get("id")).to_string(),
    }
}

fn extract_contract_params(v: &Value) -> Result<(String, Option<Vec<String>>), String> {
    // Support JSON-RPC shape or plain object: { "address": "...", "chains": ["..."] }
    let (address_opt, chains_opt) = if let Some(method) = v.get("method").and_then(|m| m.as_str()) {
        if method != "sr_contract_chains" {
            return Err("invalid method; expected sr_contract_chains".to_string());
        }
        // Accept params as object or array
        match v.get("params") {
            Some(Value::Array(arr)) if !arr.is_empty() => {
                let address = arr
                    .first()
                    .and_then(|p| p.as_str())
                    .ok_or_else(|| "params[0] must be the contract address string".to_string())?;
                let chains = arr.get(1).and_then(parse_chain_list);
                (Some(address.to_string()), chains)
            }
            Some(Value::Object(map)) => {
                let address = map
                    .get("address")
                    .and_then(|p| p.as_str())
                    .ok_or_else(|| "params.address is required".to_string())?;
                let chains = map.get("chains").and_then(parse_chain_list);
                (Some(address.to_string()), chains)
            }
            Some(_) | None => (None, None),
        }
    } else {
        let address = v
            .get("address")
            .and_then(|p| p.as_str())
            .ok_or_else(|| "address is required".to_string())?;
        let chains = v.get("chains").and_then(parse_chain_list);
        (Some(address.to_string()), chains)
    };

    let address = address_opt.ok_or_else(|| "address is required".to_string())?;
    Ok((address, chains_opt))
}

fn parse_chain_list(v: &Value) -> Option<Vec<String>> {
    match v {
        Value::Array(arr) => {
            let mut chains = Vec::new();
            for c in arr {
                if let Some(s) = c.as_str() {
                    chains.push(s.to_string());
                }
            }
            if chains.is_empty() {
                None
            } else {
                Some(chains)
            }
        }
        _ => None,
    }
}

fn code_is_empty(code: &str) -> bool {
    let trimmed = code.trim();
    if trimmed.is_empty() {
        return true;
    }
    if let Some(stripped) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        return stripped.chars().all(|c| c == '0');
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::http::header::HeaderValue;
    use hyper::service::service_fn;
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use hyper_util::server::conn::auto::Builder as HyperServerBuilder;
    use std::sync::atomic::Ordering as AtomicOrdering;
    use tokio::net::TcpListener;

    #[test]
    fn extracts_custom_header_token() {
        let mut headers = HeaderMap::new();
        headers.insert("x-singlerpc-auth", HeaderValue::from_static("secret"));
        let token = RpcProxy::extract_auth_token(&headers, None);
        assert_eq!(token.as_deref(), Some("secret"));
    }

    #[test]
    fn extracts_bearer_header_token() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer abc123 "));
        let token = RpcProxy::extract_auth_token(&headers, None);
        assert_eq!(token.as_deref(), Some("abc123"));
    }

    #[test]
    fn extracts_query_token() {
        let headers = HeaderMap::new();
        let token = RpcProxy::extract_auth_token(&headers, Some("foo=bar&auth=qwerty"));
        assert_eq!(token.as_deref(), Some("qwerty"));
    }

    #[tokio::test]
    async fn json_rpc_error_is_returned_without_rotating() {
        let first_hits = Arc::new(AtomicUsize::new(0));
        let second_hits = Arc::new(AtomicUsize::new(0));
        let first = spawn_upstream(
            StatusCode::OK,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"invalid opcode"}}"#,
            first_hits.clone(),
        )
        .await;
        let second = spawn_upstream(
            StatusCode::OK,
            r#"{"jsonrpc":"2.0","id":1,"result":"0x1"}"#,
            second_hits.clone(),
        )
        .await;

        let proxy = RpcProxy::with_timeout(
            HashMap::from([("1".to_string(), vec![first, second])]),
            0,
            Duration::from_secs(2),
            None,
        );

        let response = proxy
            .request_json(
                "1",
                json!({"jsonrpc":"2.0","id":1,"method":"eth_estimateGas","params":[]}),
            )
            .await
            .unwrap();

        assert_eq!(
            response
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str),
            Some("invalid opcode")
        );
        assert_eq!(first_hits.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(second_hits.load(AtomicOrdering::SeqCst), 0);
    }

    #[tokio::test]
    async fn rate_limited_endpoint_rotates_without_persisting_dead_state() {
        let first_hits = Arc::new(AtomicUsize::new(0));
        let second_hits = Arc::new(AtomicUsize::new(0));
        let first = spawn_upstream(
            StatusCode::TOO_MANY_REQUESTS,
            "rate limited",
            first_hits.clone(),
        )
        .await;
        let second = spawn_upstream(
            StatusCode::OK,
            r#"{"jsonrpc":"2.0","id":1,"result":"0x123"}"#,
            second_hits.clone(),
        )
        .await;
        let health_path = unique_temp_path("rate-limit-health.json");

        let proxy = RpcProxy::with_health_store(
            HashMap::from([("1".to_string(), vec![first, second])]),
            0,
            Duration::from_secs(2),
            None,
            Some(health_path.clone()),
        );

        let response = proxy
            .request_json(
                "1",
                json!({"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}),
            )
            .await
            .unwrap();

        assert_eq!(
            response.get("result").and_then(Value::as_str),
            Some("0x123")
        );
        assert_eq!(first_hits.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(second_hits.load(AtomicOrdering::SeqCst), 1);

        let persisted: PersistedHealthFile =
            serde_json::from_str(&fs::read_to_string(&health_path).unwrap()).unwrap();
        assert!(persisted.endpoints.is_empty());
        let _ = fs::remove_file(health_path);
    }

    #[test]
    fn hard_dead_endpoint_is_persisted_and_restored() {
        let health_path = unique_temp_path("dead-health.json");
        let proxy = RpcProxy::with_health_store(
            HashMap::from([("1".to_string(), vec!["http://dead.local".to_string()])]),
            0,
            Duration::from_secs(2),
            None,
            Some(health_path.clone()),
        );
        let chain = proxy.get_chain_state("1").unwrap();
        let endpoint = &chain.endpoints[0];

        endpoint.mark_hard_failure(&proxy.health_store);
        endpoint.mark_hard_failure(&proxy.health_store);
        assert!(endpoint.is_healthy());
        endpoint.mark_hard_failure(&proxy.health_store);
        assert!(!endpoint.is_healthy());

        let persisted: PersistedHealthFile =
            serde_json::from_str(&fs::read_to_string(&health_path).unwrap()).unwrap();
        assert_eq!(persisted.endpoints.len(), 1);

        let restored = RpcProxy::with_health_store(
            HashMap::from([("1".to_string(), vec!["http://dead.local".to_string()])]),
            0,
            Duration::from_secs(2),
            None,
            Some(health_path.clone()),
        );
        let restored_chain = restored.get_chain_state("1").unwrap();
        assert!(!restored_chain.endpoints[0].is_healthy());
        let _ = fs::remove_file(health_path);
    }

    async fn two_upstreams(
        first_status: StatusCode,
        first_body: &'static str,
    ) -> (RpcProxy, Arc<AtomicUsize>, Arc<AtomicUsize>, PathBuf) {
        let first_hits = Arc::new(AtomicUsize::new(0));
        let second_hits = Arc::new(AtomicUsize::new(0));
        let first = spawn_upstream(first_status, first_body, first_hits.clone()).await;
        let second = spawn_upstream(
            StatusCode::OK,
            r#"{"jsonrpc":"2.0","id":1,"result":"0x2a"}"#,
            second_hits.clone(),
        )
        .await;
        let health_path = unique_temp_path("failover-health.json");
        let proxy = RpcProxy::with_health_store(
            HashMap::from([("1".to_string(), vec![first, second])]),
            0,
            Duration::from_secs(2),
            None,
            Some(health_path.clone()),
        );
        (proxy, first_hits, second_hits, health_path)
    }

    fn balance_at(block: &str) -> Value {
        json!({"jsonrpc":"2.0","id":1,"method":"eth_getBalance","params":["0x0000000000000000000000000000000000000001", block]})
    }

    #[tokio::test]
    async fn html_success_body_fails_over() {
        let (proxy, first_hits, second_hits, health) =
            two_upstreams(StatusCode::OK, "<!DOCTYPE html><html>for sale</html>").await;
        let response = proxy.request_json("1", balance_at("latest")).await.unwrap();
        assert_eq!(response.get("result").and_then(Value::as_str), Some("0x2a"));
        assert_eq!(first_hits.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(second_hits.load(AtomicOrdering::SeqCst), 1);
        let _ = fs::remove_file(health);
    }

    #[tokio::test]
    async fn missing_historical_state_fails_over_and_is_remembered() {
        for body in [
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"missing trie node abc (path ) state 0x1 is not available"}}"#,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"historical state 90ae is not available"}}"#,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"public endpoint only serves recent blocks (last 1024)"}}"#,
        ] {
            let (proxy, first_hits, second_hits, health) =
                two_upstreams(StatusCode::OK, body).await;
            for _ in 0..3 {
                let response = proxy.request_json("1", balance_at("0x100")).await.unwrap();
                assert_eq!(response.get("result").and_then(Value::as_str), Some("0x2a"));
            }
            // First endpoint asked once, then skipped for blocks <= 0x100.
            assert_eq!(first_hits.load(AtomicOrdering::SeqCst), 1, "{body}");
            assert_eq!(second_hits.load(AtomicOrdering::SeqCst), 3);
            let persisted: PersistedHealthFile =
                serde_json::from_str(&fs::read_to_string(&health).unwrap()).unwrap();
            assert!(persisted.endpoints.is_empty());
            let _ = fs::remove_file(health);
        }
    }

    #[tokio::test]
    async fn lagging_header_not_found_fails_over() {
        let (proxy, first_hits, second_hits, health) = two_upstreams(
            StatusCode::OK,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"header not found"}}"#,
        )
        .await;
        let response = proxy.request_json("1", balance_at("0x100")).await.unwrap();
        assert_eq!(response.get("result").and_then(Value::as_str), Some("0x2a"));
        assert_eq!(first_hits.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(second_hits.load(AtomicOrdering::SeqCst), 1);
        let _ = fs::remove_file(health);
    }

    #[tokio::test]
    async fn internal_error_fails_over() {
        let (proxy, _first_hits, second_hits, health) = two_upstreams(
            StatusCode::OK,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32603,"message":"Internal error"}}"#,
        )
        .await;
        let response = proxy.request_json("1", balance_at("0x100")).await.unwrap();
        assert_eq!(response.get("result").and_then(Value::as_str), Some("0x2a"));
        assert_eq!(second_hits.load(AtomicOrdering::SeqCst), 1);
        let _ = fs::remove_file(health);
    }

    #[tokio::test]
    async fn revert_and_method_not_found_are_final() {
        for body in [
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":3,"message":"execution reverted","data":"0x08c379a0"}}"#,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"execution reverted"}}"#,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"the method eth_getAccountInfo does not exist/is not available"}}"#,
        ] {
            let (proxy, first_hits, second_hits, health) =
                two_upstreams(StatusCode::OK, body).await;
            let response = proxy.request_json("1", balance_at("0x100")).await.unwrap();
            assert!(response.get("error").is_some(), "{body}");
            assert_eq!(first_hits.load(AtomicOrdering::SeqCst), 1);
            assert_eq!(second_hits.load(AtomicOrdering::SeqCst), 0, "{body}");
            let _ = fs::remove_file(health);
        }
    }

    #[tokio::test]
    async fn archive_403_fails_over_without_persisting_dead() {
        let (proxy, first_hits, second_hits, health) = two_upstreams(
            StatusCode::FORBIDDEN,
            r#"{"jsonrpc":"2.0","error":{"code":-32602,"message":"Archive requests require a personal token"},"id":1}"#,
        )
        .await;
        for block in ["0x100", "0x101", "0x102", "0x103"] {
            let response = proxy.request_json("1", balance_at(block)).await.unwrap();
            assert_eq!(response.get("result").and_then(Value::as_str), Some("0x2a"));
        }
        assert!(first_hits.load(AtomicOrdering::SeqCst) >= 1);
        assert_eq!(second_hits.load(AtomicOrdering::SeqCst), 4);
        let chain = proxy.get_chain_state("1").unwrap();
        assert!(chain.endpoints[0].is_healthy());
        let persisted: PersistedHealthFile =
            serde_json::from_str(&fs::read_to_string(&health).unwrap()).unwrap();
        assert!(persisted.endpoints.is_empty());
        let _ = fs::remove_file(health);
    }

    #[tokio::test]
    async fn all_endpoints_failing_returns_bounded_error() {
        let hits = Arc::new(AtomicUsize::new(0));
        let a = spawn_upstream(StatusCode::OK, "<html></html>", hits.clone()).await;
        let b = spawn_upstream(StatusCode::BAD_GATEWAY, "bad gateway", hits.clone()).await;
        let proxy = RpcProxy::with_timeout(
            HashMap::from([("1".to_string(), vec![a, b])]),
            0,
            Duration::from_secs(1),
            None,
        );
        let started = Instant::now();
        let result = proxy.request_json("1", balance_at("latest")).await;
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(11));
        let body: Value =
            serde_json::from_str(&upstream_failure_body(&balance_at("latest"), "x")).unwrap();
        assert_eq!(body.get("id"), Some(&json!(1)));
        assert!(body.get("error").is_some());
    }

    #[tokio::test]
    async fn batch_passes_through_and_single_object_reply_fails_over() {
        let first_hits = Arc::new(AtomicUsize::new(0));
        let second_hits = Arc::new(AtomicUsize::new(0));
        let first = spawn_upstream(
            StatusCode::OK,
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"batch requests not supported"}}"#,
            first_hits.clone(),
        )
        .await;
        let second = spawn_upstream(
            StatusCode::OK,
            r#"[{"jsonrpc":"2.0","id":1,"result":"0x1"},{"jsonrpc":"2.0","id":2,"result":"0x2"}]"#,
            second_hits.clone(),
        )
        .await;
        let proxy = RpcProxy::with_timeout(
            HashMap::from([("1".to_string(), vec![first, second])]),
            0,
            Duration::from_secs(2),
            None,
        );
        let response = proxy
            .request_json(
                "1",
                json!([
                    {"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]},
                    {"jsonrpc":"2.0","id":2,"method":"eth_blockNumber","params":[]}
                ]),
            )
            .await
            .unwrap();
        assert_eq!(response.as_array().map(Vec::len), Some(2));
        assert_eq!(second_hits.load(AtomicOrdering::SeqCst), 1);
    }

    #[tokio::test]
    async fn http_400_json_rpc_error_is_returned_without_penalty() {
        let (proxy, first_hits, second_hits, health) = two_upstreams(
            StatusCode::BAD_REQUEST,
            r#"{"id":1,"jsonrpc":"2.0","error":{"message":"method is not available","code":-32601}}"#,
        )
        .await;
        for _ in 0..4 {
            let response = proxy.request_json("1", balance_at("latest")).await;
            let response = response.unwrap();
            if response.get("error").is_some() {
                assert_eq!(
                    response.pointer("/error/code").and_then(Value::as_i64),
                    Some(-32601)
                );
            }
        }
        assert!(first_hits.load(AtomicOrdering::SeqCst) >= 1);
        assert!(second_hits.load(AtomicOrdering::SeqCst) >= 1);
        let chain = proxy.get_chain_state("1").unwrap();
        assert!(chain.endpoints[0].is_healthy());
        let persisted: PersistedHealthFile =
            serde_json::from_str(&fs::read_to_string(&health).unwrap()).unwrap();
        assert!(persisted.endpoints.is_empty());
        let _ = fs::remove_file(health);
    }

    #[test]
    fn classifies_string_errors_and_lagging_heads() {
        let req = balance_at("0x10");
        assert_eq!(
            classify_response(&req, br#"{"jsonrpc":"2.0","id":1,"error":"cu limit exceeded; Request too fast per second"}"#),
            ResponseVerdict::Retryable(RetryKind::RateLimited)
        );
        assert_eq!(
            classify_response(&req, br#"{"jsonrpc":"2.0","id":1,"result":"cu limit exceeded; Request too fast per second"}"#),
            ResponseVerdict::Retryable(RetryKind::RateLimited)
        );
        assert_eq!(
            classify_response(&req, br#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"request beyond head block: requested 26320764, head 26319930"}}"#),
            ResponseVerdict::Retryable(RetryKind::Transient)
        );
        assert_eq!(
            classify_response(&req, br#"{"jsonrpc":"2.0","id":1,"error":{"code":-32603,"message":"insufficient changesets to revert to block"}}"#),
            ResponseVerdict::Retryable(RetryKind::MissingState)
        );
        assert_eq!(
            classify_response(&req, br#"{"jsonrpc":"2.0","id":1,"result":"0x0"}"#),
            ResponseVerdict::Usable
        );
    }

    #[tokio::test]
    async fn rate_limited_endpoint_gets_last_resort_attempt() {
        let (proxy, first_hits, second_hits, health) = two_upstreams(
            StatusCode::OK,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"historical state abc is not available"}}"#,
        )
        .await;
        let chain = proxy.get_chain_state("1").unwrap();
        chain.endpoints[1].mark_rate_limited();
        let response = proxy.request_json("1", balance_at("0x100")).await.unwrap();
        assert_eq!(response.get("result").and_then(Value::as_str), Some("0x2a"));
        assert_eq!(first_hits.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(second_hits.load(AtomicOrdering::SeqCst), 1);
        let _ = fs::remove_file(health);
    }

    #[tokio::test]
    async fn transaction_submission_is_not_resent_on_json_rpc_error() {
        let (proxy, first_hits, second_hits, health) = two_upstreams(
            StatusCode::OK,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32603,"message":"Internal error"}}"#,
        )
        .await;
        let response = proxy
            .request_json(
                "1",
                json!({"jsonrpc":"2.0","id":1,"method":"eth_sendRawTransaction","params":["0x02"]}),
            )
            .await
            .unwrap();
        assert!(response.get("error").is_some());
        assert_eq!(first_hits.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(second_hits.load(AtomicOrdering::SeqCst), 0);
        let _ = fs::remove_file(health);
    }

    #[tokio::test]
    async fn last_resort_hits_throttled_endpoint_at_most_once() {
        let first_hits = Arc::new(AtomicUsize::new(0));
        let second_hits = Arc::new(AtomicUsize::new(0));
        let first = spawn_upstream(
            StatusCode::OK,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"historical state abc is not available"}}"#,
            first_hits.clone(),
        )
        .await;
        let second = spawn_upstream(
            StatusCode::TOO_MANY_REQUESTS,
            "slow down",
            second_hits.clone(),
        )
        .await;
        let proxy = RpcProxy::with_timeout(
            HashMap::from([("1".to_string(), vec![first, second])]),
            0,
            Duration::from_secs(2),
            None,
        );
        let chain = proxy.get_chain_state("1").unwrap();
        chain.endpoints[1].mark_rate_limited();
        let started = Instant::now();
        let response = proxy.request_json("1", balance_at("0x100")).await.unwrap();
        assert!(response.get("error").is_some());
        assert_eq!(first_hits.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(second_hits.load(AtomicOrdering::SeqCst), 1);
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn extracts_pinned_block_numbers() {
        assert_eq!(requested_block_number(&balance_at("0x10")), Some(16));
        assert_eq!(requested_block_number(&balance_at("latest")), None);
        let storage = json!({"method":"eth_getStorageAt","params":["0xabc","0x0","0x20"]});
        assert_eq!(requested_block_number(&storage), Some(32));
        let block = json!({"method":"eth_getBlockByNumber","params":["0x30", false]});
        assert_eq!(requested_block_number(&block), Some(48));
    }

    async fn spawn_upstream(
        status: StatusCode,
        body: &'static str,
        hits: Arc<AtomicUsize>,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let hits = hits.clone();
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let svc = service_fn(move |_req| {
                        let hits = hits.clone();
                        async move {
                            hits.fetch_add(1, AtomicOrdering::SeqCst);
                            Ok::<_, Infallible>(
                                Response::builder()
                                    .status(status)
                                    .body(Full::new(Bytes::from_static(body.as_bytes())))
                                    .unwrap(),
                            )
                        }
                    });
                    let _ = HyperServerBuilder::new(TokioExecutor::new())
                        .serve_connection(io, svc)
                        .await;
                });
            }
        });
        format!("http://{}", addr)
    }

    fn unique_temp_path(file_name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("singlerpc-{nanos}-{file_name}"))
    }
}
