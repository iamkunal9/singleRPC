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
        }
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

        let total_endpoints = chain_state.endpoints.len();
        let mut start_idx =
            chain_state.current_index.fetch_add(1, Ordering::Relaxed) % total_endpoints;

        loop {
            for offset in 0..total_endpoints {
                let idx = (start_idx + offset) % total_endpoints;
                let endpoint = &chain_state.endpoints[idx];

                if !endpoint.is_healthy() {
                    continue;
                }

                if self.verbose >= 1 {
                    println!(
                        "-> Hitting endpoint: {} (timeout {:?})",
                        endpoint.url, self.request_timeout
                    );
                }
                match self
                    .client
                    .post(&endpoint.url)
                    .json(&request_json)
                    .send()
                    .await
                {
                    Ok(response) => {
                        if self.verbose >= 1 {
                            println!(
                                "<- Endpoint: {} Status: {}",
                                endpoint.url,
                                response.status()
                            );
                        }
                        if response.status().is_success() {
                            let body = match response.bytes().await {
                                Ok(b) => b,
                                Err(e) => {
                                    if self.verbose >= 1 {
                                        println!("read body error from {}: {}", endpoint.url, e);
                                    }
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
                            endpoint.reset(&self.health_store);
                            return Ok(Response::new(Full::new(body)));
                        } else if response.status().as_u16() == 429 {
                            if self.verbose >= 1 {
                                println!("rate limited by {}", endpoint.url);
                            }
                            endpoint.mark_rate_limited();
                            time::sleep(Duration::from_millis(150)).await;
                        } else if response.status().is_server_error() {
                            if self.verbose >= 1 {
                                println!("server error at {}", endpoint.url);
                            }
                            endpoint.mark_hard_failure(&self.health_store);
                        } else {
                            if self.verbose >= 1 {
                                println!(
                                    "unexpected status {} from {}",
                                    response.status(),
                                    endpoint.url
                                );
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

            start_idx = (start_idx + 1) % total_endpoints;
            time::sleep(Duration::from_millis(200)).await;
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

        let mut start_idx =
            chain_state.current_index.fetch_add(1, Ordering::Relaxed) % total_endpoints;

        loop {
            for offset in 0..total_endpoints {
                let idx = (start_idx + offset) % total_endpoints;
                let endpoint = &chain_state.endpoints[idx];

                if !endpoint.is_healthy() {
                    continue;
                }

                if self.verbose >= 1 {
                    println!(
                        "-> Hitting endpoint: {} (timeout {:?})",
                        endpoint.url, self.request_timeout
                    );
                }

                match self
                    .client
                    .post(&endpoint.url)
                    .json(request_json)
                    .send()
                    .await
                {
                    Ok(response) => {
                        if self.verbose >= 1 {
                            println!(
                                "<- Endpoint: {} Status: {}",
                                endpoint.url,
                                response.status()
                            );
                        }
                        if response.status().is_success() {
                            let body = match response.bytes().await {
                                Ok(b) => b,
                                Err(e) => {
                                    if self.verbose >= 1 {
                                        println!("read body error from {}: {}", endpoint.url, e);
                                    }
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
                            endpoint.reset(&self.health_store);
                            return Ok(body);
                        }

                        if response.status().as_u16() == 429 {
                            if self.verbose >= 1 {
                                println!("rate limited by {}", endpoint.url);
                            }
                            endpoint.mark_rate_limited();
                            time::sleep(Duration::from_millis(150)).await;
                        } else if response.status().is_server_error() {
                            if self.verbose >= 1 {
                                println!("server error at {}", endpoint.url);
                            }
                            endpoint.mark_hard_failure(&self.health_store);
                        } else {
                            if self.verbose >= 1 {
                                println!(
                                    "unexpected status {} from {}",
                                    response.status(),
                                    endpoint.url
                                );
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

            start_idx = (start_idx + 1) % total_endpoints;
            time::sleep(Duration::from_millis(200)).await;
        }
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
