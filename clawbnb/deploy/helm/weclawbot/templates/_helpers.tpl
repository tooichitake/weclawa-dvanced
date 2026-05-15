{{/*
Standard Helm template helpers.
*/}}

{{- define "weclawbot.fullname" -}}
{{- printf "%s" .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "weclawbot.labels" -}}
app.kubernetes.io/name: weclawbot
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end -}}

{{- define "weclawbot.selectorLabels" -}}
app.kubernetes.io/name: weclawbot
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}
