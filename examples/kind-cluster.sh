#!/usr/bin/env bash
#
# Create a local kind cluster for tonic-lb-k8s examples.
#
# This script provisions a simple kind (Kubernetes in Docker) cluster
# suitable for deploying and testing the greeter examples.
#
# Usage:
#   ./kind-cluster.sh [options]
#
# Options:
#   --create         Create the kind cluster (default)
#   --delete         Delete the kind cluster
#   --status         Show cluster status
#   --name NAME      Cluster name (default: tonic-lb-k8s)
#   --help           Show this help message
#
# Requirements:
#   - Docker (running)
#   - kind (https://kind.sigs.k8s.io/)

set -euo pipefail

# Default values
ACTION="create"
CLUSTER_NAME="tonic-lb-k8s"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

log_info() {
    echo -e "${GREEN}[INFO]${NC} $1"
}

log_warn() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

log_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

log_step() {
    echo -e "${BLUE}[STEP]${NC} $1"
}

show_help() {
    head -20 "$0" | tail -18
    exit 0
}

# Parse command line arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        --create)
            ACTION="create"
            shift
            ;;
        --delete)
            ACTION="delete"
            shift
            ;;
        --status)
            ACTION="status"
            shift
            ;;
        --name)
            CLUSTER_NAME="$2"
            shift 2
            ;;
        --help|-h)
            show_help
            ;;
        *)
            log_error "Unknown option: $1"
            show_help
            ;;
    esac
done

check_prerequisites() {
    log_step "Checking prerequisites..."
    
    # Check for Docker
    if ! command -v docker &> /dev/null; then
        log_error "Docker is not installed. Please install Docker first."
        exit 1
    fi
    
    # Check if Docker is running
    if ! docker info &> /dev/null; then
        log_error "Docker is not running. Please start Docker first."
        exit 1
    fi
    
    # Check for kind
    if ! command -v kind &> /dev/null; then
        log_error "kind is not installed."
        echo ""
        echo "Install kind using one of these methods:"
        echo "  brew install kind              # macOS"
        echo "  go install sigs.k8s.io/kind@latest  # Go"
        echo "  See: https://kind.sigs.k8s.io/docs/user/quick-start/#installation"
        exit 1
    fi
    
    # Check for kubectl
    if ! command -v kubectl &> /dev/null; then
        log_warn "kubectl is not installed. You'll need it to interact with the cluster."
    fi
    
    log_info "Prerequisites satisfied"
}

cluster_exists() {
    kind get clusters 2>/dev/null | grep -q "^${CLUSTER_NAME}$"
}

create_cluster() {
    log_step "Creating kind cluster '${CLUSTER_NAME}'..."
    
    if cluster_exists; then
        log_warn "Cluster '${CLUSTER_NAME}' already exists"
        show_status
        return 0
    fi
    
    # Create a kind cluster with a simple configuration
    # We use a single control-plane node which is sufficient for examples
    cat <<EOF | kind create cluster --name "${CLUSTER_NAME}" --config=-
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes:
  - role: control-plane
    # Enable the NodePort range for potential future use
    kubeadmConfigPatches:
      - |
        kind: InitConfiguration
        nodeRegistration:
          kubeletExtraArgs:
            node-labels: "ingress-ready=true"
EOF
    
    log_info "Cluster '${CLUSTER_NAME}' created successfully!"
    echo ""
    
    # Wait for the cluster to be ready
    log_step "Waiting for cluster to be ready..."
    kubectl wait --for=condition=Ready nodes --all --timeout=60s
    
    log_info "Cluster is ready!"
    echo ""
    
    show_status
    show_next_steps
}

delete_cluster() {
    log_step "Deleting kind cluster '${CLUSTER_NAME}'..."
    
    if ! cluster_exists; then
        log_warn "Cluster '${CLUSTER_NAME}' does not exist"
        return 0
    fi
    
    kind delete cluster --name "${CLUSTER_NAME}"
    log_info "Cluster '${CLUSTER_NAME}' deleted successfully!"
}

show_status() {
    echo -e "${BLUE}=== Cluster Status ===${NC}"
    
    if ! cluster_exists; then
        log_warn "Cluster '${CLUSTER_NAME}' does not exist"
        return 0
    fi
    
    echo ""
    echo "Cluster: ${CLUSTER_NAME}"
    echo "Context: kind-${CLUSTER_NAME}"
    echo ""
    
    # Show nodes
    echo "Nodes:"
    kubectl get nodes -o wide 2>/dev/null || log_warn "Unable to get nodes (is kubectl configured?)"
    echo ""
    
    # Show namespaces
    echo "Namespaces:"
    kubectl get namespaces 2>/dev/null || true
}

show_next_steps() {
    echo -e "${BLUE}=== Next Steps ===${NC}"
    echo ""
    echo "Your kind cluster is ready! To deploy the examples:"
    echo ""
    echo "  1. Build and deploy the greeter example:"
    echo "     ./deploy.sh"
    echo ""
    echo "  2. Watch the client logs:"
    echo "     kubectl logs -f job/greeter-client -n tonic-lb-k8s-demo"
    echo ""
    echo "  3. Clean up when done:"
    echo "     ./deploy.sh --cleanup"
    echo "     ./kind-cluster.sh --delete"
    echo ""
    echo "Note: kind loads Docker images directly, so no registry is needed."
    echo "The deploy.sh script should auto-detect kind and load images appropriately."
}

# Main
case $ACTION in
    create)
        check_prerequisites
        create_cluster
        ;;
    delete)
        delete_cluster
        ;;
    status)
        show_status
        ;;
esac
