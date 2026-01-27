# Tonic Client Load Balancing for Kubernetes

[![Crates.io](https://img.shields.io/crates/v/tonic-lb-k8s.svg)](https://crates.io/crates/tonic-lb-k8s/)
[![Docs.rs](https://docs.rs/tonic-lb-k8s/badge.svg)](https://docs.rs/tonic-lb-k8s/)
[![CI](https://github.com/ecliptical/tonic-lb-k8s/actions/workflows/rust-ci.yaml/badge.svg)](https://github.com/ecliptical/tonic-lb-k8s/actions/workflows/rust-ci.yaml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

[Tonic](https://crates.io/crates/tonic/) client load balancing for Kubernetes.

This crate provides client-side load balancing for Tonic-based gRPC applications running in Kubernetes. It does that by watching the target service's `EndpointSlice`s and feeding changes to the client channel, thus enabling responsive load balancing across pod replicas.

## Why?

Standard Kubernetes `ClusterIP` services don't load balance gRPC effectively. HTTP/2 multiplexes all requests over a single long-lived TCP connection, so all traffic goes to one pod. Headless services expose individual pod IPs, but the client must:

1. Discover all pod endpoints
2. Maintain connections to each
3. Load balance requests across them
4. React to pods being added/removed

This crate handles all of that automatically.

## Installation

Add `tonic` and `tonic-lb-k8s` to your `Cargo.toml`:

```toml
[dependencies]
tonic = "0.14"
tonic-lb-k8s = "0.1"
```

### TLS Root Certificates

This crate uses `rustls` for TLS. Choose a root certificate feature based on your deployment:

```toml
# For containers with system CA certificates (Alpine, Debian, etc.)
# Enables native/system roots for tonic
tonic-lb-k8s = { version = "0.1", features = ["tls-native-roots"] }

# For scratch/distroless images (no system CA certs)
# Embeds Mozilla's root certs for both kube and tonic
tonic-lb-k8s = { version = "0.1", features = ["tls-webpki-roots"] }
```

| Feature | kube | tonic | Use case |
|---------|------|-------|----------|
| *(none)* | System certs (default) | No roots configured | kube-only TLS |
| `tls-native-roots` | System certs (default) | System certs | Containers with CA certs |
| `tls-webpki-roots` | Embedded Mozilla certs | Embedded Mozilla certs | scratch/distroless |

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
