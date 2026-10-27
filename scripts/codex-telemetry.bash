# Source this file from an interactive Bash shell after its global OTEL exports.
# A shared daemon retains the environment of its first launch, so it cannot
# inherit a different project's direnv attributes for each new terminal client.
codex() {
    local codex_attribute codex_attributes="" codex_instance
    local -a codex_parts=()
    IFS=, read -r -a codex_parts <<< "${OTEL_RESOURCE_ATTRIBUTES-}"
    for codex_attribute in "${codex_parts[@]}"; do
        if [[ -n "$codex_attribute" && "$codex_attribute" != service.instance.id=* ]]; then
            codex_attributes+="${codex_attributes:+,}$codex_attribute"
        fi
    done
    if ! IFS= read -r codex_instance < /proc/sys/kernel/random/uuid; then
        printf '%s\n' 'Cannot generate the Codex telemetry instance ID.' >&2
        return 1
    fi
    OTEL_RESOURCE_ATTRIBUTES="${codex_attributes:+$codex_attributes,}service.instance.id=$codex_instance" \
        command codex --no-daemon "$@"
}
