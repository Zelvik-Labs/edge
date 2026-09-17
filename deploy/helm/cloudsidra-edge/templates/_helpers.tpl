{{- define "cloudsidra-edge.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "cloudsidra-edge.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name .Chart.Name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}

{{- define "cloudsidra-edge.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "cloudsidra-edge.labels" -}}
helm.sh/chart: {{ include "cloudsidra-edge.chart" . }}
app.kubernetes.io/name: {{ include "cloudsidra-edge.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/component: edge
{{- end }}

{{- define "cloudsidra-edge.selectorLabels" -}}
app.kubernetes.io/name: {{ include "cloudsidra-edge.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/component: edge
{{- end }}
