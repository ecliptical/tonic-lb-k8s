# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.1] - 2026-05-14

### Fixed

- **Endpoint removal on `EndpointSlice` updates.** The watcher state
  machine only emitted `Change::Remove` on whole-slice deletion. Pod
  removals (rollouts, scale-down, crashes) are delivered as `Apply`
  events with one fewer entry in `slice.endpoints`; the previous
  implementation never diffed the new slice against the previous one,
  so dead pod IPs leaked into the tonic balance picker forever and
  surfaced as intermittent gRPC 5xx in production.
- Endpoints with `EndpointConditions.terminating == true` are now
  excluded from `extract_ready_endpoints`, even when `ready` briefly
  remains `true` during graceful shutdown.
- Watcher restarts now reconcile state correctly: slices known before
  a disconnect that are NOT replayed via `InitApply` are evicted on
  `InitDone`, preventing stale addresses from persisting across
  reconnects.

### Changed

- Internal `process_event` state replaced with a `DiscoveryState`
  struct that tracks per-slice address sets keyed by `EndpointSlice`
  UID and a global refcount of each address across slices. Emits
  `Remove` only when the last slice referencing an address drops it,
  correctly handling Services backed by multiple slices.

### Documentation

- New "Production Hardening" section in the README covering TLS SNI
  when targeting pod IPs, HTTP/2 keep-alive settings, idempotent
  retry as defense-in-depth, and server-side
  `terminationGracePeriodSeconds` + `preStop` for graceful shutdown.
- Example server `Deployment` updated with
  `terminationGracePeriodSeconds: 30` and a 5-second `preStop` sleep
  to demonstrate the recommended graceful-shutdown pattern.
- Example client updated with HTTP/2 keep-alive settings on the
  discovered `Endpoint`.

## [0.1.0] - 2026-01-13

### Added

- Initial release.
- `discover` function and `DiscoveryConfig` builder for watching a
  Kubernetes Service's `EndpointSlice` resources and feeding endpoint
  changes to a user-provided `tonic::transport::Channel::balance_channel`
  sender.
- `Port` enum supporting both numeric (`u16`) and named ports with
  `From` conversions.
- Optional namespace; defaults to the kube client's current namespace
  (in-cluster service-account namespace or kubeconfig context).
- TLS root certificate features (`tls-native-roots`, `tls-webpki-roots`)
  that coordinate kube and tonic configuration for system or embedded
  Mozilla roots respectively.
- Worked example under `examples/` with a Greeter server/client,
  Dockerfiles, kind cluster setup, Kubernetes manifests, and a
  deploy script.

[0.1.1]: https://github.com/ecliptical/tonic-lb-k8s/releases/tag/v0.1.1
[0.1.0]: https://github.com/ecliptical/tonic-lb-k8s/releases/tag/v0.1.0
