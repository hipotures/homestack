"""Write the first rust/parity/manifest.toml (phase 0 of the migration).

    python -m homestack_parity.seed_manifest tests > rust/parity/manifest.toml

Each Python test gets the phase that ports it, from the rules below: the most
specific rule wins (test, then class, then file). A test touching several
operations goes to the latest phase it needs. Later phases edit the manifest
directly; this script is kept to show where the first assignment came from.
"""

from __future__ import annotations

import sys
from pathlib import Path

from .list_tests import manifest_ids

HERDR = "Herdr transport: replaced by the Proxmox API client (mapping T1)"
ROUTING = "ssh routing between nodes: the API proxies /nodes/{node} itself (mapping T2)"
TUI = "TUI: out of scope of the migration (mapping, introduction)"

# Default phase per test file.
FILES = {
    "test_addkey": "9",
    "test_alien_workspace": "2",
    "test_bk": "14d",
    "test_cli": "13",
    "test_config": "1",
    "test_create_execution": "5",
    "test_create_transfer": "10",
    "test_discovery": "2",
    "test_fido": "13",
    "test_gold": "2",
    "test_install": "13",
    "test_lifecycle": "5",
    "test_proxmox": "2",
    "test_refresh_lock": "6",
    "test_repo": "14f",
    "test_resize": "7",
    "test_resize_auth": "7",
    "test_resize_cli": "13",
    "test_resize_inspection": "7",
    "test_resize_layers": "7",
    "test_resize_luks": "7",
    "test_setup": "14a",
    "test_setup_app_files": "14c",
    "test_setup_backup": "14d",
    "test_setup_backup_retrieval": "14d",
    "test_setup_documents": "14c",
    "test_setup_files": "14a",
    "test_setup_herdr": "14e",
    "test_setup_repositories": "14f",
    "test_setup_startup": "14a",
    "test_setup_state": "14a",
    "test_setup_structured": "14c",
    "test_setup_structured_guest": "14c",
    "test_status": "2",
    "test_touch": "13",
    "test_workspace_ssh": "13",
}

# Phase per test class: "file::Class".
CLASSES = {
    "test_alien_workspace::GuestIdentityTests": "3",
    "test_alien_workspace::SetupRolePropagationTests": "14a",
    "test_gold::GoldReadinessTests": "2",
    "test_lifecycle::MigrationPowerStateTests": "8",
    "test_lifecycle::MigrationProgressTests": "8",
    "test_lifecycle::ProgressTests": "3",
    "test_lifecycle::RefreshPowerStateTests": "6",
    "test_lifecycle::WorkspaceTargetTests": "2",
    "test_proxmox::QgaWaitTests": "3",
    "test_proxmox::SnippetMigrationTests": "8",
    "test_status::FreePercentDisplayTests": "13",
    "test_status::GlobalStorageTableTests": "13",
    "test_status::MigrationPlanDisplayTests": "13",
    "test_status::StorageDisplayFormatTests": "13",
    "test_status::VolumeRenameCleanupTests": "5",
    "test_workspace_ssh::GoldKeyRoutingTests": "4",
}

# Phase per test: "file::Class::test".
TESTS = {
    "test_alien_workspace::LifecycleAlienGuardsTests::test_destroy_execution_rejects_alien_before_shutdown": "5",
    "test_alien_workspace::LifecycleAlienGuardsTests::test_migrate_execution_rejects_alien_before_snippets_or_shutdown": "8",
    "test_alien_workspace::LifecycleAlienGuardsTests::test_refresh_plan_rejects_alien_before_reading_recovery_journal": "6",
    "test_alien_workspace::LifecycleAlienGuardsTests::test_refresh_recovery_rejects_alien_before_recovery": "6",
    "test_alien_workspace::LifecycleAlienGuardsTests::test_resize_destroy_and_migrate_plans_reject_alien_before_guest_or_disk_work": "8",
    "test_alien_workspace::RoleTagTests::test_setup_accepts_standalone_alien_but_native_lifecycle_does_not": "14a",
    "test_discovery::EnvironmentDiscoveryTests::test_hardware_identity_discovery_accepts_suffix_after_sk": "13",
    "test_gold::GoldReadinessTests::test_running_gold_checks_guest_contract": "3",
    "test_lifecycle::CreateWorkspaceCompletionTests::test_create_result_uses_local_ssh_alias": "13",
    "test_lifecycle::CreateWorkspaceCompletionTests::test_local_ssh_config_failure_reports_verified_vm_without_rollback": "13",
    "test_lifecycle::DestroyPlanPresentationTests::test_destroy_does_not_remove_backup_keys_when_vm_still_exists": "13",
    "test_lifecycle::DestroyPlanPresentationTests::test_destroy_removes_local_ssh_artifacts_only_after_vm_is_absent": "13",
    "test_lifecycle::DestroyPlanPresentationTests::test_destroy_reports_local_cleanup_failure_after_verified_deletion": "13",
    "test_lifecycle::DestroyPlanPresentationTests::test_destroy_result_shows_local_ssh_config_and_known_hosts_cleanup": "13",
    "test_lifecycle::WorkspaceTargetTests::test_create_name_cannot_start_with_digit": "5",
    "test_proxmox::HomeDiskTests::test_boot_order_contains_root_disk": "5",
    "test_proxmox::HomeDiskTests::test_create_snippet_formats_only_blank_new_home": "4",
    "test_proxmox::HomeDiskTests::test_disk_option": "5",
    "test_proxmox::HomeDiskTests::test_home_label_and_size": "5",
    "test_proxmox::HomeDiskTests::test_logical_volume_names": "5",
    "test_proxmox::HomeDiskTests::test_named_raw_volume_allocation_uses_requested_name": "5",
    "test_proxmox::HomeDiskTests::test_pve_rename_volume_uses_proxmox_storage_layer": "5",
    "test_proxmox::HomeDiskTests::test_refresh_snippet_refuses_blank_home": "4",
    "test_proxmox::HomeDiskTests::test_replace_disk_volume_preserves_options": "6",
    "test_resize_auth::ResizeAuthenticationTests::test_noninteractive_password_refuses_before_prompt_or_connection": "13",
    "test_resize_auth::ResizeAuthenticationTests::test_password_is_only_sent_on_stdin_after_identity_check": "13",
    "test_resize_auth::ResizeAuthenticationTests::test_wrong_password_aborts_before_resize": "13",
    "test_resize_auth::ResizeAuthenticationTests::test_wrong_ssh_vm_does_not_receive_password": "13",
}

# Excluded tests with the reason: "file", "file::Class" or "file::Class::test".
EXCLUDED = {
    "test_discovery::HerdrCandidateDiscoveryTests": HERDR,
    "test_install::InstallerTransportDiscoveryTests": HERDR,
    "test_install::InstallerProgressTests::test_verified_transport_progress_keeps_session_open_until_context_exit": HERDR,
    "test_proxmox::ControlNodeTests::test_node_bound_storage_command_is_routed_when_control_node_differs": ROUTING,
    "test_proxmox::ControlNodeTests::test_node_shell_command_runs_directly_on_control_node": ROUTING,
    "test_setup_structured_ui": TUI,
    "test_setup_tui": TUI,
    "test_transport::TransportTests::test_node_shell_command": ROUTING,
    "test_transport::TransportTests::test_remote_envelope_parser_tolerates_indented_end_marker": HERDR,
    "test_transport::TransportTests::test_remote_envelope_parser_uses_latest_begin_marker": HERDR,
    "test_transport::TransportTests::test_remote_exit_is_isolated_and_still_emits_completion_marker": HERDR,
    "test_transport::TransportTests::test_remote_timeout_is_applied_to_the_child_command": HERDR,
    "test_transport::TransportTests::test_remote_wrapper_does_not_echo_completion_marker": HERDR,
    "test_transport::TransportTests::test_transport_protocol_accepts_required_operations": HERDR,
}


def lookup(table: dict[str, str], file: str, cls: str, test: str) -> str | None:
    for key in (f"{file}::{cls}::{test}", f"{file}::{cls}", file):
        if key in table:
            return table[key]
    return None


def toml_string(value: str) -> str:
    return '"' + value.replace("\\", "\\\\").replace('"', '\\"') + '"'


def entry(test_id: str) -> str:
    path, cls, test = test_id.split("::")
    file = Path(path).stem
    reason = lookup(EXCLUDED, file, cls, test)
    if reason is not None:
        return f"  {{ python = {toml_string(test_id)}, phase = \"0\", status = \"excluded\", reason = {toml_string(reason)} }},"
    phase = TESTS.get(f"{file}::{cls}::{test}") or CLASSES.get(f"{file}::{cls}") or FILES.get(file)
    if phase is None:
        raise SystemExit(f"no phase rule for {test_id}")
    return f"  {{ python = {toml_string(test_id)}, phase = {toml_string(phase)}, status = \"todo\" }},"


def main() -> None:
    tests_dir = Path(sys.argv[1] if len(sys.argv) > 1 else "tests")
    ids = manifest_ids(tests_dir)
    print("# Every Python test and its Rust replacement. Format and rules: rust/parity/README.md.")
    print("# Seeded by rust/parity/python/homestack_parity/seed_manifest.py in phase 0.")
    print()
    print("tests = [")
    for test_id in ids:
        print(entry(test_id))
    print("]")


if __name__ == "__main__":
    main()
