# How to deploy with Helm

This guide takes you from nothing to a running singleton deployment on Kubernetes using the
supported Helm chart. It assumes you already have a cluster, `kubectl` and `helm` configured
against it, and have worked through [Get a webhook flowing into metrics](../tutorials/getting-started.md)
or otherwise know what the exporter needs at runtime.

The chart installs exactly one StatefulSet replica with a `ReadWriteOnce` PVC for SQLite, a
ClusterIP Service, and fixed non-root UID/GID/`fsGroup` `65532`. It never creates the Secret it
depends on — you supply that.

## Create the Secret

```bash
kubectl create secret generic github-webhook-exporter \
  --from-literal=master-key="$(openssl rand -base64 32)" \
  --from-literal=admin-token="$(openssl rand -hex 32)"
```

If you also want OTLP export, add its header value as an additional key on the same Secret; see
[How to configure remote telemetry](configure-remote-telemetry.md).

## Install the chart

```bash
helm install github-webhook-exporter \
  oci://ghcr.io/petergrace/charts/github-webhook-exporter --version 0.1.6
```

An empty `image.tag` resolves to the chart's `appVersion`, which matches the published image for
that chart version — you don't need to set `image.repository` or `image.tag` unless you're
pointing at a separately published, compatible image.

Every other value — storage class and size, resource requests/limits, probe tuning, ingress,
network policy, the optional dedicated metrics Service and `ServiceMonitor`, and the optional
separate administration Service — is documented with defaults and constraints in the
[chart README](https://github.com/PeterGrace/github_webhook_exporter/blob/main/charts/github-webhook-exporter/README.md).
[Helm values](../reference/helm-values.md) is a map into that document, grouped by concern, so you
know where to look for a given setting.

Pass overrides the way you normally would, for example to size storage and set an ingress host:

```bash
helm install github-webhook-exporter \
  oci://ghcr.io/petergrace/charts/github-webhook-exporter --version 0.1.6 \
  --set persistence.size=5Gi \
  --set webhookIngress.enabled=true \
  --set webhookIngress.host=webhooks.example.com
```

## Verify the rollout

```bash
kubectl rollout status statefulset/github-webhook-exporter --timeout=180s
kubectl exec statefulset/github-webhook-exporter -- true
```

Then confirm both probes and register your first repository through the admin API, either by
port-forwarding or through whatever Service/Ingress you configured:

```bash
kubectl port-forward service/github-webhook-exporter 8080:8080
curl --fail --silent --show-error http://localhost:8080/health/ready
```

An empty `200` response means migrations completed and SQLite is reachable. From here, register
repositories the same way as in the [tutorial](../tutorials/getting-started.md), against this
Service instead of the local container.

## Next

- [How to upgrade a running deployment](upgrade-a-deployment.md) once a new image is published.
- [How to back up and restore SQLite](back-up-and-restore.md) before you rely on this in
  production.
- [Architecture](../explanation/architecture.md) explains why the chart deliberately deploys a
  fixed singleton rather than a Deployment with replicas.
