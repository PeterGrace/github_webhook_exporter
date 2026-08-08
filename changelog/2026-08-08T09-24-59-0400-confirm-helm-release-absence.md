# Confirm Helm release absence after Kind acceptance

Updated the Kind acceptance cleanup assertion to use a successful, namespace-scoped `helm list`
query through the script's isolated kubeconfig and context. The script now requires the exact
release-name query to return empty output after uninstall, so authentication, connectivity, and
Kubernetes API errors cannot be mistaken for release absence.
