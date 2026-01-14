//! Simple gRPC server example for demonstrating tonic-lb-k8s.
//!
//! This server implements a simple Greeter service and includes its hostname
//! in responses so you can verify that requests are being load balanced
//! across multiple pods.
//!
//! # Running locally
//!
//! ```bash
//! cargo run --example greeter-server
//! ```
//!
//! # Environment Variables
//!
//! - `GRPC_PORT`: Port to listen on (default: 50051)
//! - `HOSTNAME`: Included in responses (set automatically in Kubernetes)

use std::env;
use std::net::SocketAddr;

use tonic::{Request, Response, Status, transport::Server};
use tracing::{Level, info};

pub mod greeter {
    tonic::include_proto!("greeter");
}

use greeter::greeter_server::{Greeter, GreeterServer};
use greeter::{HelloReply, HelloRequest};

#[derive(Debug, Default)]
pub struct MyGreeter {
    hostname: String,
}

impl MyGreeter {
    fn new() -> Self {
        let hostname = env::var("HOSTNAME").unwrap_or_else(|_| "unknown".to_string());
        Self { hostname }
    }
}

#[tonic::async_trait]
impl Greeter for MyGreeter {
    async fn say_hello(
        &self,
        request: Request<HelloRequest>,
    ) -> Result<Response<HelloReply>, Status> {
        let name = &request.into_inner().name;
        info!("Received request from: {name}");

        let reply = HelloReply {
            message: format!("Hello, {name}!"),
            served_by: self.hostname.clone(),
        };

        Ok(Response::new(reply))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env().add_directive(Level::INFO.into()),
        )
        .init();

    let port: u16 = env::var("GRPC_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(50051);

    let addr: SocketAddr = format!("0.0.0.0:{port}").parse()?;
    let greeter = MyGreeter::new();

    info!("Greeter server listening on {addr}");
    info!("Hostname: {}", greeter.hostname);

    Server::builder()
        .add_service(GreeterServer::new(greeter))
        .serve(addr)
        .await?;

    Ok(())
}
