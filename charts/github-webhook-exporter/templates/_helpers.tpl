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
{{- $otlp := .Values.networkPolicy.egress.otlp -}}
{{- if and $otlp.enabled (or (empty $otlp.peers) (empty $otlp.ports)) -}}
{{- fail "networkPolicy.egress.otlp requires at least one peer and port" -}}
{{- end -}}
{{- if $otlp.enabled -}}
{{- range $index, $peer := $otlp.peers -}}
{{- if and (not (hasKey $peer "ipBlock"))
    (or (empty $peer.namespaceSelector) (empty $peer.podSelector)) -}}
{{- fail (printf
    "networkPolicy.egress.otlp.peers[%d] requires non-empty namespaceSelector and podSelector"
    $index) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- end -}}
