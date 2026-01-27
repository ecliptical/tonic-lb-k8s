#![deny(missing_docs)]
#![deny(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

//! [Tonic](https://docs.rs/tonic) client load balancing for Kubernetes.
//!
//! This crate watches Kubernetes `EndpointSlice` resources and feeds endpoint changes to a
//! Tonic balance channel, enabling client-side load balancing across pod replicas.
//!
//! # Why?
//!
//! Standard Kubernetes `ClusterIP` services don't load balance gRPC effectively. HTTP/2
//! multiplexes all requests over a single long-lived TCP connection, so all traffic goes
//! to one pod. This crate handles endpoint discovery and connection management automatically.
//!
//! # Usage
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
//! // Use with your generated gRPC client
//! // let client = MyServiceClient::new(channel);
//! ```

mod k8s;

pub use k8s::{DiscoveryConfig, Port, discover};
