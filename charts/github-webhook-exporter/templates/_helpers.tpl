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
{{- end -}}
