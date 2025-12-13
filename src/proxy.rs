use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::AUTHORIZATION;
use hyper::http::HeaderMap;
use hyper::{Request, Response, StatusCode};
use reqwest::Client;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::time;
use url::form_urlencoded;

#[derive(Debug, Clone)]
pub struct RpcEndpoint {
    pub url: String,
    failures: Arc<Mutex<u32>>,
    last_failure: Arc<Mutex<Option<Instant>>>,
}

impl RpcEndpoint {
    pub fn new(url: String) -> Self {
        RpcEndpoint {
            url,
            failures: Arc::new(Mutex::new(0)),
            last_failure: Arc::new(Mutex::new(None)),
        }
    }

    fn is_healthy(&self) -> bool {
        let failures = *self.failures.lock().unwrap();
        let last_failure = *self.last_failure.lock().unwrap();
        if failures >= 3 {
            if let Some(time) = last_failure {
                return time.elapsed() > Duration::from_secs(10800);
            }
        }
        true
    }

    fn mark_failure(&self) {
        let mut failures = self.failures.lock().unwrap();
        *failures += 1;
        *self.last_failure.lock().unwrap() = Some(Instant::now());
    }

    fn reset(&self) {
        *self.failures.lock().unwrap() = 0;
        *self.last_failure.lock().unwrap() = None;
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
}

impl RpcProxy {
    pub fn with_timeout(
        config: HashMap<String, Vec<String>>,
        verbose: u8,
        request_timeout: Duration,
        required_auth_token: Option<String>,
    ) -> Self {
        let mut chains = HashMap::new();
        for (chain_id, urls) in config {
            let endpoints = urls.into_iter().map(RpcEndpoint::new).collect();
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
        }
    }

    pub async fn handle_request(
        &self,
        req: Request<Incoming>,
    ) -> Result<Response<Full<Bytes>>, Infallible> {
        let (parts, body) = req.into_parts();

        if let Some(expected) = self.required_auth_token.as_deref() {
            if !Self::is_authorized(&parts.headers, parts.uri.query(), expected) {
                return Ok(Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .body(Full::new(Bytes::from_static(
                        b"Missing or invalid auth token",
                    )))
                    .unwrap());
            }
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
                                    endpoint.mark_failure();
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
                            // Try to detect JSON-RPC error object
                            let is_json_error = serde_json::from_slice::<serde_json::Value>(&body)
                                .ok()
                                .and_then(|v| v.get("error").cloned())
                                .is_some();
                            if is_json_error || String::from_utf8_lossy(&body).contains("\"error\"")
                            {
                                if self.verbose >= 1 {
                                    println!("json-rpc error from {}", endpoint.url);
                                }
                                endpoint.mark_failure();
                                continue;
                            }
                            endpoint.reset();
                            return Ok(Response::new(Full::new(body)));
                        } else {
                            if response.status().as_u16() == 429 {
                                if self.verbose >= 1 {
                                    println!("rate limited by {}", endpoint.url);
                                }
                                endpoint.mark_failure();
                                time::sleep(Duration::from_millis(150)).await;
                            } else if response.status().is_server_error() {
                                if self.verbose >= 1 {
                                    println!("server error at {}", endpoint.url);
                                }
                                endpoint.mark_failure();
                            } else {
                                if self.verbose >= 1 {
                                    println!(
                                        "unexpected status {} from {}",
                                        response.status(),
                                        endpoint.url
                                    );
                                }
                                endpoint.mark_failure();
                            }
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
                        endpoint.mark_failure();
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
                                        endpoint.mark_failure();
                                        continue;
                                    }
                                };

                                if let Some(err) = parsed.get("error") {
                                    if self.verbose >= 1 {
                                        println!("json-rpc error from {}: {}", endpoint.url, err);
                                    }
                                    endpoint.mark_failure();
                                    continue;
                                }

                                if let Some(code_str) =
                                    parsed.get("result").and_then(|v| v.as_str())
                                {
                                    endpoint.reset();
                                    return Ok(code_str.to_string());
                                }

                                if self.verbose >= 1 {
                                    println!(
                                        "missing result field from {} response {:?}",
                                        endpoint.url, parsed
                                    );
                                }
                                endpoint.mark_failure();
                                continue;
                            }
                            Err(e) => {
                                if self.verbose >= 1 {
                                    println!("read body error from {}: {}", endpoint.url, e);
                                }
                                endpoint.mark_failure();
                                continue;
                            }
                        }
                    } else {
                        if response.status().as_u16() == 429 {
                            if self.verbose >= 1 {
                                println!("rate limited by {}", endpoint.url);
                            }
                        } else if response.status().is_server_error() && self.verbose >= 1 {
                            println!("server error at {}", endpoint.url);
                        }
                        endpoint.mark_failure();
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
                    endpoint.mark_failure();
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
                    .get(0)
                    .and_then(|p| p.as_str())
                    .ok_or_else(|| "params[0] must be the contract address string".to_string())?;
                let chains = arr.get(1).and_then(|c| parse_chain_list(c));
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
}
