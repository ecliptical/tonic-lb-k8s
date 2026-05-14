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

## Production Hardening

The discovery side of this crate gives the tonic balance channel an
accurate, up-to-date view of healthy pod IPs. To get the full benefit
in production, a few pieces have to line up on both the **client** and
the **server** side. None of these belong inside this crate, but
omitting them tends to surface as intermittent gRPC 5xx during pod
rollouts, scale-downs, and crashes.

### Client-side: TLS SNI when targeting pod IPs

When TLS is enabled and the `Endpoint` is built from a `SocketAddr`,
tonic derives SNI from the URI authority — i.e. the **pod IP**. Server
certificates almost never list pod IPs in their SAN; they list the
Service DNS name. Without an explicit override, every TLS handshake
will fail.

Always pin SNI to the Service DNS name in your `build` closure:

```rust
let service_name = "my-grpc-service".to_string();
let namespace = "my-namespace".to_string();
let sni = format!("{service_name}.{namespace}.svc.cluster.local");
let tls = ClientTlsConfig::new().domain_name(sni);

discover(config, tx, move |addr| {
    Endpoint::from_shared(format!("https://{addr}"))
        .unwrap()
        .tls_config(tls.clone())
        .unwrap()
        .connect_timeout(Duration::from_secs(5))
});
```

### Client-side: HTTP/2 keep-alive

Long-lived idle HTTP/2 connections to a removed pod can stay open until
the next request, at which point the client gets a connection error
that surfaces as `Unavailable`. Configure keep-alive on every
`Endpoint`:

```rust
endpoint
    .http2_keep_alive_interval(Duration::from_secs(10))
    .keep_alive_timeout(Duration::from_secs(20))
    .keep_alive_while_idle(true)
```

### Client-side: idempotent retry

Even with correct discovery and graceful shutdown, there is always a
small window where a request can race a pod that just stopped accepting
new streams (`GOAWAY`). For idempotent RPCs, wrap the channel in a
retry layer (e.g. `tower::retry`) with a tight retry budget so a single
stale-endpoint pick is invisible to callers. Do **not** retry
non-idempotent RPCs blindly.

### Server-side: graceful shutdown

The discovery side excludes endpoints with
`EndpointConditions.terminating == true`, but the pod still has to give
in-flight RPCs time to complete before the container exits. On the
server `Deployment`:

```yaml
spec:
  template:
    spec:
      terminationGracePeriodSeconds: 30
      containers:
        - name: my-grpc-server
          lifecycle:
            preStop:
              # Give the EndpointSlice controller and watchers a few
              # seconds to observe terminating=true before the gRPC
              # server starts refusing new streams.
              exec:
                command: ["sh", "-c", "sleep 5"]
```

The server's gRPC implementation should also handle SIGTERM by stopping
acceptance of new streams while letting in-flight ones drain (tonic's
`Server::serve_with_shutdown` does this).

## Examples

See the [examples](examples/) directory for a complete demonstration including:
- A sample gRPC server and client
- Dockerfiles for Alpine/musl builds
- Kubernetes manifests
- Deployment script

## License

Licensed under the [MIT license](LICENSE).
