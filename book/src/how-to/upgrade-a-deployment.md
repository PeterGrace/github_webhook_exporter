# How to upgrade a running deployment

The exporter is a singleton, so upgrading means replacing the one running pod. Which procedure to
use depends on whether your storage provider completes volume handoff before a replacement pod can
mount the PVC. If you're unsure, use the stopped procedure — it costs a short downtime window but
cannot race the old pod for the volume.

Both procedures need an immutable, already-published image tag:

```bash
export GHE_IMAGE_TAG=0.1.3   # the version you're upgrading to
```

## Rolling upgrade (fast-handoff storage only)

Use this only when you've confirmed your CSI driver detaches the old pod's volume before the new
pod tries to attach it. A normal one-replica `RollingUpdate` terminates ordinal `0` before creating
its replacement:

```bash
helm upgrade github-webhook-exporter charts/github-webhook-exporter \
  --namespace github-webhook-exporter \
  --reuse-values \
  --set-string image.tag="${GHE_IMAGE_TAG}" \
  --wait
kubectl --namespace github-webhook-exporter rollout status \
  statefulset/github-webhook-exporter --timeout=180s
```

## Stopped upgrade (safe for any storage provider)

This intentionally causes downtime, in exchange for guaranteeing there is never a window where two
pods could attach the same PVC.

Scale to zero and wait for the pod to fully disappear:

```bash
kubectl --namespace github-webhook-exporter scale \
  statefulset/github-webhook-exporter --replicas=0
kubectl --namespace github-webhook-exporter wait --for=delete \
  pod/github-webhook-exporter-0 --timeout=180s
```

If your provider detaches volumes asynchronously, also confirm detachment in its console or CLI
before continuing — the wait above only proves the pod object is gone, not that the volume is
free.

Enter maintenance mode with the new image, then leave it:

```bash
helm upgrade github-webhook-exporter charts/github-webhook-exporter \
  --namespace github-webhook-exporter \
  --reuse-values \
  --set maintenanceMode=true \
  --set-string image.tag="${GHE_IMAGE_TAG}" \
  --wait

helm upgrade github-webhook-exporter charts/github-webhook-exporter \
  --namespace github-webhook-exporter \
  --reuse-values \
  --set maintenanceMode=false \
  --wait
kubectl --namespace github-webhook-exporter wait --for=condition=Ready \
  pod/github-webhook-exporter-0 --timeout=180s
```

`maintenanceMode=true` records the release with zero desired replicas so the second `helm upgrade`
doesn't fight the first; setting it back to `false` restores the one replica with the new image
already applied.

## If you have a PodDisruptionBudget

The chart's optional PDB sets `minAvailable: 0` deliberately — it permits voluntary disruption
rather than blocking it, because a singleton has no other replica to preserve availability with.
It won't get in the way of either procedure above.

## Verify

```bash
kubectl --namespace github-webhook-exporter get pods -l app.kubernetes.io/name=github-webhook-exporter
curl --fail --silent --show-error http://localhost:8080/health/ready   # after port-forwarding
```

If readiness doesn't come back within your probe's grace period, see
[Startup, retention, and shutdown](../reference/lifecycle.md) for what a failed readiness probe
means and what it doesn't (it never terminates the process on its own).
