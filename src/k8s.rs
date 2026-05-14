//! Kubernetes endpoint discovery using `EndpointSlice` watches.
//!
//! This module watches Kubernetes `EndpointSlice` resources and sends endpoint
//! changes to a user-provided channel. Users are responsible for creating
//! their own Tonic channel and endpoints.
//!
//! # How It Works
//!
//! 1. Watches `EndpointSlice` resources for the specified service
//! 2. Extracts ready endpoint addresses from slice events
//! 3. Sends `Change::Insert` or `Change::Remove` events to the provided sender
//! 4. User's balance channel receives updates and manages connections
//!
//! # Example
//!
//! ```ignore
//! use std::{net::SocketAddr, time::Duration};
//! use tonic::transport::{Channel, Endpoint};
//! use tonic_lb_k8s::{discover, DiscoveryConfig};
//!
//! // Create your own balance channel
//! let (channel, tx) = Channel::balance_channel::<SocketAddr>(1024);
//!
//! // Start discovery - build function returns Endpoint for each address
//! let config = DiscoveryConfig::new("my-grpc-service", 50051);
//! discover(config, tx, |addr| {
//!     Endpoint::from_shared(format!("http://{addr}"))
//!         .unwrap()
//!         .connect_timeout(Duration::from_secs(5))
//! });
//!
//! // Use the channel with your gRPC client
//! let client = MyServiceClient::new(channel);
//! ```

use futures::{Stream, TryStreamExt as _};
use k8s_openapi::api::discovery::v1::EndpointSlice;
use kube::runtime::WatchStreamExt as _;
use kube::runtime::watcher::{self, Config as WatcherConfig, Event};
use kube::{Api, Client};
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use tokio::sync::mpsc::Sender;
use tonic::transport::{Endpoint, channel::Change};
use tracing::{debug, error, warn};

/// Error type for discovery failures.
type Error = Box<dyn std::error::Error + Send + Sync>;

/// Result type for discovery operations.
type Result<T> = std::result::Result<T, Error>;

/// Port specification for the gRPC service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Port {
    /// A numeric port number.
    Number(u16),
    /// A named port (resolved from `EndpointSlice`).
    Name(String),
}

impl From<u16> for Port {
    fn from(port: u16) -> Self {
        Self::Number(port)
    }
}

impl From<&str> for Port {
    fn from(name: &str) -> Self {
        Self::Name(name.to_string())
    }
}

impl From<String> for Port {
    fn from(name: String) -> Self {
        Self::Name(name)
    }
}

/// Configuration for Kubernetes endpoint discovery.
#[derive(Clone, Debug)]
pub struct DiscoveryConfig {
    /// The Kubernetes service name to watch.
    pub service_name: String,

    /// The Kubernetes namespace where the service is deployed.
    /// If `None`, uses the current namespace from the kube client.
    pub namespace: Option<String>,

    /// The port for the gRPC service (number or name).
    pub port: Port,
}

impl DiscoveryConfig {
    /// Creates a new discovery configuration.
    ///
    /// The port can be specified as a number (`50051`) or a name (`"grpc"`).
    /// Uses the current namespace from the kube client configuration.
    #[must_use]
    pub fn new(service_name: impl Into<String>, port: impl Into<Port>) -> Self {
        Self {
            service_name: service_name.into(),
            namespace: None,
            port: port.into(),
        }
    }

    /// Sets an explicit namespace for the service.
    #[must_use]
    pub fn namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = Some(namespace.into());
        self
    }
}

/// Starts watching Kubernetes endpoints and sends changes to the provided sender.
///
/// This function spawns a background task that watches `EndpointSlice` resources
/// for the specified service and sends `Change` events to the provided sender.
/// The user is responsible for creating the balance channel and building endpoints.
///
/// # Arguments
///
/// * `config` - Discovery configuration specifying the service to watch
/// * `tx` - Sender for endpoint changes (from `Channel::balance_channel()`)
/// * `build` - Function to build a key and `Endpoint` from a `SocketAddr`
///
/// # Requirements
///
/// - The application must have RBAC permissions to watch `EndpointSlice` resources
/// - Kubernetes client configuration (in-cluster or kubeconfig)
///
/// # Example
///
/// ```ignore
/// use std::net::SocketAddr;
/// use std::time::Duration;
/// use tonic::transport::{Channel, Endpoint};
/// use tonic_lb_k8s::{discover, DiscoveryConfig};
///
/// let (channel, tx) = Channel::balance_channel::<SocketAddr>(1024);
///
/// let config = DiscoveryConfig::new("my-grpc-service", 50051);
/// discover(config, tx, |addr| {
///     Endpoint::from_shared(format!("http://{addr}"))
///         .unwrap()
///         .connect_timeout(Duration::from_secs(5))
/// });
///
/// // Use with your generated gRPC client
/// let client = MyServiceClient::new(channel);
/// ```
pub fn discover<F>(config: DiscoveryConfig, tx: Sender<Change<SocketAddr, Endpoint>>, build: F)
where
    F: Fn(SocketAddr) -> Endpoint + Send + 'static,
{
    tokio::spawn(async move {
        if let Err(e) = discovery_loop(tx, config, build).await {
            error!("Kubernetes endpoint watcher failed: {e}");
        }
    });
}

/// Background task that watches `EndpointSlice` resources and sends endpoint changes.
async fn discovery_loop<F>(
    tx: Sender<Change<SocketAddr, Endpoint>>,
    config: DiscoveryConfig,
    build: F,
) -> Result<()>
where
    F: Fn(SocketAddr) -> Endpoint,
{
    let client = Client::try_default().await?;
    let namespace = config
        .namespace
        .unwrap_or_else(|| client.default_namespace().to_string());
    let slices: Api<EndpointSlice> = Api::namespaced(client, &namespace);

    let label_selector = format!("kubernetes.io/service-name={}", config.service_name);
    let watcher_config = WatcherConfig::default().labels(&label_selector);

    let stream = watcher::watcher(slices, watcher_config).default_backoff();

    debug!(
        "Starting Kubernetes endpoint watch for {namespace}/{} on port {:?}",
        config.service_name, config.port
    );

    run_event_stream(
        stream,
        tx,
        build,
        &config.port,
        &namespace,
        &config.service_name,
    )
    .await
}

/// Drives a stream of `EndpointSlice` watch events, updating the provided
/// `Sender` with `Change::Insert` / `Change::Remove` actions.
///
/// Extracted from [`discovery_loop`] so it can be unit-tested without a
/// real Kubernetes API server: tests pass a synthetic `Stream` of
/// `Event<EndpointSlice>` values via `futures::stream::iter`.
async fn run_event_stream<S, E, F>(
    stream: S,
    tx: Sender<Change<SocketAddr, Endpoint>>,
    build: F,
    port: &Port,
    namespace: &str,
    service_name: &str,
) -> Result<()>
where
    S: Stream<Item = std::result::Result<Event<EndpointSlice>, E>>,
    E: std::error::Error + Send + Sync + 'static,
    F: Fn(SocketAddr) -> Endpoint,
{
    let mut state = DiscoveryState::default();
    tokio::pin!(stream);

    while let Some(event) = stream.try_next().await.map_err(|e| Box::new(e) as Error)? {
        let actions = process_event(&event, &mut state, port);

        for action in actions {
            let change = match action {
                EndpointAction::Insert(addr) => Change::Insert(addr, build(addr)),
                EndpointAction::Remove(addr) => Change::Remove(addr),
            };

            if tx.send(change).await.is_err() {
                warn!("channel closed, stopping Kubernetes watcher");
                return Ok(());
            }
        }

        debug!(
            "Kubernetes discovery: {} endpoints for {namespace}/{service_name}",
            state.refcount.len(),
        );
    }

    Ok(())
}

/// Represents an endpoint change action.
#[derive(Debug, Clone, PartialEq, Eq)]
enum EndpointAction {
    Insert(SocketAddr),
    Remove(SocketAddr),
}

/// Per-slice and aggregate state for the discovery loop.
///
/// A Kubernetes Service may be backed by multiple `EndpointSlice` objects
/// (the controller shards slices at ~100 endpoints, and there is typically
/// one slice per address family). The same address can therefore legally
/// appear in more than one slice; we must only emit `Remove` once the
/// last slice referencing it has dropped it.
#[derive(Debug, Default)]
struct DiscoveryState {
    /// For each slice (keyed by its UID), the set of ready addresses we
    /// last observed in that slice.
    slices: HashMap<String, HashSet<SocketAddr>>,
    /// Reference count of each address across all slices. Drives the
    /// emit-once semantics required by `tonic::transport::channel::Change`.
    refcount: HashMap<SocketAddr, usize>,
    /// During an `Init` → `InitDone` window (watcher cold start or restart
    /// after a disconnect), records the slice UIDs delivered via
    /// `InitApply`. On `InitDone` we evict slices we did NOT see, so that
    /// pods that disappeared while we were disconnected are removed from
    /// the balance channel.
    init_seen: Option<HashSet<String>>,
}

impl DiscoveryState {
    /// Apply a slice: diff against the previously-known set for the same
    /// slice UID and emit `Insert`/`Remove` actions, updating refcounts.
    fn apply_slice(&mut self, uid: String, current: HashSet<SocketAddr>) -> Vec<EndpointAction> {
        let prev = self.slices.remove(&uid).unwrap_or_default();
        let mut actions = Vec::new();

        // Addresses dropped from this slice
        for addr in &prev {
            if !current.contains(addr) {
                self.decref(*addr, &mut actions);
            }
        }

        // Addresses newly present in this slice
        for addr in &current {
            if !prev.contains(addr) {
                self.incref(*addr, &mut actions);
            }
        }

        self.slices.insert(uid, current);
        actions
    }

    /// Drop a slice entirely (the `EndpointSlice` object was deleted).
    fn delete_slice(&mut self, uid: &str) -> Vec<EndpointAction> {
        let Some(prev) = self.slices.remove(uid) else {
            return Vec::new();
        };

        let mut actions = Vec::new();
        for addr in prev {
            self.decref(addr, &mut actions);
        }

        actions
    }

    fn incref(&mut self, addr: SocketAddr, actions: &mut Vec<EndpointAction>) {
        let count = self.refcount.entry(addr).or_insert(0);
        *count += 1;
        if *count == 1 {
            debug!("adding endpoint: {addr}");
            actions.push(EndpointAction::Insert(addr));
        }
    }

    fn decref(&mut self, addr: SocketAddr, actions: &mut Vec<EndpointAction>) {
        if let Some(count) = self.refcount.get_mut(&addr) {
            *count -= 1;
            if *count == 0 {
                self.refcount.remove(&addr);
                debug!("removing endpoint: {addr}");
                actions.push(EndpointAction::Remove(addr));
            }
        }
    }
}

/// Best-effort identity for an `EndpointSlice`. Prefers the apiserver-
/// assigned UID; falls back to namespace/name when UID is missing (only
/// expected in synthetic test fixtures).
fn slice_id(slice: &EndpointSlice) -> String {
    if let Some(uid) = slice.metadata.uid.as_deref() {
        return uid.to_string();
    }

    let ns = slice.metadata.namespace.as_deref().unwrap_or("");
    let name = slice.metadata.name.as_deref().unwrap_or("");
    format!("{ns}/{name}")
}

/// Processes a watcher event and returns the endpoint actions.
///
/// This function is extracted to enable unit testing of the event processing logic.
fn process_event(
    event: &Event<EndpointSlice>,
    state: &mut DiscoveryState,
    port: &Port,
) -> Vec<EndpointAction> {
    match event {
        Event::Apply(slice) => {
            let uid = slice_id(slice);
            let current = extract_ready_endpoints(slice, port);
            state.apply_slice(uid, current)
        }

        Event::InitApply(slice) => {
            let uid = slice_id(slice);
            if let Some(seen) = state.init_seen.as_mut() {
                seen.insert(uid.clone());
            }
            let current = extract_ready_endpoints(slice, port);
            state.apply_slice(uid, current)
        }

        Event::Delete(slice) => {
            let uid = slice_id(slice);
            state.delete_slice(&uid)
        }

        Event::Init => {
            debug!("Kubernetes watcher init starting");
            state.init_seen = Some(HashSet::new());
            Vec::new()
        }

        Event::InitDone => {
            debug!("Kubernetes watcher init complete");
            let seen = state.init_seen.take().unwrap_or_default();
            let stale_uids: Vec<String> = state
                .slices
                .keys()
                .filter(|uid| !seen.contains(*uid))
                .cloned()
                .collect();

            let mut actions = Vec::new();
            for uid in stale_uids {
                actions.extend(state.delete_slice(&uid));
            }

            actions
        }
    }
}

/// Extracts ready endpoint addresses from an `EndpointSlice`.
fn extract_ready_endpoints(slice: &EndpointSlice, port: &Port) -> HashSet<SocketAddr> {
    // Resolve the port number
    let port_number = match port {
        Port::Number(n) => Some(*n),
        Port::Name(name) => slice.ports.as_ref().and_then(|ports| {
            ports
                .iter()
                .find(|p| p.name.as_deref() == Some(name.as_str()))
                .and_then(|p| p.port)
                .and_then(|p| u16::try_from(p).ok())
        }),
    };

    let Some(port_number) = port_number else {
        return HashSet::new();
    };

    let mut addrs = HashSet::new();

    for ep in &slice.endpoints {
        // EndpointConditions semantics (k8s discovery v1):
        //   * `ready`       - prepared to receive new traffic. `None` is
        //                     treated as ready.
        //   * `terminating` - pod is going away; per upstream guidance,
        //                     load-balancer clients picking targets for
        //                     NEW connections should treat this as not
        //                     ready, even if `ready` briefly lags at
        //                     `Some(true)` during shutdown.
        let (ready, terminating) = match ep.conditions.as_ref() {
            Some(c) => (c.ready.unwrap_or(true), c.terminating.unwrap_or(false)),
            None => (true, false),
        };

        if !ready || terminating {
            continue;
        }

        for addr in &ep.addresses {
            if let Ok(ip) = addr.parse::<IpAddr>() {
                addrs.insert(SocketAddr::new(ip, port_number));
            }
        }
    }

    addrs
}

#[cfg(test)]
mod tests {
    use futures::stream;
    use k8s_openapi::api::discovery::v1::{Endpoint, EndpointConditions, EndpointPort};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::mpsc;
    use tonic::transport::Endpoint as TonicEndpoint;

    use super::*;

    // Port conversion tests

    #[test]
    fn port_from_u16() {
        let port: Port = 50051_u16.into();
        assert_eq!(port, Port::Number(50051));
    }

    #[test]
    fn port_from_str() {
        let port: Port = "grpc".into();
        assert_eq!(port, Port::Name("grpc".to_string()));
    }

    #[test]
    fn port_from_string() {
        let port: Port = String::from("grpc").into();
        assert_eq!(port, Port::Name("grpc".to_string()));
    }

    // DiscoveryConfig tests

    #[test]
    fn config_new_with_numeric_port() {
        let config = DiscoveryConfig::new("my-service", 50051_u16);

        assert_eq!(config.service_name, "my-service");
        assert!(config.namespace.is_none());
        assert_eq!(config.port, Port::Number(50051));
    }

    #[test]
    fn config_new_with_named_port() {
        let config = DiscoveryConfig::new("my-service", "grpc");

        assert_eq!(config.service_name, "my-service");
        assert!(config.namespace.is_none());
        assert_eq!(config.port, Port::Name("grpc".to_string()));
    }

    #[test]
    fn config_with_namespace() {
        let config = DiscoveryConfig::new("my-service", 50051_u16).namespace("my-namespace");

        assert_eq!(config.service_name, "my-service");
        assert_eq!(config.namespace, Some("my-namespace".to_string()));
        assert_eq!(config.port, Port::Number(50051));
    }

    // Helper to create an endpoint with addresses and optional ready condition
    fn make_endpoint(addresses: Vec<&str>, ready: Option<bool>) -> Endpoint {
        Endpoint {
            addresses: addresses.into_iter().map(String::from).collect(),
            conditions: Some(EndpointConditions {
                ready,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    // Helper to create an endpoint port
    fn make_port(name: Option<&str>, port: i32) -> EndpointPort {
        EndpointPort {
            name: name.map(String::from),
            port: Some(port),
            ..Default::default()
        }
    }

    // extract_ready_endpoints tests

    #[test]
    fn extract_ready_endpoints_empty_slice() {
        let slice = EndpointSlice {
            endpoints: Vec::new(),
            ..Default::default()
        };

        let addrs = extract_ready_endpoints(&slice, &Port::Number(50051));
        assert!(addrs.is_empty());
    }

    #[test]
    fn extract_ready_endpoints_with_numeric_port() {
        let slice = EndpointSlice {
            endpoints: vec![make_endpoint(vec!["10.0.0.1", "10.0.0.2"], Some(true))],
            ..Default::default()
        };

        let addrs = extract_ready_endpoints(&slice, &Port::Number(50051));

        assert_eq!(addrs.len(), 2);
        assert!(addrs.contains(&"10.0.0.1:50051".parse().unwrap()));
        assert!(addrs.contains(&"10.0.0.2:50051".parse().unwrap()));
    }

    #[test]
    fn extract_ready_endpoints_with_named_port() {
        let slice = EndpointSlice {
            endpoints: vec![make_endpoint(vec!["10.0.0.1"], Some(true))],
            ports: Some(vec![make_port(Some("grpc"), 9090)]),
            ..Default::default()
        };

        let addrs = extract_ready_endpoints(&slice, &Port::Name("grpc".to_string()));

        assert_eq!(addrs.len(), 1);
        assert!(addrs.contains(&"10.0.0.1:9090".parse().unwrap()));
    }

    #[test]
    fn extract_ready_endpoints_named_port_not_found() {
        let slice = EndpointSlice {
            endpoints: vec![make_endpoint(vec!["10.0.0.1"], Some(true))],
            ports: Some(vec![make_port(Some("http"), 8080)]),
            ..Default::default()
        };

        let addrs = extract_ready_endpoints(&slice, &Port::Name("grpc".to_string()));
        assert!(addrs.is_empty());
    }

    #[test]
    fn extract_ready_endpoints_named_port_no_ports_defined() {
        let slice = EndpointSlice {
            endpoints: vec![make_endpoint(vec!["10.0.0.1"], Some(true))],
            ports: None,
            ..Default::default()
        };

        let addrs = extract_ready_endpoints(&slice, &Port::Name("grpc".to_string()));
        assert!(addrs.is_empty());
    }

    #[test]
    fn extract_ready_endpoints_skips_not_ready() {
        let slice = EndpointSlice {
            endpoints: vec![
                make_endpoint(vec!["10.0.0.1"], Some(true)),
                make_endpoint(vec!["10.0.0.2"], Some(false)),
            ],
            ..Default::default()
        };

        let addrs = extract_ready_endpoints(&slice, &Port::Number(50051));

        assert_eq!(addrs.len(), 1);
        assert!(addrs.contains(&"10.0.0.1:50051".parse().unwrap()));
    }

    #[test]
    fn extract_ready_endpoints_ready_defaults_to_true() {
        // When ready is None, it should default to true
        let slice = EndpointSlice {
            endpoints: vec![Endpoint {
                addresses: vec!["10.0.0.1".to_string()],
                conditions: Some(EndpointConditions {
                    ready: None,
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        };

        let addrs = extract_ready_endpoints(&slice, &Port::Number(50051));

        assert_eq!(addrs.len(), 1);
        assert!(addrs.contains(&"10.0.0.1:50051".parse().unwrap()));
    }

    #[test]
    fn extract_ready_endpoints_no_conditions_defaults_to_ready() {
        // When conditions is None entirely, should default to ready
        let slice = EndpointSlice {
            endpoints: vec![Endpoint {
                addresses: vec!["10.0.0.1".to_string()],
                conditions: None,
                ..Default::default()
            }],
            ..Default::default()
        };

        let addrs = extract_ready_endpoints(&slice, &Port::Number(50051));

        assert_eq!(addrs.len(), 1);
        assert!(addrs.contains(&"10.0.0.1:50051".parse().unwrap()));
    }

    #[test]
    fn extract_ready_endpoints_skips_invalid_ip() {
        let slice = EndpointSlice {
            endpoints: vec![Endpoint {
                addresses: vec!["not-an-ip".to_string(), "10.0.0.1".to_string()],
                conditions: Some(EndpointConditions {
                    ready: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        };

        let addrs = extract_ready_endpoints(&slice, &Port::Number(50051));

        assert_eq!(addrs.len(), 1);
        assert!(addrs.contains(&"10.0.0.1:50051".parse().unwrap()));
    }

    #[test]
    fn extract_ready_endpoints_ipv6() {
        let slice = EndpointSlice {
            endpoints: vec![make_endpoint(vec!["::1", "2001:db8::1"], Some(true))],
            ..Default::default()
        };

        let addrs = extract_ready_endpoints(&slice, &Port::Number(50051));

        assert_eq!(addrs.len(), 2);
        assert!(addrs.contains(&"[::1]:50051".parse().unwrap()));
        assert!(addrs.contains(&"[2001:db8::1]:50051".parse().unwrap()));
    }

    #[test]
    fn extract_ready_endpoints_multiple_endpoints() {
        let slice = EndpointSlice {
            endpoints: vec![
                make_endpoint(vec!["10.0.0.1"], Some(true)),
                make_endpoint(vec!["10.0.0.2"], Some(true)),
                make_endpoint(vec!["10.0.0.3"], Some(true)),
            ],
            ..Default::default()
        };

        let addrs = extract_ready_endpoints(&slice, &Port::Number(50051));

        assert_eq!(addrs.len(), 3);
        assert!(addrs.contains(&"10.0.0.1:50051".parse().unwrap()));
        assert!(addrs.contains(&"10.0.0.2:50051".parse().unwrap()));
        assert!(addrs.contains(&"10.0.0.3:50051".parse().unwrap()));
    }

    #[test]
    fn extract_ready_endpoints_deduplicates_addresses() {
        // Same address in multiple endpoints should only appear once
        let slice = EndpointSlice {
            endpoints: vec![
                make_endpoint(vec!["10.0.0.1"], Some(true)),
                make_endpoint(vec!["10.0.0.1"], Some(true)),
            ],
            ..Default::default()
        };

        let addrs = extract_ready_endpoints(&slice, &Port::Number(50051));

        assert_eq!(addrs.len(), 1);
        assert!(addrs.contains(&"10.0.0.1:50051".parse().unwrap()));
    }

    #[test]
    fn extract_ready_endpoints_multiple_ports_finds_correct_one() {
        let slice = EndpointSlice {
            endpoints: vec![make_endpoint(vec!["10.0.0.1"], Some(true))],
            ports: Some(vec![
                make_port(Some("http"), 8080),
                make_port(Some("grpc"), 9090),
                make_port(Some("metrics"), 9100),
            ]),
            ..Default::default()
        };

        let addrs = extract_ready_endpoints(&slice, &Port::Name("grpc".to_string()));

        assert_eq!(addrs.len(), 1);
        assert!(addrs.contains(&"10.0.0.1:9090".parse().unwrap()));
    }

    // process_event tests

    /// Helper: a slice with a stable UID so subsequent applies update the
    /// same per-slice state.
    fn slice_with(uid: &str, addresses: &[&str], ready: Option<bool>) -> EndpointSlice {
        slice_with_terminating(uid, addresses, ready, None)
    }

    fn slice_with_terminating(
        uid: &str,
        addresses: &[&str],
        ready: Option<bool>,
        terminating: Option<bool>,
    ) -> EndpointSlice {
        EndpointSlice {
            metadata: kube::core::ObjectMeta {
                name: Some(format!("svc-{uid}")),
                uid: Some(uid.to_string()),
                ..Default::default()
            },
            endpoints: vec![Endpoint {
                addresses: addresses.iter().map(|s| (*s).to_string()).collect(),
                conditions: Some(EndpointConditions {
                    ready,
                    terminating,
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn addr(s: &str) -> SocketAddr {
        s.parse().expect("valid SocketAddr")
    }

    #[test]
    fn process_event_apply_inserts_new_endpoints() {
        let slice = slice_with("uid-1", &["10.0.0.1", "10.0.0.2"], Some(true));

        let mut state = DiscoveryState::default();
        let actions = process_event(&Event::Apply(slice), &mut state, &Port::Number(50051));

        assert_eq!(actions.len(), 2);
        assert!(actions.contains(&EndpointAction::Insert(addr("10.0.0.1:50051"))));
        assert!(actions.contains(&EndpointAction::Insert(addr("10.0.0.2:50051"))));
        assert_eq!(state.refcount.len(), 2);
    }

    #[test]
    fn process_event_apply_skips_known_endpoints() {
        let slice = slice_with("uid-1", &["10.0.0.1", "10.0.0.2"], Some(true));

        let mut state = DiscoveryState::default();
        // Pre-seed by applying the same slice once
        let _ = process_event(
            &Event::Apply(slice_with("uid-1", &["10.0.0.1"], Some(true))),
            &mut state,
            &Port::Number(50051),
        );

        let actions = process_event(&Event::Apply(slice), &mut state, &Port::Number(50051));

        // Only 10.0.0.2 is newly added
        assert_eq!(
            actions,
            vec![EndpointAction::Insert(addr("10.0.0.2:50051"))]
        );
        assert_eq!(state.refcount.len(), 2);
    }

    #[test]
    fn process_event_init_apply_inserts_endpoints() {
        let slice = slice_with("uid-1", &["10.0.0.1"], Some(true));

        let mut state = DiscoveryState {
            init_seen: Some(HashSet::new()),
            ..Default::default()
        };
        let actions = process_event(&Event::InitApply(slice), &mut state, &Port::Number(50051));

        assert_eq!(
            actions,
            vec![EndpointAction::Insert(addr("10.0.0.1:50051"))]
        );
    }

    #[test]
    fn process_event_delete_removes_known_endpoints() {
        let slice = slice_with("uid-1", &["10.0.0.1", "10.0.0.2"], Some(true));

        let mut state = DiscoveryState::default();
        let _ = process_event(
            &Event::Apply(slice.clone()),
            &mut state,
            &Port::Number(50051),
        );

        let actions = process_event(&Event::Delete(slice), &mut state, &Port::Number(50051));

        assert_eq!(actions.len(), 2);
        assert!(actions.contains(&EndpointAction::Remove(addr("10.0.0.1:50051"))));
        assert!(actions.contains(&EndpointAction::Remove(addr("10.0.0.2:50051"))));
        assert!(state.refcount.is_empty());
        assert!(state.slices.is_empty());
    }

    #[test]
    fn process_event_delete_unknown_slice_is_noop() {
        let slice = slice_with("uid-unknown", &["10.0.0.1"], Some(true));

        let mut state = DiscoveryState::default();
        let actions = process_event(&Event::Delete(slice), &mut state, &Port::Number(50051));

        assert!(actions.is_empty());
    }

    #[test]
    fn process_event_init_returns_empty_and_arms_seen() {
        let mut state = DiscoveryState::default();
        let actions = process_event(&Event::Init, &mut state, &Port::Number(50051));

        assert!(actions.is_empty());
        assert!(state.init_seen.is_some());
    }

    #[test]
    fn process_event_init_done_returns_empty_when_no_state() {
        let mut state = DiscoveryState::default();
        let _ = process_event(&Event::Init, &mut state, &Port::Number(50051));
        let actions = process_event(&Event::InitDone, &mut state, &Port::Number(50051));

        assert!(actions.is_empty());
        assert!(state.init_seen.is_none());
    }

    /// Regression test for the production bug observed with Cerbos:
    ///
    /// When a pod is removed (rolling deploy, scale-down, crash) the
    /// `EndpointSlice` is **updated**, not deleted. A subsequent
    /// `Event::Apply` arrives with one fewer entry in `slice.endpoints`.
    /// We must emit `Remove` for the dropped address so the tonic balance
    /// channel stops routing to the dead pod IP.
    #[test]
    fn process_event_apply_removes_dropped_endpoints_from_same_slice() {
        let slice_v1 = slice_with("uid-1", &["10.0.0.1", "10.0.0.2"], Some(true));
        let slice_v2 = slice_with("uid-1", &["10.0.0.1"], Some(true));

        let mut state = DiscoveryState::default();

        let first = process_event(&Event::Apply(slice_v1), &mut state, &Port::Number(50051));
        assert_eq!(first.len(), 2);
        assert!(first.contains(&EndpointAction::Insert(addr("10.0.0.1:50051"))));
        assert!(first.contains(&EndpointAction::Insert(addr("10.0.0.2:50051"))));

        let second = process_event(&Event::Apply(slice_v2), &mut state, &Port::Number(50051));

        assert_eq!(
            second,
            vec![EndpointAction::Remove(addr("10.0.0.2:50051"))],
            "second Apply must emit Remove for the dropped 10.0.0.2 endpoint"
        );
        assert!(!state.refcount.contains_key(&addr("10.0.0.2:50051")));
    }

    /// An endpoint flipping `ready: true → false` (e.g. pod readiness
    /// probe failing during graceful shutdown) must be removed from the
    /// balance channel even though the slice itself is still present.
    #[test]
    fn process_event_apply_removes_endpoint_that_became_not_ready() {
        let mut state = DiscoveryState::default();
        let _ = process_event(
            &Event::Apply(slice_with("uid-1", &["10.0.0.1"], Some(true))),
            &mut state,
            &Port::Number(50051),
        );

        let actions = process_event(
            &Event::Apply(slice_with("uid-1", &["10.0.0.1"], Some(false))),
            &mut state,
            &Port::Number(50051),
        );

        assert_eq!(
            actions,
            vec![EndpointAction::Remove(addr("10.0.0.1:50051"))]
        );
    }

    /// `terminating: true` must exclude the endpoint even when `ready`
    /// briefly remains `true` during a graceful shutdown — this is the
    /// upstream-recommended rule for load-balancer clients picking
    /// targets for new connections.
    #[test]
    fn extract_ready_endpoints_skips_terminating() {
        let slice = slice_with_terminating(
            "uid-1",
            &["10.0.0.1"],
            Some(true), // ready still true
            Some(true), // but terminating
        );

        let addrs = extract_ready_endpoints(&slice, &Port::Number(50051));
        assert!(
            addrs.is_empty(),
            "terminating endpoints must be excluded even when ready=true"
        );
    }

    /// A pod that flips into `terminating=true` while still ready must
    /// be removed from the channel on the next Apply event.
    #[test]
    fn process_event_apply_removes_endpoint_that_became_terminating() {
        let mut state = DiscoveryState::default();
        let _ = process_event(
            &Event::Apply(slice_with_terminating(
                "uid-1",
                &["10.0.0.1"],
                Some(true),
                Some(false),
            )),
            &mut state,
            &Port::Number(50051),
        );

        let actions = process_event(
            &Event::Apply(slice_with_terminating(
                "uid-1",
                &["10.0.0.1"],
                Some(true),
                Some(true),
            )),
            &mut state,
            &Port::Number(50051),
        );

        assert_eq!(
            actions,
            vec![EndpointAction::Remove(addr("10.0.0.1:50051"))]
        );
    }

    /// Multiple `EndpointSlice` shards can legitimately reference the
    /// same address (e.g. dual-stack or sharding edge cases). Removing
    /// it from one slice must NOT remove it from the channel while
    /// another slice still references it.
    #[test]
    fn process_event_apply_keeps_endpoint_referenced_by_other_slice() {
        let mut state = DiscoveryState::default();
        let _ = process_event(
            &Event::Apply(slice_with("uid-A", &["10.0.0.1"], Some(true))),
            &mut state,
            &Port::Number(50051),
        );
        let actions_b = process_event(
            &Event::Apply(slice_with("uid-B", &["10.0.0.1"], Some(true))),
            &mut state,
            &Port::Number(50051),
        );
        assert!(
            actions_b.is_empty(),
            "address already inserted by slice A; second slice must not re-insert"
        );

        // Slice A is deleted; address must remain because slice B still has it.
        let actions_del = process_event(
            &Event::Delete(slice_with("uid-A", &["10.0.0.1"], Some(true))),
            &mut state,
            &Port::Number(50051),
        );
        assert!(
            actions_del.is_empty(),
            "address still referenced by slice B; must not emit Remove"
        );
        assert!(state.refcount.contains_key(&addr("10.0.0.1:50051")));

        // Slice B is deleted; now the address must be removed.
        let actions_final = process_event(
            &Event::Delete(slice_with("uid-B", &["10.0.0.1"], Some(true))),
            &mut state,
            &Port::Number(50051),
        );
        assert_eq!(
            actions_final,
            vec![EndpointAction::Remove(addr("10.0.0.1:50051"))]
        );
    }

    /// After a watcher disconnect, `Init` arms the seen-set and the
    /// follow-up `InitApply` events declare the still-existing slices.
    /// Slices that were known before the disconnect but NOT seen during
    /// the init replay must be evicted on `InitDone`, otherwise dead
    /// pod IPs that disappeared during the disconnect window stay in
    /// the balance channel forever.
    #[test]
    fn process_event_init_done_evicts_slices_not_seen_during_replay() {
        let mut state = DiscoveryState::default();

        // Pre-disconnect: two slices known
        let _ = process_event(
            &Event::Apply(slice_with("uid-A", &["10.0.0.1"], Some(true))),
            &mut state,
            &Port::Number(50051),
        );
        let _ = process_event(
            &Event::Apply(slice_with("uid-B", &["10.0.0.2"], Some(true))),
            &mut state,
            &Port::Number(50051),
        );

        // Watcher reconnects: only uid-A still exists upstream
        let _ = process_event(&Event::Init, &mut state, &Port::Number(50051));
        let _ = process_event(
            &Event::InitApply(slice_with("uid-A", &["10.0.0.1"], Some(true))),
            &mut state,
            &Port::Number(50051),
        );
        let actions = process_event(&Event::InitDone, &mut state, &Port::Number(50051));

        assert_eq!(
            actions,
            vec![EndpointAction::Remove(addr("10.0.0.2:50051"))],
            "uid-B was not seen during init replay and must be evicted"
        );
        assert!(state.refcount.contains_key(&addr("10.0.0.1:50051")));
        assert!(!state.refcount.contains_key(&addr("10.0.0.2:50051")));
    }

    // run_event_stream tests: drive the discovery loop end-to-end against
    // a synthetic stream and a real `tokio::sync::mpsc` Sender, with no
    // Kubernetes API server. Covers the full code path that the prod
    // `discovery_loop` runs (process_event + send) and the build closure.

    fn build_endpoint(addr: SocketAddr) -> TonicEndpoint {
        TonicEndpoint::from_shared(format!("http://{addr}")).expect("valid endpoint")
    }

    /// Convenience: collect every `Change` the receiver currently has,
    /// reducing each to the simpler `EndpointAction` for assertions.
    async fn drain(
        rx: &mut mpsc::Receiver<Change<SocketAddr, TonicEndpoint>>,
    ) -> Vec<EndpointAction> {
        let mut out = Vec::new();
        while let Ok(Some(change)) =
            tokio::time::timeout(Duration::from_millis(100), rx.recv()).await
        {
            out.push(match change {
                Change::Insert(a, _) => EndpointAction::Insert(a),
                Change::Remove(a) => EndpointAction::Remove(a),
            });
        }
        out
    }

    /// Stream of `Ok(Event)` items with `std::io::Error` as the error
    /// type so the helper's generic `E: std::error::Error` bound is
    /// satisfied.
    fn ok_events(
        events: Vec<Event<EndpointSlice>>,
    ) -> impl Stream<Item = std::result::Result<Event<EndpointSlice>, std::io::Error>> {
        stream::iter(events.into_iter().map(Ok))
    }

    #[tokio::test]
    async fn run_event_stream_emits_inserts_for_apply() {
        let (tx, mut rx) = mpsc::channel(16);
        let events = vec![Event::Apply(slice_with(
            "uid-1",
            &["10.0.0.1", "10.0.0.2"],
            Some(true),
        ))];

        run_event_stream(
            ok_events(events),
            tx,
            build_endpoint,
            &Port::Number(50051),
            "default",
            "svc",
        )
        .await
        .expect("loop completes");

        let actions = drain(&mut rx).await;
        assert_eq!(actions.len(), 2);
        assert!(actions.contains(&EndpointAction::Insert(addr("10.0.0.1:50051"))));
        assert!(actions.contains(&EndpointAction::Insert(addr("10.0.0.2:50051"))));
    }

    /// End-to-end regression for the production bug: an Apply with
    /// fewer endpoints than the previous Apply must surface a `Remove`
    /// on the receiver side, not just internally in `process_event`.
    #[tokio::test]
    async fn run_event_stream_emits_remove_when_apply_drops_endpoint() {
        let (tx, mut rx) = mpsc::channel(16);
        let events = vec![
            Event::Apply(slice_with("uid-1", &["10.0.0.1", "10.0.0.2"], Some(true))),
            Event::Apply(slice_with("uid-1", &["10.0.0.1"], Some(true))),
        ];

        run_event_stream(
            ok_events(events),
            tx,
            build_endpoint,
            &Port::Number(50051),
            "default",
            "svc",
        )
        .await
        .expect("loop completes");

        let actions = drain(&mut rx).await;
        // Order matters: both Inserts before the Remove.
        assert_eq!(actions.len(), 3);
        assert_eq!(actions[2], EndpointAction::Remove(addr("10.0.0.2:50051")));
    }

    /// `Init` -> `InitApply` -> `InitDone` over the wire, with one
    /// pre-existing slice that doesn't get replayed. Verifies the
    /// reconcile Remove makes it through the channel.
    #[tokio::test]
    async fn run_event_stream_init_done_evicts_unseen_slices() {
        let (tx, mut rx) = mpsc::channel(16);
        let events = vec![
            // Cold-start state from a previous watcher session
            Event::Apply(slice_with("uid-A", &["10.0.0.1"], Some(true))),
            Event::Apply(slice_with("uid-B", &["10.0.0.2"], Some(true))),
            // Watcher reconnects; only uid-A still exists upstream.
            Event::Init,
            Event::InitApply(slice_with("uid-A", &["10.0.0.1"], Some(true))),
            Event::InitDone,
        ];

        run_event_stream(
            ok_events(events),
            tx,
            build_endpoint,
            &Port::Number(50051),
            "default",
            "svc",
        )
        .await
        .expect("loop completes");

        let actions = drain(&mut rx).await;
        assert_eq!(actions.len(), 3);
        assert_eq!(actions[0], EndpointAction::Insert(addr("10.0.0.1:50051")));
        assert_eq!(actions[1], EndpointAction::Insert(addr("10.0.0.2:50051")));
        assert_eq!(actions[2], EndpointAction::Remove(addr("10.0.0.2:50051")));
    }

    /// If the receiver is dropped, the loop must exit cleanly with
    /// `Ok(())` instead of panicking or returning an error. Mirrors
    /// what happens when the user-side balance channel goes away.
    #[tokio::test]
    async fn run_event_stream_returns_ok_when_receiver_dropped() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);

        let events = vec![Event::Apply(slice_with("uid-1", &["10.0.0.1"], Some(true)))];

        let result = run_event_stream(
            ok_events(events),
            tx,
            build_endpoint,
            &Port::Number(50051),
            "default",
            "svc",
        )
        .await;

        assert!(result.is_ok(), "must exit cleanly when receiver is dropped");
    }

    /// Stream errors (e.g. apiserver disconnects past the backoff
    /// retries) must propagate as `Err`, not be swallowed.
    #[tokio::test]
    async fn run_event_stream_propagates_stream_error() {
        let (tx, _rx) = mpsc::channel(16);
        let err = std::io::Error::other("watch failed");
        let events: Vec<std::result::Result<Event<EndpointSlice>, std::io::Error>> = vec![
            Ok(Event::Apply(slice_with("uid-1", &["10.0.0.1"], Some(true)))),
            Err(err),
        ];

        let result = run_event_stream(
            stream::iter(events),
            tx,
            build_endpoint,
            &Port::Number(50051),
            "default",
            "svc",
        )
        .await;

        assert!(result.is_err());
    }

    /// The user-supplied `build` closure must be called exactly once
    /// per `Insert` and never for a `Remove`. This guards against
    /// regressions that would either build a fresh `Endpoint` for
    /// every event (wasteful, breaks tonic's connection reuse) or
    /// skip building entirely (panics downstream).
    #[tokio::test]
    async fn run_event_stream_invokes_build_only_for_inserts() {
        let (tx, mut rx) = mpsc::channel(16);
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = Arc::clone(&calls);

        let events = vec![
            Event::Apply(slice_with("uid-1", &["10.0.0.1", "10.0.0.2"], Some(true))),
            Event::Apply(slice_with("uid-1", &["10.0.0.1"], Some(true))),
        ];

        run_event_stream(
            ok_events(events),
            tx,
            move |a| {
                calls_clone.fetch_add(1, Ordering::SeqCst);
                build_endpoint(a)
            },
            &Port::Number(50051),
            "default",
            "svc",
        )
        .await
        .expect("loop completes");

        let _ = drain(&mut rx).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "build must be invoked once per Insert (2), never for the Remove"
        );
    }
}
