# tonic-lb-k8s Examples

This directory contains a complete example demonstrating Kubernetes-based gRPC load balancing using `tonic-lb-k8s`.

## Overview

The example consists of:

- **greeter-server**: A simple gRPC server that responds with a greeting and its pod name
- **greeter-client**: A gRPC client that uses `tonic-lb-k8s` to discover and load balance across server pods

When deployed to Kubernetes, the client watches `EndpointSlice` resources to discover server pod IPs and distributes requests across all healthy pods.

## Quick Start

### Prerequisites

- Rust 1.88+ with protobuf compiler (`protoc`)
- Docker
- Kubernetes cluster (local or remote)
- kubectl configured to access your cluster

### Create a Local Cluster (Optional)

If you don't have a Kubernetes cluster, you can create a local one using [kind](https://kind.sigs.k8s.io/):

```bash
# Create a local kind cluster
./kind-cluster.sh

# Check cluster status
./kind-cluster.sh --status

# Delete the cluster when done
./kind-cluster.sh --delete
```

### Deploy to Kubernetes

```bash
# Build images and deploy everything
./deploy.sh

# Or build images only
./deploy.sh --build-only

# Or deploy using pre-built images
./deploy.sh --deploy-only

# Clean up
./deploy.sh --cleanup
```

### Using a Container Registry

If your cluster can't access local Docker images (e.g., remote cluster):

```bash
./deploy.sh --registry docker.io/yourusername
```

### Local Development

Run the server locally:

```bash
cargo run --example greeter-server
```

Run the client locally (requires Kubernetes access):

```bash
SERVICE_NAME=my-service SERVICE_NAMESPACE=my-namespace cargo run --example greeter-client
```

## Project Structure

```
examples/
├── deploy.sh              # Deployment script
├── greeter-server.rs      # Server implementation
├── greeter-client.rs      # Client with load balancing
├── docker/
│   ├── Dockerfile.server  # Alpine-based server image
│   └── Dockerfile.client  # Alpine-based client image
└── k8s/
    ├── namespace.yaml         # Demo namespace
    ├── server-service.yaml    # Headless service
    ├── server-deployment.yaml # Server pods (3 replicas)
    ├── client-rbac.yaml       # RBAC for EndpointSlice access
    └── client-job.yaml        # Client job
```

## How It Works

### The Problem

Standard Kubernetes `ClusterIP` services perform connection-level load balancing. With HTTP/2 (used by gRPC), a single TCP connection is established and all requests are multiplexed over it. This means all requests go to the same pod, defeating load balancing.

### The Solution

1. **Headless Service**: We create a headless service (`clusterIP: None`) that doesn't perform load balancing
2. **EndpointSlice Discovery**: `tonic-lb-k8s` watches `EndpointSlice` resources to discover pod IPs
3. **Client-side Load Balancing**: Tonic's balance channel distributes requests across discovered endpoints

### Key Components

#### Server (`greeter-server.rs`)

A simple gRPC server that:
- Listens on a configurable port (default: 50051)
- Returns greetings with its pod name (from `HOSTNAME` env var)
- Uses tracing for structured logging

#### Client (`greeter-client.rs`)

A gRPC client that demonstrates `tonic-lb-k8s`:
- Creates a `Channel::balance_channel()` for client-side load balancing
- Uses `discover()` to watch Kubernetes endpoints
- Sends multiple requests and tracks which pods serve them
- Prints a summary showing request distribution

#### RBAC (`client-rbac.yaml`)

The client needs permissions to watch `EndpointSlice` resources:
- `list` and `watch` on `endpointslices` in the `discovery.k8s.io` API group

## RBAC Requirements

Any application using `tonic-lb-k8s` requires Kubernetes RBAC permissions to discover endpoints. The following permissions are required:

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
```

### Example RoleBinding

```yaml
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

### ClusterRole (for cross-namespace discovery)

If your client needs to discover services in multiple namespaces, use a `ClusterRole` and `ClusterRoleBinding` instead:

```yaml
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: endpointslice-reader
rules:
  - apiGroups: ["discovery.k8s.io"]
    resources: ["endpointslices"]
    verbs: ["list", "watch"]
```

## Expected Output

After deployment, the client job output should look like:

```
[INFO] Starting greeter client
[INFO] Service: greeter-server
[INFO] Namespace: greeter-demo
[INFO] Waiting for endpoint discovery...
[INFO] Sending 20 requests...

[INFO] Request 1: Hello, client-request-1! (served by: greeter-server-abc123)
[INFO] Request 2: Hello, client-request-2! (served by: greeter-server-def456)
[INFO] Request 3: Hello, client-request-3! (served by: greeter-server-ghi789)
...

[INFO] === Load Balancing Summary ===
[INFO] greeter-server-abc123: 7 requests (35.0%)
[INFO] greeter-server-def456: 6 requests (30.0%)
[INFO] greeter-server-ghi789: 7 requests (35.0%)
[INFO] Total pods used: 3
```

## Troubleshooting

### Images not found

For local clusters like Kind:
```bash
kind load docker-image greeter-server:latest
kind load docker-image greeter-client:latest
```

For Minikube:
```bash
eval $(minikube docker-env)
./deploy.sh --build-only
./deploy.sh --deploy-only
```

### Client can't discover endpoints

Check RBAC:
```bash
kubectl auth can-i list endpointslices -n greeter-demo --as=system:serviceaccount:greeter-demo:greeter-client
```

Check EndpointSlice exists:
```bash
kubectl get endpointslices -n greeter-demo -l kubernetes.io/service-name=greeter-server -o yaml
```

### Connection refused

Ensure server pods are ready:
```bash
kubectl get pods -n greeter-demo -l app=greeter-server
kubectl logs -n greeter-demo -l app=greeter-server
```
