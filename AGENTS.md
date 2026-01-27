# Agent Guidelines for tonic-lb-k8s

This document captures the design decisions, patterns, and lessons learned during the creation of this crate. It serves as context for AI agents working on this project.

## Project Overview

**Purpose**: Tonic client load balancing for Kubernetes.

**Problem Solved**: When using gRPC (HTTP/2) with Kubernetes, standard `ClusterIP` services don't load balance effectively because HTTP/2 multiplexes all requests over a single long-lived TCP connection. This crate watches Kubernetes `EndpointSlice` resources and feeds endpoint changes to a user-provided Tonic balance channel.

## Design Decisions

### 1. User-Controlled Channels

**Decision**: Users create their own `Channel::balance_channel()` and pass the sender to `discover()`.

**Rationale**: This gives users full control over:
- Channel buffer size
- Endpoint configuration (timeouts, TLS, etc.)
- How endpoints are built from socket addresses

**API**:
```rust
pub fn discover<F>(config: DiscoveryConfig, tx: Sender<Change<SocketAddr, Endpoint>>, build: F)
where
    F: Fn(SocketAddr) -> Endpoint + Send + 'static,
```

### 2. SocketAddr as Key Type

**Decision**: Use `SocketAddr` directly as the key type instead of a generic `K`.

**Rationale**: 
- Simpler API - no need for users to specify key types
- `SocketAddr` is the natural identifier for network endpoints
- Generic key types added complexity without clear benefit

### 3. Custom Port Enum

**Decision**: Define our own `Port` enum instead of using `k8s-openapi`'s `IntOrString`.

**Rationale**:
- Better ergonomics with `From` implementations for `u16`, `&str`, and `String`
- Clearer semantics: `Port::Number(50051)` vs `Port::Name("grpc")`
- Users don't need to understand Kubernetes API types

```rust
pub enum Port {
    Number(u16),
    Name(String),
}
```

### 4. Standard Label Selector

**Decision**: Always use `kubernetes.io/service-name={service_name}` label selector.

**Rationale**:
- This is the standard Kubernetes label for EndpointSlice-to-Service association
- No realistic scenario where a user would need a different selector
- Simplifies the API by removing unnecessary configuration

### 5. Optional Namespace with Runtime Resolution

**Decision**: Namespace is optional in `DiscoveryConfig`; defaults to client's namespace at runtime.

**Rationale**:
- In-cluster, the default namespace is read from the service account
- Out-of-cluster, it comes from kubeconfig
- Explicit namespace can be set when needed

### 6. Testable Event Processing

**Decision**: Extract `process_event()` as a separate sync function returning `Vec<EndpointAction>`.

**Rationale**:
- The async `discovery_loop()` requires a real Kubernetes cluster
- By extracting the event processing logic, we can unit test it
- Achieved 87%+ code coverage without integration tests

```rust
enum EndpointAction {
    Insert(SocketAddr),
    Remove(SocketAddr),
}

fn process_event(
    event: &Event<EndpointSlice>,
    known: &mut HashSet<SocketAddr>,
    port: &Port,
) -> Vec<EndpointAction>
```

## Code Patterns

### Tracing

Use inline format arguments for cleaner code:
```rust
// Good
tracing::debug!("adding endpoint: {addr}");

// Avoid
tracing::debug!("adding endpoint: {}", addr);
```

### Kubernetes Watcher Setup

```rust
let label_selector = format!("kubernetes.io/service-name={}", config.service_name);
let watcher_config = WatcherConfig::default().labels(&label_selector);
let stream = watcher::watcher(slices, watcher_config).default_backoff();
```

### EndpointSlice Parsing

- Check `conditions.ready` (defaults to `true` if unset)
- Resolve named ports from the slice's port list
- Parse addresses as `IpAddr`, skip invalid ones
- Support both IPv4 and IPv6

## Project Structure

```
tonic-lb-k8s/
├── Cargo.toml
├── LICENSE              # MIT
├── README.md            # With badges: crates.io, docs.rs, CI, license
├── AGENTS.md            # This file
├── .rustfmt.toml        # edition = "2024"
├── .github/
│   ├── dependabot.yml   # Weekly cargo + actions updates
│   └── workflows/
│       ├── rust-ci.yaml              # Lint, test, coverage
│       └── dependabot-automerge.yaml # Auto-merge patch/minor
├── src/
│   ├── lib.rs           # Public exports only
│   └── k8s.rs           # All implementation
└── examples/
    ├── deploy.sh        # Build and deploy examples
    ├── kind-cluster.sh  # Create/delete local kind cluster
    ├── README.md        # Example documentation
    ├── greeter-server.rs
    ├── greeter-client.rs
    ├── docker/
    │   ├── Dockerfile.server
    │   └── Dockerfile.client
    ├── k8s/
    │   ├── namespace.yaml
    │   ├── server-service.yaml
    │   ├── server-deployment.yaml
    │   ├── client-rbac.yaml
    │   └── client-job.yaml
    └── proto/
        └── greeter.proto
```

## Dependencies

- **tonic 0.14**: gRPC framework (channel feature only)
- **kube 3**: Kubernetes client (rustls-tls, aws-lc-rs, client, runtime)
- **k8s-openapi 0.27**: Kubernetes API types (v1_31)
- **tokio 1**: Async runtime (sync feature only)
- **futures 0.3**: Stream utilities
- **tracing 0.1**: Structured logging

## TLS and Root Certificates

### Design Decision

The crate uses `rustls-tls` with `aws-lc-rs` as the crypto backend for connecting to the Kubernetes API server. Two optional features coordinate TLS root certificate configuration for both kube and tonic dependencies.

**Root Certificate Options**:
- **Default (no feature)**: kube uses system CA certificates; tonic has no root cert configuration from this crate
- **`tls-native-roots` feature**: Explicitly enables system/native root certificates for tonic (kube uses them by default)
- **`tls-webpki-roots` feature**: Embeds Mozilla's root certificates for both kube and tonic - ideal for `scratch` or `distroless` images

### Why This Matters

- `aws-lc-rs` is a **crypto backend** for rustls, not a TLS stack itself
- You need `rustls-tls` to enable the actual TLS layer
- Root certificate features coordinate both dependencies to use the same certificate source
- For kube: `rustls-tls` uses native certs by default; `webpki-roots` feature switches to embedded certs
- For tonic: requires explicit feature flags (`tls-native-roots` or `tls-webpki-roots`)

### Feature Configuration

```toml
# Default: kube uses system CA certs, tonic has no root certs configured by this crate
tonic-lb-k8s = "0.1"

# For containers with system CA certificates (Alpine, Debian, etc.)
# Explicitly enables native roots for tonic
tonic-lb-k8s = { version = "0.1", features = ["tls-native-roots"] }

# For scratch/distroless images (no system CA certs)
# Enables webpki-roots for both kube and tonic
tonic-lb-k8s = { version = "0.1", features = ["tls-webpki-roots"] }
```

**Note**: The features are not mutually exclusive, but you should choose one based on your deployment environment.

## Testing Strategy

1. **Unit tests** for:
   - `Port` conversions
   - `DiscoveryConfig` builder
   - `extract_ready_endpoints()` - various slice configurations
   - `process_event()` - all event types and state transitions

2. **Coverage target**: 80%+

3. **Untestable without cluster**:
   - `discover()` - spawns async task
   - `discovery_loop()` - requires Kubernetes API

## CI/CD

- **rust-ci.yaml**: Runs on PR and push to main
  - `cargo fmt --check`
  - `cargo clippy -- -D warnings`
  - `cargo test` with coverage instrumentation
  - Coverage report posted as PR comment

- **dependabot-automerge.yaml**: Auto-approves and merges patch/minor Cargo updates

## Evolution Notes

The project went through several refinements:

1. Generic key type `K` → simplified to `SocketAddr`
2. Increased test coverage by extracting testable `process_event()`
3. TLS configuration: `aws-lc-rs` alone is insufficient; need `rustls-tls` + `aws-lc-rs`
4. Root certificates: Added `tls-native-roots` and `tls-webpki-roots` features to coordinate root certificate configuration for both kube and tonic dependencies

The guiding principle was **simplicity over flexibility** when the flexibility wasn't clearly needed.
