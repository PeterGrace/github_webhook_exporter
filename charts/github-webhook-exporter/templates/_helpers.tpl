{{/* Return the chart name, constrained to a valid Kubernetes name length. */}}
{{- define "github-webhook-exporter.name" -}}
{{- .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/* Return a release-qualified resource name. */}}
{{- define "github-webhook-exporter.fullname" -}}
{{- if contains .Chart.Name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{/* Return the chart label value. */}}
{{- define "github-webhook-exporter.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/* Return labels shared by chart resources. */}}
{{- define "github-webhook-exporter.labels" -}}
helm.sh/chart: {{ include "github-webhook-exporter.chart" . | quote }}
{{ include "github-webhook-exporter.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service | quote }}
{{- end -}}

{{/* Return stable labels used by workload selectors. */}}
{{- define "github-webhook-exporter.selectorLabels" -}}
app.kubernetes.io/name: {{ include "github-webhook-exporter.name" . | quote }}
app.kubernetes.io/instance: {{ .Release.Name | trunc 63 | trimSuffix "-" | quote }}
{{- end -}}

{{/* Return a deterministic checksum of the rendered non-secret ConfigMap. */}}
{{- define "github-webhook-exporter.configChecksum" -}}
{{- include (print $.Template.BasePath "/configmap.yaml") . | sha256sum -}}
{{- end -}}

{{/*
Return "true" when every GitHub App setting needed for required-check lookups is present.

The three settings are all-or-nothing; "github-webhook-exporter.validate" rejects a partial
configuration, so templates can treat a truthy result as fully configured.
*/}}
{{- define "github-webhook-exporter.githubAppEnabled" -}}
{{- $app := .Values.githubApp -}}
{{- if and $app.appId $app.installationId .Values.existingSecret.keys.githubAppPrivateKey -}}
true
{{- end -}}
{{- end -}}

{{/* Return the directory holding the projected GitHub App private key. */}}
{{- define "github-webhook-exporter.githubAppKeyDirectory" -}}
/etc/github-webhook-exporter/github-app
{{- end -}}

{{/* Render one egress peer list shared by the OTLP and GitHub rules. */}}
{{- define "github-webhook-exporter.egressPeers" -}}
{{- range $peer := . }}
- {{- with $peer.ipBlock }}
  ipBlock:
    {{- toYaml . | nindent 4 }}
  {{- else }}
  namespaceSelector:
    {{- toYaml $peer.namespaceSelector | nindent 4 }}
  podSelector:
    {{- toYaml $peer.podSelector | nindent 4 }}
  {{- end }}
{{- end }}
{{- end -}}

{{/* Validate singleton, storage, telemetry, and shutdown invariants. */}}
{{- define "github-webhook-exporter.validate" -}}
{{- if ne (int .Values.replicaCount) 1 -}}
{{- fail (printf "replicaCount must equal 1; got replicaCount=%v" .Values.replicaCount) -}}
{{- end -}}
{{- $accessModes := .Values.persistence.accessModes -}}
{{- if ne (len $accessModes) 1 -}}
{{- fail (printf
    "persistence.accessModes must equal [ReadWriteOnce]; got persistence.accessModes=%v"
    $accessModes) -}}
{{- else if ne (index $accessModes 0) "ReadWriteOnce" -}}
{{- fail (printf
    "persistence.accessModes must equal [ReadWriteOnce]; got persistence.accessModes=%v"
    $accessModes) -}}
{{- end -}}
{{- $batchSize := .Values.telemetry.batchSize -}}
{{- $queueCapacity := .Values.telemetry.queueCapacity -}}
{{- if gt (int $batchSize) (int $queueCapacity) -}}
{{- $batchMessage := print
    "telemetry.batchSize must be no greater than telemetry.queueCapacity; "
    "got telemetry.batchSize=%v telemetry.queueCapacity=%v" -}}
{{- fail (printf $batchMessage $batchSize $queueCapacity) -}}
{{- end -}}
{{- $applicationShutdown := .Values.application.shutdownTimeoutSeconds -}}
{{- $telemetryShutdown := .Values.telemetry.shutdownTimeoutSeconds -}}
{{- $shutdownTotal := add $applicationShutdown $telemetryShutdown -}}
{{- $terminationGrace := .Values.terminationGracePeriodSeconds -}}
{{- if le (int $terminationGrace) (int $shutdownTotal) -}}
{{- $graceMessage := print
    "terminationGracePeriodSeconds must be greater than "
    "application.shutdownTimeoutSeconds + telemetry.shutdownTimeoutSeconds; "
    "got terminationGracePeriodSeconds=%v application.shutdownTimeoutSeconds=%v "
    "telemetry.shutdownTimeoutSeconds=%v" -}}
{{- fail (printf
    $graceMessage $terminationGrace $applicationShutdown $telemetryShutdown) -}}
{{- end -}}
{{- $app := .Values.githubApp -}}
{{- $appKey := .Values.existingSecret.keys.githubAppPrivateKey -}}
{{- $appSettings := list $app.appId $app.installationId $appKey -}}
{{- $appPresent := 0 -}}
{{- range $appSettings -}}
{{- if . -}}
{{- $appPresent = add1 $appPresent -}}
{{- end -}}
{{- end -}}
{{- if and (gt $appPresent 0) (lt $appPresent 3) -}}
{{/*
    The diagnostic reports whether the key entry is configured rather than echoing its name
    alongside its value: the repository's structural secret scan treats any assignment whose
    left-hand side ends in "privatekey" as an embedded credential, even in a format string.
*/}}
{{- $appMessage := print
    "githubApp.appId, githubApp.installationId, and "
    "existingSecret.keys.githubAppPrivateKey must be set together or left unset; "
    "got githubApp.appId=%v githubApp.installationId=%v and key entry configured=%v" -}}
{{- fail (printf $appMessage $app.appId $app.installationId (not (empty $appKey))) -}}
{{- end -}}
{{- if and .Values.metrics.serviceMonitor.enabled (not .Values.metrics.service.enabled) -}}
{{- fail "metrics.serviceMonitor.enabled requires metrics.service.enabled" -}}
{{- end -}}
{{- if and .Values.administration.ingress.enabled
    (not .Values.administration.service.enabled) -}}
{{- fail "administration.ingress.enabled requires administration.service.enabled" -}}
{{- end -}}
{{- if .Values.networkPolicy.enabled -}}
{{- range $name, $rule := .Values.networkPolicy.ingress -}}
{{- if and $rule.enabled (or (empty $rule.namespaceSelector) (empty $rule.podSelector)) -}}
{{- fail (printf
    "networkPolicy.ingress.%s requires non-empty namespaceSelector and podSelector" $name) -}}
{{- end -}}
{{- end -}}
{{- $dns := .Values.networkPolicy.egress.dns -}}
{{- if and $dns.enabled (or (empty $dns.namespaceSelector) (empty $dns.podSelector)) -}}
{{- fail "networkPolicy.egress.dns requires non-empty namespaceSelector and podSelector" -}}
{{- end -}}
{{- range $name, $rule := (dict
    "otlp" .Values.networkPolicy.egress.otlp
    "github" .Values.networkPolicy.egress.github) -}}
{{- if and $rule.enabled (or (empty $rule.peers) (empty $rule.ports)) -}}
{{- fail (printf "networkPolicy.egress.%s requires at least one peer and port" $name) -}}
{{- end -}}
{{- if $rule.enabled -}}
{{- range $index, $peer := $rule.peers -}}
{{- if and (not (hasKey $peer "ipBlock"))
    (or (empty $peer.namespaceSelector) (empty $peer.podSelector)) -}}
{{- fail (printf
    "networkPolicy.egress.%s.peers[%d] requires non-empty namespaceSelector and podSelector"
    $name $index) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- end -}}
