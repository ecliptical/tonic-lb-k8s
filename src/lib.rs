#![deny(missing_docs)]
#![deny(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

//! Kubernetes endpoint discovery for [Tonic](https://docs.rs/tonic) gRPC load balancing.
//!
//! When using gRPC (HTTP/2) with Kubernetes, standard `ClusterIP` services don't load balance
//! effectively because HTTP/2 multiplexes all requests over a single long-lived TCP connection.
//! This crate watches Kubernetes `EndpointSlice` resources and feeds endpoint changes to a
//! user-provided Tonic balance channel.
//!
//! # Features
//!
//! - **Kubernetes API discovery**: Real-time endpoint updates via `EndpointSlice` watch
//! - **User-controlled channels**: You create the channel and endpoints however you want
//! - **Dynamic endpoint management**: Automatically adds/removes backends as pods scale
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
