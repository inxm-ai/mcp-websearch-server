{{/*
Expand the name of the chart
*/}}
{{- define "mcp-websearch-server-chart.name" -}}
{{- .Chart.Name | quote -}}
{{- end -}}

{{/*
Expand the full name of the chart
*/}}
{{- define "mcp-websearch-server-chart.fullname" -}}
{{- printf "%s-%s" .Release.Name .Chart.Name | quote -}}
{{- end -}}

{{/*
Return the image name
*/}}
{{- define "mcp-websearch-server-chart.image" -}}
{{- printf "%s:%s" .Values.image.repository .Values.image.tag | quote -}}
{{- end -}}

{{/*
Return the service name
*/}}
{{- define "mcp-websearch-server-chart.service.name" -}}
{{- .Chart.Name }}-service
{{- end -}}

{{/*
Canonical NGINX Gateway Fabric authentication contract. The ExternalAuth
filter remains attached to each generated HTTPRoute because NGF evaluates it
at route level; keeping the definition here prevents generated charts from
drifting apart.
*/}}
{{- define "mcp-websearch-server-chart.externalAuthFilter" -}}
- type: ExternalAuth
  externalAuth:
    protocol: HTTP
    backendRef:
      name: iam-oauth2-proxy
      port: 4180
    http:
      path: /oauth2/auth
      allowedHeaders:
        - Authorization
        - Cookie
        - X-Forwarded-For
        - X-Forwarded-Host
        - X-Forwarded-Proto
        - X-Auth-Request-Redirect
      allowedResponseHeaders:
        - X-Auth-Request-User
        - X-Auth-Request-Email
        - X-Auth-Request-Preferred-Username
        - X-Auth-Request-Groups
        - X-Auth-Request-Access-Token
        - X-Forwarded-Access-Token
{{- end -}}
