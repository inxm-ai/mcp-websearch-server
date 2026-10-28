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

{{/*
A CPU quantity ("100m", "1", "0.5") in millicores.
*/}}
{{- define "mcp-websearch-server-chart.millicores" -}}
{{- $quantity := toString . -}}
{{- if hasSuffix "m" $quantity -}}
{{- trimSuffix "m" $quantity | int -}}
{{- else -}}
{{- mulf (float64 $quantity) 1000 | int -}}
{{- end -}}
{{- end -}}

{{/*
A memory quantity ("256Mi", "1Gi") in Mi.
*/}}
{{- define "mcp-websearch-server-chart.mebibytes" -}}
{{- $quantity := toString . -}}
{{- if hasSuffix "Gi" $quantity -}}
{{- mul (trimSuffix "Gi" $quantity | int) 1024 -}}
{{- else if hasSuffix "Mi" $quantity -}}
{{- trimSuffix "Mi" $quantity | int -}}
{{- else -}}
{{- fail (printf "memory quantity %s: write it in Mi or Gi" $quantity) -}}
{{- end -}}
{{- end -}}

{{/*
The server container's resources: `resources`, plus render.perBrowser for
every browser in the pool when render is enabled.
*/}}
{{- define "mcp-websearch-server-chart.resources" -}}
{{- $browsers := 0 -}}
{{- if .Values.render.enabled -}}
{{- $browsers = int .Values.render.poolSize -}}
{{- end -}}
{{- $perBrowser := .Values.render.perBrowser -}}
{{- range $bound := list "requests" "limits" }}
{{ $bound }}:
  cpu: {{ add (include "mcp-websearch-server-chart.millicores" (index $.Values.resources $bound "cpu")) (mul $browsers (index $perBrowser.cpuMillicores $bound)) }}m
  memory: {{ add (include "mcp-websearch-server-chart.mebibytes" (index $.Values.resources $bound "memory")) (mul $browsers (index $perBrowser.memoryMi $bound)) }}Mi
{{- end }}
{{- end -}}

{{/*
The name of SearXNG's objects. `ext-` like other MCP charts' dependencies
(ext-<app>-postgres): app-mcp-manager registers every Service in the
namespace whose name starts with `mcp-` as an MCP server, and SearXNG is not
one.
*/}}
{{- define "mcp-websearch-server-chart.searxngName" -}}
{{- printf "ext-%s-searxng" .Values.appName -}}
{{- end -}}

{{/*
Where the server reaches SearXNG.
*/}}
{{- define "mcp-websearch-server-chart.searxngUrl" -}}
{{- if .Values.searxng.url -}}
{{- .Values.searxng.url -}}
{{- else if .Values.searxng.enabled -}}
{{- printf "http://%s:%v" (include "mcp-websearch-server-chart.searxngName" .) .Values.searxng.port -}}
{{- else -}}
{{- fail "searxng.enabled is false: set searxng.url to the SearXNG the server should use" -}}
{{- end -}}
{{- end -}}
