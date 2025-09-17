use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use hyper::{Request, Response, StatusCode};
use hyper::body::Incoming;
use http_body_util::{Full, BodyExt};
use bytes::Bytes;
use std::convert::Infallible;
use serde_json::Value;
use reqwest::Client;
use tokio::time;

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
}

impl RpcProxy {
    pub fn with_timeout(config: HashMap<String, Vec<String>>, verbose: u8, request_timeout: Duration) -> Self {
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
        }
    }

    pub async fn handle_request(&self, req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
        let path = req.uri().path();
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if segments.is_empty() {
            return Ok(
                Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(Full::new(Bytes::from_static(b"Missing chain ID in path. Use /<chain-id>")))
                    .unwrap(),
            );
        }

        let chain_id = segments[0];

        let chain_state = match self.get_chain_state(chain_id) {
            Some(cs) => cs,
            None => {
                return Ok(
                    Response::builder()
                        .status(StatusCode::NOT_FOUND)
                        .body(Full::new(Bytes::from_static(b"Chain not supported")))
                        .unwrap(),
                )
            }
        };

        let body_bytes = match req.into_body().collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(_) => {
                return Ok(
                    Response::builder()
                        .status(StatusCode::BAD_REQUEST)
                        .body(Full::new(Bytes::from_static(b"Invalid request body")))
                        .unwrap(),
                )
            }
        };
        if body_bytes.is_empty() {
            return Ok(
                Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(Full::new(Bytes::from_static(b"Request body cannot be empty")))
                    .unwrap(),
            );
        }
        let request_json: Value = match serde_json::from_slice(&body_bytes) {
            Ok(v) => v,
            Err(_) => {
                return Ok(
                    Response::builder()
                        .status(StatusCode::BAD_REQUEST)
                        .body(Full::new(Bytes::from_static(b"Invalid JSON in request body")))
                        .unwrap(),
                )
            }
        };
        if self.verbose >= 1 {
            println!("Incoming JSON: {}", request_json);
        }

        let total_endpoints = chain_state.endpoints.len();
        let mut start_idx = chain_state
            .current_index
            .fetch_add(1, Ordering::Relaxed)
            % total_endpoints;

        loop {
            for offset in 0..total_endpoints {
                let idx = (start_idx + offset) % total_endpoints;
                let endpoint = &chain_state.endpoints[idx];

                if !endpoint.is_healthy() {
                    continue;
                }

                if self.verbose >= 1 {
                    println!("-> Hitting endpoint: {} (timeout {:?})", endpoint.url, self.request_timeout);
                }
                match self.client.post(&endpoint.url).json(&request_json).send().await {
                    Ok(response) => {
                        if self.verbose >= 1 {
                            println!("<- Endpoint: {} Status: {}", endpoint.url, response.status());
                        }
                        if response.status().is_success() {
                            let body = match response.bytes().await {
                                Ok(b) => b,
                                Err(e) => {
                                    if self.verbose >= 1 { println!("read body error from {}: {}", endpoint.url, e); }
                                    endpoint.mark_failure();
                                    continue;
                                }
                            };
                            if self.verbose >= 2 {
                                println!("<- Body from {}: {}", endpoint.url, String::from_utf8_lossy(&body));
                            }
                            // Try to detect JSON-RPC error object
                            let is_json_error = serde_json::from_slice::<serde_json::Value>(&body)
                                .ok()
                                .and_then(|v| v.get("error").cloned())
                                .is_some();
                            if is_json_error || String::from_utf8_lossy(&body).contains("\"error\"") {
                                if self.verbose >= 1 { println!("json-rpc error from {}", endpoint.url); }
                                endpoint.mark_failure();
                                continue;
                            }
                            endpoint.reset();
                            return Ok(Response::new(Full::new(body)));
                        } else {
                            if response.status().as_u16() == 429 {
                                if self.verbose >= 1 { println!("rate limited by {}", endpoint.url); }
                                endpoint.mark_failure();
                                time::sleep(Duration::from_millis(150)).await;
                            } else if response.status().is_server_error() {
                                if self.verbose >= 1 { println!("server error at {}", endpoint.url); }
                                endpoint.mark_failure();
                            } else {
                                if self.verbose >= 1 { println!("unexpected status {} from {}", response.status(), endpoint.url); }
                                endpoint.mark_failure();
                            }
                        }
                    }
                    Err(e) => {
                        if self.verbose >= 1 {
                            if e.is_timeout() { println!("timeout from {}", endpoint.url); }
                            else if e.is_connect() { println!("connect error at {}", endpoint.url); }
                            else { println!("request error at {}: {}", endpoint.url, e); }
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

    fn get_chain_state(&self, chain_id: &str) -> Option<Arc<ChainState>> {
        self.chains.lock().unwrap().get(chain_id).cloned()
    }
}


