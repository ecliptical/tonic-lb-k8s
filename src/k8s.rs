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
//! use std::net::SocketAddr;
//! use std::time::Duration;
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

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};

use futures::TryStreamExt;
use k8s_openapi::api::discovery::v1::EndpointSlice;
use kube::runtime::WatchStreamExt;
use kube::runtime::watcher::{self, Config as WatcherConfig, Event};
use kube::{Api, Client};
use tokio::sync::mpsc::Sender;
use tonic::transport::Endpoint;
use tonic::transport::channel::Change;

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
            tracing::error!("Kubernetes endpoint watcher failed: {e}");
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

    let mut known: HashSet<SocketAddr> = HashSet::new();
    let stream = watcher::watcher(slices, watcher_config).default_backoff();
    tokio::pin!(stream);

    tracing::debug!(
        "Starting Kubernetes endpoint watch for {namespace}/{} on port {:?}",
        config.service_name,
        config.port
    );

    while let Some(event) = stream.try_next().await? {
        let actions = process_event(&event, &mut known, &config.port);

        for action in actions {
            let change = match action {
                EndpointAction::Insert(addr) => Change::Insert(addr, build(addr)),
                EndpointAction::Remove(addr) => Change::Remove(addr),
            };

            if tx.send(change).await.is_err() {
                tracing::warn!("channel closed, stopping Kubernetes watcher");
                return Ok(());
            }
        }

        tracing::debug!(
            "Kubernetes discovery: {} endpoints for {namespace}/{}",
            known.len(),
            config.service_name
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

/// Processes a watcher event and returns the endpoint actions.
///
/// This function is extracted to enable unit testing of the event processing logic.
fn process_event(
    event: &Event<EndpointSlice>,
    known: &mut HashSet<SocketAddr>,
    port: &Port,
) -> Vec<EndpointAction> {
    match event {
        Event::Apply(slice) | Event::InitApply(slice) => {
            let current = extract_ready_endpoints(slice, port);
            let mut actions = Vec::new();

            for addr in current {
                if known.insert(addr) {
                    tracing::debug!("adding endpoint: {addr}");
                    actions.push(EndpointAction::Insert(addr));
                }
            }

            actions
        }

        Event::Delete(slice) => {
            let removed = extract_ready_endpoints(slice, port);
            let mut actions = Vec::new();

            for addr in removed {
                if known.remove(&addr) {
                    tracing::debug!("removing endpoint: {addr}");
                    actions.push(EndpointAction::Remove(addr));
                }
            }

            actions
        }

        Event::Init | Event::InitDone => {
            tracing::debug!("Kubernetes watcher initialization event");
            Vec::new()
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
        // An endpoint is ready if conditions.ready is true or unset (defaults to true)
        let ready = ep.conditions.as_ref().and_then(|c| c.ready).unwrap_or(true);

        if !ready {
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
    use k8s_openapi::api::discovery::v1::{Endpoint, EndpointConditions, EndpointPort};

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

    #[test]
    fn process_event_apply_inserts_new_endpoints() {
        let slice = EndpointSlice {
            endpoints: vec![make_endpoint(vec!["10.0.0.1", "10.0.0.2"], Some(true))],
            ..Default::default()
        };

        let mut known = HashSet::new();
        let actions = process_event(&Event::Apply(slice), &mut known, &Port::Number(50051));

        assert_eq!(actions.len(), 2);
        assert!(actions.contains(&EndpointAction::Insert("10.0.0.1:50051".parse().unwrap())));
        assert!(actions.contains(&EndpointAction::Insert("10.0.0.2:50051".parse().unwrap())));
        assert_eq!(known.len(), 2);
    }

    #[test]
    fn process_event_apply_skips_known_endpoints() {
        let slice = EndpointSlice {
            endpoints: vec![make_endpoint(vec!["10.0.0.1", "10.0.0.2"], Some(true))],
            ..Default::default()
        };

        let mut known = HashSet::new();
        known.insert("10.0.0.1:50051".parse().unwrap());

        let actions = process_event(&Event::Apply(slice), &mut known, &Port::Number(50051));

        // Only 10.0.0.2 should be inserted since 10.0.0.1 is already known
        assert_eq!(actions.len(), 1);
        assert!(actions.contains(&EndpointAction::Insert("10.0.0.2:50051".parse().unwrap())));
        assert_eq!(known.len(), 2);
    }

    #[test]
    fn process_event_init_apply_inserts_endpoints() {
        let slice = EndpointSlice {
            endpoints: vec![make_endpoint(vec!["10.0.0.1"], Some(true))],
            ..Default::default()
        };

        let mut known = HashSet::new();
        let actions = process_event(&Event::InitApply(slice), &mut known, &Port::Number(50051));

        assert_eq!(actions.len(), 1);
        assert!(actions.contains(&EndpointAction::Insert("10.0.0.1:50051".parse().unwrap())));
    }

    #[test]
    fn process_event_delete_removes_known_endpoints() {
        let slice = EndpointSlice {
            endpoints: vec![make_endpoint(vec!["10.0.0.1", "10.0.0.2"], Some(true))],
            ..Default::default()
        };

        let mut known = HashSet::new();
        known.insert("10.0.0.1:50051".parse().unwrap());
        known.insert("10.0.0.2:50051".parse().unwrap());

        let actions = process_event(&Event::Delete(slice), &mut known, &Port::Number(50051));

        assert_eq!(actions.len(), 2);
        assert!(actions.contains(&EndpointAction::Remove("10.0.0.1:50051".parse().unwrap())));
        assert!(actions.contains(&EndpointAction::Remove("10.0.0.2:50051".parse().unwrap())));
        assert!(known.is_empty());
    }

    #[test]
    fn process_event_delete_skips_unknown_endpoints() {
        let slice = EndpointSlice {
            endpoints: vec![make_endpoint(vec!["10.0.0.1", "10.0.0.2"], Some(true))],
            ..Default::default()
        };

        let mut known = HashSet::new();
        known.insert("10.0.0.1:50051".parse().unwrap());
        // 10.0.0.2 is not known

        let actions = process_event(&Event::Delete(slice), &mut known, &Port::Number(50051));

        // Only 10.0.0.1 should be removed since 10.0.0.2 wasn't known
        assert_eq!(actions.len(), 1);
        assert!(actions.contains(&EndpointAction::Remove("10.0.0.1:50051".parse().unwrap())));
        assert!(known.is_empty());
    }

    #[test]
    fn process_event_init_returns_empty() {
        let mut known = HashSet::new();
        let actions = process_event(&Event::Init, &mut known, &Port::Number(50051));

        assert!(actions.is_empty());
    }

    #[test]
    fn process_event_init_done_returns_empty() {
        let mut known = HashSet::new();
        let actions = process_event(&Event::InitDone, &mut known, &Port::Number(50051));

        assert!(actions.is_empty());
    }
}
