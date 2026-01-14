#!/usr/bin/env bash
#
# Deploy the tonic-lb-k8s example to a Kubernetes cluster.
#
# This script builds the Docker images and deploys the greeter server
# and client to demonstrate Kubernetes-based gRPC load balancing.
#
# Usage:
#   ./deploy.sh [options]
#
# Options:
#   --build-only     Build images without deploying
#   --deploy-only    Deploy without building (images must exist)
#   --cleanup        Remove all deployed resources
#   --registry URL   Push images to a registry (e.g., docker.io/myuser)
#   --help           Show this help message
#
# Requirements:
#   - Docker
#   - kubectl configured with cluster access
#   - Optionally: kind, minikube, or other local Kubernetes

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

# Default values
BUILD=true
DEPLOY=true
CLEANUP=false
REGISTRY=""

# Image names
SERVER_IMAGE="greeter-server:latest"
CLIENT_IMAGE="greeter-client:latest"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
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

show_help() {
    head -30 "$0" | tail -20
    exit 0
}

# Parse command line arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        --build-only)
            BUILD=true
            DEPLOY=false
            shift
            ;;
        --deploy-only)
            BUILD=false
            DEPLOY=true
            shift
            ;;
        --cleanup)
            CLEANUP=true
            BUILD=false
            DEPLOY=false
            shift
            ;;
        --registry)
            REGISTRY="$2"
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

cleanup() {
    log_info "Cleaning up resources..."
    
    kubectl delete -f "${SCRIPT_DIR}/k8s/client-job.yaml" --ignore-not-found=true || true
    kubectl delete -f "${SCRIPT_DIR}/k8s/client-rbac.yaml" --ignore-not-found=true || true
    kubectl delete -f "${SCRIPT_DIR}/k8s/server-deployment.yaml" --ignore-not-found=true || true
    kubectl delete -f "${SCRIPT_DIR}/k8s/server-service.yaml" --ignore-not-found=true || true
    kubectl delete -f "${SCRIPT_DIR}/k8s/namespace.yaml" --ignore-not-found=true || true
    
    log_info "Cleanup complete!"
}

build_images() {
    log_info "Building Docker images..."
    
    cd "${PROJECT_ROOT}"
    
    # Build server image
    log_info "Building server image..."
    docker build -t "${SERVER_IMAGE}" -f examples/docker/Dockerfile.server .
    
    # Build client image
    log_info "Building client image..."
    docker build -t "${CLIENT_IMAGE}" -f examples/docker/Dockerfile.client .
    
    if [[ -n "${REGISTRY}" ]]; then
        log_info "Tagging and pushing images to ${REGISTRY}..."
        
        docker tag "${SERVER_IMAGE}" "${REGISTRY}/greeter-server:latest"
        docker tag "${CLIENT_IMAGE}" "${REGISTRY}/greeter-client:latest"
        
        docker push "${REGISTRY}/greeter-server:latest"
        docker push "${REGISTRY}/greeter-client:latest"
        
        SERVER_IMAGE="${REGISTRY}/greeter-server:latest"
        CLIENT_IMAGE="${REGISTRY}/greeter-client:latest"
    fi
    
    log_info "Images built successfully!"
}

load_images_to_kind() {
    # Check if using kind and detect cluster name from kubectl context
    if command -v kind &> /dev/null; then
        local context
        context=$(kubectl config current-context 2>/dev/null || echo "")
        if [[ "${context}" == kind-* ]]; then
            local cluster_name="${context#kind-}"
            log_info "Detected kind cluster '${cluster_name}', loading images..."
            kind load docker-image "${SERVER_IMAGE}" --name "${cluster_name}" || log_warn "Failed to load server image to kind"
            kind load docker-image "${CLIENT_IMAGE}" --name "${cluster_name}" || log_warn "Failed to load client image to kind"
        fi
    fi
}

load_images_to_minikube() {
    # Check if using minikube
    if command -v minikube &> /dev/null && minikube status &>/dev/null; then
        log_info "Detected minikube, you may need to run: eval \$(minikube docker-env)"
    fi
}

deploy() {
    log_info "Deploying to Kubernetes..."
    
    # Load images to local cluster if applicable
    if [[ -z "${REGISTRY}" ]]; then
        load_images_to_kind
        load_images_to_minikube
    fi
    
    # Apply Kubernetes manifests in order
    log_info "Creating namespace..."
    kubectl apply -f "${SCRIPT_DIR}/k8s/namespace.yaml"
    
    log_info "Creating headless service..."
    kubectl apply -f "${SCRIPT_DIR}/k8s/server-service.yaml"
    
    log_info "Deploying server pods..."
    kubectl apply -f "${SCRIPT_DIR}/k8s/server-deployment.yaml"
    
    log_info "Setting up RBAC for client..."
    kubectl apply -f "${SCRIPT_DIR}/k8s/client-rbac.yaml"
    
    # Wait for server pods to be ready
    log_info "Waiting for server pods to be ready..."
    kubectl wait --namespace greeter-demo \
        --for=condition=ready pod \
        --selector=app=greeter-server \
        --timeout=120s
    
    # Show server pods
    log_info "Server pods running:"
    kubectl get pods -n greeter-demo -l app=greeter-server
    
    # Show EndpointSlice (what the client will discover)
    log_info "EndpointSlice for the service:"
    kubectl get endpointslices -n greeter-demo -l kubernetes.io/service-name=greeter-server
    
    # Delete any existing client job
    kubectl delete -f "${SCRIPT_DIR}/k8s/client-job.yaml" --ignore-not-found=true || true
    
    log_info "Running client job..."
    kubectl apply -f "${SCRIPT_DIR}/k8s/client-job.yaml"
    
    # Wait for the job to complete
    log_info "Waiting for client job to complete..."
    kubectl wait --namespace greeter-demo \
        --for=condition=complete job/greeter-client \
        --timeout=120s || {
        log_error "Job did not complete in time. Check logs:"
        echo "  kubectl logs -n greeter-demo -l app=greeter-client"
        exit 1
    }
    
    # Show client logs
    echo ""
    log_info "=== Client Output ==="
    kubectl logs -n greeter-demo -l app=greeter-client --tail=50
    echo ""
    
    log_info "Demo complete! The client successfully load balanced requests across server pods."
    echo ""
    log_info "To run the client again:"
    echo "  kubectl delete job -n greeter-demo greeter-client"
    echo "  kubectl apply -f ${SCRIPT_DIR}/k8s/client-job.yaml"
    echo "  kubectl logs -n greeter-demo -l app=greeter-client -f"
    echo ""
    log_info "To clean up:"
    echo "  $0 --cleanup"
}

# Main execution
if [[ "${CLEANUP}" == true ]]; then
    cleanup
    exit 0
fi

if [[ "${BUILD}" == true ]]; then
    build_images
fi

if [[ "${DEPLOY}" == true ]]; then
    deploy
fi

log_info "Done!"
