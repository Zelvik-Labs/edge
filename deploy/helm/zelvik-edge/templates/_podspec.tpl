{{- define "zelvik-edge.podSpec" -}}
{{- with .Values.imagePullSecrets }}
imagePullSecrets:
  {{- toYaml . | nindent 2 }}
{{- end }}
serviceAccountName: {{ include "zelvik-edge.fullname" . }}-sa
securityContext:
  {{- toYaml .Values.podSecurityContext | nindent 2 }}
terminationGracePeriodSeconds: 60
containers:
  - name: edge
    image: "{{ .Values.image.repository }}:{{ .Values.image.tag }}"
    imagePullPolicy: {{ .Values.image.pullPolicy }}
    ports:
      - name: http
        containerPort: 80
        protocol: TCP
      - name: https
        containerPort: 443
        protocol: TCP
      - name: quic
        containerPort: 443
        protocol: UDP
      - name: internal
        containerPort: 19090
        protocol: TCP
    env:
      - name: CLOUD_NODE_MODE
        value: {{ .Values.runtime.mode }}
      - name: CLOUD_NODE_HOME
        value: /etc/zelvik/edge
      - name: POD_NAME
        valueFrom:
          fieldRef:
            fieldPath: metadata.name
      - name: POD_NAMESPACE
        valueFrom:
          fieldRef:
            fieldPath: metadata.namespace
      - name: POD_IP
        valueFrom:
          fieldRef:
            fieldPath: status.podIP
      - name: K8S_NODE_NAME
        valueFrom:
          fieldRef:
            fieldPath: spec.nodeName
      - name: CLOUD_NODE_CLUSTER_INTERNAL_TOKEN
        valueFrom:
          secretKeyRef:
            name: {{ include "zelvik-edge.fullname" . }}-secret
            key: internal-token
      - name: TZ
        value: "Asia/Shanghai"
    startupProbe:
      httpGet:
        path: {{ .Values.probes.startup.path }}
        port: {{ .Values.probes.startup.port }}
      periodSeconds: {{ .Values.probes.startup.periodSeconds }}
      failureThreshold: {{ .Values.probes.startup.failureThreshold }}
    readinessProbe:
      httpGet:
        path: {{ .Values.probes.readiness.path }}
        port: {{ .Values.probes.readiness.port }}
      periodSeconds: {{ .Values.probes.readiness.periodSeconds }}
      failureThreshold: {{ .Values.probes.readiness.failureThreshold }}
    livenessProbe:
      httpGet:
        path: {{ .Values.probes.liveness.path }}
        port: {{ .Values.probes.liveness.port }}
      periodSeconds: {{ .Values.probes.liveness.periodSeconds }}
      failureThreshold: {{ .Values.probes.liveness.failureThreshold }}
    securityContext:
      {{- toYaml .Values.securityContext | nindent 6 }}
    resources:
      {{- toYaml .Values.resources | nindent 6 }}
    volumeMounts:
      - name: runtime-config
        mountPath: /etc/zelvik/edge/runtime.yaml
        subPath: runtime.yaml
        readOnly: true
      - name: local-data
        mountPath: /etc/zelvik/edge
      - name: local-cache-meta
        mountPath: /var/lib/zelvik/edge/cache-meta
      {{- range .Values.storage.cache.shards }}
      - name: cache-{{ .id }}
        mountPath: {{ .path }}
      {{- end }}
volumes:
  - name: runtime-config
    configMap:
      name: {{ include "zelvik-edge.fullname" . }}-runtime
      items:
        - key: runtime.yaml
          path: runtime.yaml
  - name: local-data
    emptyDir: {}
  - name: local-cache-meta
    emptyDir: {}
  {{- range .Values.storage.cache.shards }}
  - name: cache-{{ .id }}
    persistentVolumeClaim:
      claimName: {{ include "zelvik-edge.fullname" $ }}-cache-{{ .id }}
  {{- end }}
{{- with .Values.nodeSelector }}
nodeSelector:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- with .Values.affinity }}
affinity:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- with .Values.tolerations }}
tolerations:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- end }}
