use std::net::SocketAddr;
use std::sync::Arc;
use hyper::service::service_fn;
use tokio::net::TcpListener;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as HyperServerBuilder;

use crate::proxy::RpcProxy;

pub async fn run_server(proxy: Arc<RpcProxy>, addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(addr).await?;
    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let svc = {
            let proxy = proxy.clone();
            service_fn(move |req| {
                let proxy = proxy.clone();
                async move { proxy.handle_request(req).await }
            })
        };
        tokio::spawn(async move {
            if let Err(e) = HyperServerBuilder::new(TokioExecutor::new())
                .serve_connection(io, svc)
                .await
            {
                eprintln!("server connection error: {}", e);
            }
        });
    }
}


