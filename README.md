# Kubernetes Endpoint Discovery for Tonic

[![Crates.io](https://img.shields.io/crates/v/tonic-lb-k8s.svg)](https://crates.io/crates/tonic-lb-k8s/)
[![Docs.rs](https://docs.rs/tonic-lb-k8s/badge.svg)](https://docs.rs/tonic-lb-k8s/)
[![CI](https://github.com/ecliptical/tonic-lb-k8s/actions/workflows/rust-ci.yaml/badge.svg)](https://github.com/ecliptical/tonic-lb-k8s/actions/workflows/rust-ci.yaml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

Kubernetes endpoint discovery for [Tonic](https://crates.io/crates/tonic/) gRPC load balancing.

## The Problem

When using gRPC (HTTP/2) with Kubernetes, standard ClusterIP services don't load balance effectively
because HTTP/2 multiplexes all requests over a single long-lived TCP connection. Headless services
expose individual pod IPs, but the client needs to:

1. Discover all pod endpoints
2. Maintain connections to each
3. Load balance requests across them
4. React to pods being added/removed

This crate watches Kubernetes `EndpointSlice` resources and feeds endpoint changes to a
user-provided Tonic balance channel.

## Features

- **Kubernetes API discovery**: Real-time endpoint updates via `EndpointSlice` watch
- **User-controlled channels**: You create the channel and endpoints however you want
- **Dynamic endpoint management**: Automatically adds/removes backends as pods scale

## Installation

Add `tonic` and `tonic-lb-k8s` to your `Cargo.toml`:

```toml
[dependencies]
tonic = "0.14"
tonic-lb-k8s = "0.1"
```

## Usage

```rust
use std::net::SocketAddr;
use std::time::Duration;
use tonic::transport::{Channel, Endpoint};
use tonic_lb_k8s::{discover, DiscoveryConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create your own balance channel
    let (channel, tx) = Channel::balance_channel::<SocketAddr>(1024);

    // Start discovery - build function returns Endpoint for each address
    let config = DiscoveryConfig::new("my-grpc-service", 50051);
    discover(config, tx, |addr| {
        Endpoint::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect_timeout(Duration::from_secs(5))
    });

    // Use with your generated gRPC client
    let client = MyServiceClient::new(channel);
    let response = client.some_method(request).await?;

    Ok(())
}
```

### With TLS

```rust
use std::net::SocketAddr;
use std::time::Duration;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tonic_lb_k8s::{discover, DiscoveryConfig};

let (channel, tx) = Channel::balance_channel::<SocketAddr>(1024);

let config = DiscoveryConfig::new("my-grpc-service", 50051);
let tls = ClientTlsConfig::new();

discover(config, tx, move |addr| {
    Endpoint::from_shared(format!("https://{addr}"))
        .unwrap()
        .tls_config(tls.clone())
        .unwrap()
        .connect_timeout(Duration::from_secs(5))
});
```

## RBAC Requirements

Applications using this crate require Kubernetes RBAC permissions to watch `EndpointSlice` resources.

| API Group | Resource | Verbs |
|-----------|----------|-------|
| `discovery.k8s.io` | `endpointslices` | `list`, `watch` |

### Example Role

```yaml
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: endpointslice-reader
  namespace: <your-namespace>
rules:
  - apiGroups: ["discovery.k8s.io"]
    resources: ["endpointslices"]
    verbs: ["list", "watch"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: <your-app>-endpointslice-reader
  namespace: <your-namespace>
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: Role
  name: endpointslice-reader
subjects:
  - kind: ServiceAccount
    name: <your-service-account>
    namespace: <your-namespace>
```

For cross-namespace discovery, use a `ClusterRole` and `ClusterRoleBinding` instead.

## Examples

See the [examples](examples/) directory for a complete demonstration including:
- A sample gRPC server and client
- Dockerfiles for Alpine/musl builds
- Kubernetes manifests
- Deployment script

## License

Licensed under the [MIT license](LICENSE).
