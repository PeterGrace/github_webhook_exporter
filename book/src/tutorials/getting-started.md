# Get a webhook flowing into metrics

In this tutorial we will run GitHub Webhook Exporter in Docker, register one repository, deliver
it a real signed webhook by hand, and watch that delivery turn into a Prometheus metric. By the
end you will have exercised the exporter's entire request path — authentication, deduplication,
and metrics — without needing a real GitHub repository or a public URL.

You need `docker`, `curl`, `openssl`, and `uuidgen` on your machine. All four are common on Linux
and macOS.

## Generate credentials

The exporter needs a master key, which encrypts repository webhook secrets at rest, and an admin
token, which authorizes the repository-management API. We'll generate both now and keep them in
shell variables so we can reuse them in later commands:

```bash
export GHE_MASTER_KEY="$(openssl rand -base64 32)"
export GHE_ADMIN_TOKEN="$(openssl rand -hex 32)"
```

Neither command prints anything by design — `export` is silent on success. Check that both
variables are set:

```bash
echo "master key length: ${#GHE_MASTER_KEY}, admin token length: ${#GHE_ADMIN_TOKEN}"
```

You should see `master key length: 44, admin token length: 64`.

## Start the exporter

Run the published image, mounting a named volume for its SQLite database and publishing port
8080:

```bash
docker run --rm -d --name github-webhook-exporter -p 8080:8080 \
  -v github-webhook-exporter:/var/lib/github-webhook-exporter \
  -e GHE_DATABASE_PATH=/var/lib/github-webhook-exporter/github-webhook-exporter.db \
  -e GHE_MASTER_KEY="${GHE_MASTER_KEY}" \
  -e GHE_ADMIN_TOKEN="${GHE_ADMIN_TOKEN}" \
  ghcr.io/petergrace/github-webhook-exporter:0.1.10
```

Docker prints a long container ID and returns you to the prompt — the container is now running in
the background. Give it a moment to open its database and bind its listener, then confirm it is
alive:

```bash
curl --fail --silent --show-error http://localhost:8080/health/live
```

This should return nothing and exit without an error — an empty body on `/health/live` means the
process is up. If `curl` reports connection refused, wait a couple more seconds and try again.

## Register a repository

Every repository the exporter accepts webhooks for must be registered through the admin API with
its own secret. We'll invent a webhook secret and register a repository named
`octocat/hello-world`:

```bash
export WEBHOOK_SECRET="$(openssl rand -hex 20)"

curl --fail --silent --show-error -X POST http://localhost:8080/api/v1/repositories \
  -H "Authorization: Bearer ${GHE_ADMIN_TOKEN}" \
  -H 'Content-Type: application/json' \
  -d "{\"full_name\":\"octocat/hello-world\",\"webhook_secret\":\"${WEBHOOK_SECRET}\"}"
```

You should see a JSON object echoing back the repository's `id`, `full_name`, and `enabled`
status — notice that `webhook_secret` is not in the response. The exporter never returns secrets
once they're stored, even to the administrator who set them.

## Deliver a signed webhook

GitHub signs every webhook body with HMAC-SHA256 over the exact request bytes, using the secret
you set on the repository. We'll reproduce that by hand: write a minimal payload to a file, sign
it with the secret we just registered, then send it with the matching headers.

```bash
cat > /tmp/webhook-payload.json <<'EOF'
{"repository":{"full_name":"octocat/hello-world"}}
EOF

export SIGNATURE="$(openssl dgst -sha256 -hmac "${WEBHOOK_SECRET}" /tmp/webhook-payload.json | awk '{print $NF}')"

curl --include --silent --show-error -X POST http://localhost:8080/webhooks/github \
  -H 'Content-Type: application/json' \
  -H 'X-GitHub-Event: push' \
  -H "X-GitHub-Delivery: $(uuidgen)" \
  -H "X-Hub-Signature-256: sha256=${SIGNATURE}" \
  --data-binary @/tmp/webhook-payload.json
```

The response starts with `HTTP/1.1 204 No Content` and has an empty body. That status line is the
exporter telling you the signature checked out and the delivery was accepted — it's the same
response GitHub itself would see. Nothing about the payload is stored; only the fact that this
delivery happened is retained, for deduplication.

Run the exact same `curl` command a second time. You'll get `204 No Content` again — the exporter
recognized the delivery ID as a duplicate and accepted it without processing it twice.

## Watch it become a metric

Scrape the metrics endpoint and look for the webhook families:

```bash
curl --silent http://localhost:8080/metrics | grep github_webhook
```

Notice `github_webhook_requests_total` and `github_webhook_events_total` both carry
`repository="octocat/hello-world"` and are greater than zero, and that
`github_webhook_duplicates_total` is `1` — counting the second, duplicate delivery you just sent.
None of these series contain the delivery ID, the payload, or the signature; only the repository
name and closed sets of result and event-type values appear as labels.

## What you just did

You ran the exporter, registered a repository with its own secret, authenticated and delivered a
real signed webhook, confirmed deduplication, and watched all of it surface as bounded-cardinality
Prometheus metrics — with zero webhook payloads persisted anywhere.

Clean up when you're done:

```bash
docker stop github-webhook-exporter
docker volume rm github-webhook-exporter
```

From here:

- [Deploy with Helm](../how-to/deploy-with-helm.md) to run this on Kubernetes instead of a laptop.
- The [HTTP API reference](../reference/http-api.md) and [metrics reference](../reference/metrics.md)
  cover every endpoint and series you touched above, in full.
- [Design decisions](../explanation/design-decisions.md) explains why payloads are never persisted
  and why metric labels are kept to closed, bounded vocabularies.
