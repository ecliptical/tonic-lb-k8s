//! gRPC client example demonstrating tonic-lb-k8s load balancing.
//!
//! This client connects to multiple Greeter server pods via Kubernetes
//! EndpointSlice discovery and demonstrates that requests are load balanced
//! across all available backends.
//!
//! # Running in Kubernetes
//!
//! The client expects to run inside a Kubernetes cluster with access to
//! EndpointSlice resources for the target service.
//!
//! # Environment Variables
//!
//! - `SERVICE_NAME`: Kubernetes service name (default: greeter-server)
//! - `SERVICE_NAMESPACE`: Kubernetes namespace (default: uses pod's namespace)
//! - `GRPC_PORT`: gRPC port (default: 50051)
//! - `REQUEST_COUNT`: Number of requests to make (default: 10)
//! - `REQUEST_INTERVAL_MS`: Milliseconds between requests (default: 1000)

use std::collections::HashMap;
use std::env;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::time::sleep;
use tonic::transport::{Channel, Endpoint};
use tonic_lb_k8s::{DiscoveryConfig, discover};
use tracing::{Level, error, info};

pub mod greeter {
    tonic::include_proto!("greeter");
}

use greeter::HelloRequest;
use greeter::greeter_client::GreeterClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env().add_directive(Level::INFO.into()),
        )
        .init();

    // Read configuration from environment
    let service_name = env::var("SERVICE_NAME").unwrap_or_else(|_| "greeter-server".to_string());
    let service_namespace = env::var("SERVICE_NAMESPACE").ok();
    let port: u16 = env::var("GRPC_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(50051);
    let request_count: u32 = env::var("REQUEST_COUNT")
        .ok()
        .and_then(|c| c.parse().ok())
        .unwrap_or(10);
    let request_interval_ms: u64 = env::var("REQUEST_INTERVAL_MS")
        .ok()
        .and_then(|i| i.parse().ok())
        .unwrap_or(1000);

    info!("Starting greeter client");
    info!("Service: {service_name}");
    if let Some(ref ns) = service_namespace {
        info!("Namespace: {ns}");
    }

    info!("Port: {port}");
    info!("Request count: {request_count}");
    info!("Request interval: {request_interval_ms}ms");

    // Create a balance channel for load balancing
    let (channel, tx) = Channel::balance_channel::<SocketAddr>(1024);

    // Configure Kubernetes endpoint discovery
    let mut config = DiscoveryConfig::new(&service_name, port);
    if let Some(ns) = service_namespace {
        config = config.namespace(ns);
    }

    // Start endpoint discovery
    // The build function creates an Endpoint for each discovered pod address
    discover(config, tx, |addr| {
        Endpoint::from_shared(format!("http://{addr}"))
            .expect("valid endpoint URI")
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
    });

    // Wait a bit for initial endpoint discovery
    info!("Waiting for endpoint discovery...");
    sleep(Duration::from_secs(3)).await;

    // Create the gRPC client
    let mut client = GreeterClient::new(channel);

    // Track which pods serve our requests
    let mut pod_counts: HashMap<String, u32> = HashMap::new();
    let my_name = env::var("HOSTNAME").unwrap_or_else(|_| "client".to_string());

    info!("Sending {request_count} requests...\n");

    for i in 1..=request_count {
        let request = tonic::Request::new(HelloRequest {
            name: format!("{my_name}-request-{i}"),
        });

        match client.say_hello(request).await {
            Ok(response) => {
                let reply = response.into_inner();
                info!(
                    "Request {i}: {} (served by: {})",
                    reply.message, reply.served_by
                );

                *pod_counts.entry(reply.served_by).or_insert(0) += 1;
            }

            Err(e) => {
                error!("Request {i} failed: {e}");
            }
        }

        if i < request_count {
            sleep(Duration::from_millis(request_interval_ms)).await;
        }
    }

    // Print summary
    info!("\n=== Load Balancing Summary ===");
    for (pod, count) in &pod_counts {
        let percentage = (*count as f64 / request_count as f64) * 100.0;
        info!("{pod}: {count} requests ({percentage:.1}%)");
    }

    info!("Total pods used: {}", pod_counts.len());

    Ok(())
}
