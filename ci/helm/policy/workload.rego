package main

statefulset if {
    input.kind == "StatefulSet"
}

ordinary_workload_containers := [container | container := input.spec.template.spec.containers[_]]

init_workload_containers := [container | container := input.spec.template.spec.initContainers[_]]

workload_containers := array.concat(ordinary_workload_containers, init_workload_containers)

replicas_are_one if {
    input.spec.replicas == 1
}

pod_runs_as_non_root if {
    input.spec.template.spec.securityContext.runAsNonRoot == true
}

container_runs_as_non_root(container) if {
    container.securityContext.runAsNonRoot == true
}

run_as_non_root_violation if {
    not pod_runs_as_non_root
}

run_as_non_root_violation if {
    some container in workload_containers
    not container_runs_as_non_root(container)
}

container_privileged(container) if {
    container.securityContext.privileged == true
}

container_disallows_privilege_escalation(container) if {
    container.securityContext.allowPrivilegeEscalation == false
}

host_namespace_violation if {
    input.spec.template.spec.hostNetwork == true
}

host_namespace_violation if {
    input.spec.template.spec.hostPID == true
}

host_namespace_violation if {
    input.spec.template.spec.hostIPC == true
}

host_path_violation if {
    some volume in input.spec.template.spec.volumes
    volume.hostPath
}

container_has_read_only_root_filesystem(container) if {
    container.securityContext.readOnlyRootFilesystem == true
}

container_has_capability_additions(container) if {
    some _ in container.securityContext.capabilities.add
}

container_drops_all_capabilities(container) if {
    some capability in container.securityContext.capabilities.drop
    capability == "ALL"
}

container_has_cpu_request(container) if {
    cpu := container.resources.requests.cpu
    cpu != ""
}

container_has_memory_request(container) if {
    memory := container.resources.requests.memory
    memory != ""
}

container_has_cpu_limit(container) if {
    cpu := container.resources.limits.cpu
    cpu != ""
}

container_has_memory_limit(container) if {
    memory := container.resources.limits.memory
    memory != ""
}

container_has_bounded_resources(container) if {
    container_has_cpu_request(container)
    container_has_memory_request(container)
    container_has_cpu_limit(container)
    container_has_memory_limit(container)
}

service_account_token_automount_disabled if {
    input.spec.template.spec.automountServiceAccountToken == false
}

service_account_override_present if {
    service_account_name := input.spec.template.spec.serviceAccountName
    service_account_name != ""
}

service_account_override_present if {
    service_account := input.spec.template.spec.serviceAccount
    service_account != ""
}

deny contains msg if {
    statefulset
    not replicas_are_one
    msg := "GWE001: StatefulSet replicas must be exactly 1"
}

deny contains msg if {
    statefulset
    run_as_non_root_violation
    msg := "GWE002: pods and containers must run as non-root"
}

deny contains msg if {
    statefulset
    some container in workload_containers
    container_privileged(container)
    msg := "GWE003: containers must not be privileged"
}

deny contains msg if {
    statefulset
    some container in workload_containers
    not container_disallows_privilege_escalation(container)
    msg := "GWE004: containers must disable privilege escalation"
}

deny contains msg if {
    statefulset
    host_namespace_violation
    msg := "GWE005: pods must not use host network, PID, or IPC"
}

deny contains msg if {
    statefulset
    host_path_violation
    msg := "GWE006: volumes must not use hostPath"
}

deny contains msg if {
    statefulset
    some container in workload_containers
    not container_has_read_only_root_filesystem(container)
    msg := "GWE007: containers must use a read-only root filesystem"
}

deny contains msg if {
    statefulset
    some container in workload_containers
    container_has_capability_additions(container)
    msg := "GWE008: containers must not add Linux capabilities"
}

deny contains msg if {
    statefulset
    some container in workload_containers
    not container_drops_all_capabilities(container)
    msg := "GWE009: containers must drop ALL Linux capabilities"
}

deny contains msg if {
    statefulset
    some container in workload_containers
    not container_has_bounded_resources(container)
    msg := "GWE010: containers must set CPU and memory requests and limits"
}

deny contains msg if {
    statefulset
    not service_account_token_automount_disabled
    msg := "GWE011: pods must disable service-account token automounting"
}

deny contains msg if {
    statefulset
    service_account_override_present
    msg := "GWE012: pods must not set serviceAccountName or serviceAccount"
}
