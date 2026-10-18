# Remaining parity TODO audit (2026-10-18)

The 32 remaining entries were reviewed against the current Rust flow and the
original Python assertions. Ported entries name behavioral tests; excluded
entries identify an intentionally removed mechanism or an owner-approved
behavior change. No obsolete Python draft or transfer path is reintroduced.

The audit found and fixed preservation of storage/identity selection order,
explicitly empty saved selections, pre-install local discovery, and disk/pool
ownership checks before deleting a failed clone. Tests use disposable files
and the Proxmox simulator; no live environment is modified.

| Python test | Result | Evidence or reason |
| --- | --- | --- |
| `tests/test_config.py::InstallDraftTests::test_final_publish_replaces_draft_without_creating_draft_backup` | excluded | The Python same-file draft-to-config publication path is removed. Rust keeps answers in install-draft.toml and publishes separate native client/server files; drafts are never active configuration (install_flow.rs). |
| `tests/test_config.py::InstallDraftTests::test_obsolete_sync_stage_is_rejected` | excluded | The Python completed-stage draft schema and its load_install_draft reader are removed. Rust stores option answers separately, with no sync stage or compatibility reader (install_flow.rs). |
| `tests/test_config.py::InstallDraftTests::test_runtime_loader_rejects_install_draft_with_resume_hint` | ported | an_install_draft_is_rejected_with_the_resume_hint, install_draft_round_trip_preserves_order_empty_choices_and_typed_values |
| `tests/test_create_execution.py::CreateExecutionTests::test_invalid_restored_disk_ownership_prevents_destroy` | ported | invalid_clone_disk_ownership_prevents_cleanup_deletion |
| `tests/test_create_execution.py::CreateExecutionTests::test_stream_command_preserves_binary_payload_and_pipefail_failures` | excluded | Decision 3=A: cross-node creation uses Proxmox full-clone and offline-migration API tasks, not a vzdump/SSH byte stream or shell pipeline (create_transfer.rs). |
| `tests/test_create_execution.py::CreateExecutionTests::test_stream_failure_does_not_allocate_home_boot_or_destroy_unknown_restore` | ported | migration_failure_does_not_allocate_home_or_start, invalid_clone_disk_ownership_prevents_cleanup_deletion |
| `tests/test_create_execution.py::CreateExecutionTests::test_stream_success_precedes_target_home_allocation_and_failure_destroys_target` | ported | create_on_another_node_clones_migrates_and_verifies_there, a_failure_after_the_migration_removes_the_clone_on_the_target |
| `tests/test_install.py::DiscoveryTests::test_discovery_helpers_issue_only_read_only_queries` | ported | install_discovery_is_read_only |
| `tests/test_install.py::DiscoveryTests::test_exactly_one_valid_tagged_gold_is_confirmed` | ported | a_tagged_gold_is_used_before_the_name_and_an_explicit_gold_before_both |
| `tests/test_install.py::DiscoveryTests::test_first_selected_storage_is_preserved_as_layout_default` | ported | storage_selection_preserves_click_order, install_draft_round_trip_preserves_order_empty_choices_and_typed_values |
| `tests/test_install.py::DiscoveryTests::test_fresh_install_does_not_offer_placeholder_or_non_gold_vmid_as_default` | ported | gold_is_optional_when_none_or_several_are_detected, a_tagged_gold_is_used_before_the_name_and_an_explicit_gold_before_both |
| `tests/test_install.py::DiscoveryTests::test_reconfigure_offers_current_gold_only_when_it_is_still_valid` | excluded | Python _choose_gold preference-index prompting is removed. Rust restores still-offered draft choices and validates selected Gold before mutation; its QEMU selector intentionally also permits choosing an untagged base VM for provisioning (hs-install/checks.rs). |
| `tests/test_install.py::DiscoveryTests::test_single_and_multi_node_storage_discovery_respects_restrictions` | ported | only_what_can_be_chosen_is_offered_and_a_single_possibility_is_taken, the_servers_put_workspace_disks_on_the_storages_chosen_in_the_form, an_offline_node_in_a_cluster_stops_the_run |
| `tests/test_install.py::DiscoveryTests::test_single_numbered_option_is_selected_without_prompt` | ported | only_what_can_be_chosen_is_offered_and_a_single_possibility_is_taken |
| `tests/test_install.py::DiscoveryTests::test_zero_gold_requires_explicit_vmid_and_multiple_gold_uses_numbered_selection` | excluded | Owner-approved installer behavior permits installation without Gold. No candidate or ambiguous candidates leave Gold unselected, rather than requiring a VMID (phase-13c.md, hs-install/checks.rs). |
| `tests/test_install.py::ExistingConfigSafetyTests::test_existing_default_action_aborts_without_overwrite` | ported | the_question_is_asked_on_a_terminal_and_no_changes_nothing |
| `tests/test_install.py::ExistingConfigSafetyTests::test_noninteractive_fresh_install_writes_nothing` | ported | a_plan_without_confirmation_is_shown_and_changes_nothing |
| `tests/test_install.py::ExistingConfigSafetyTests::test_reconfigure_uses_loaded_values_as_defaults` | ported | reconfiguration_loads_saved_values_while_fresh_input_has_no_network_placeholders, a_new_install_session_automatically_resumes_saved_answers |
| `tests/test_install.py::ExistingConfigSafetyTests::test_replace_from_scratch_does_not_reuse_existing_values` | excluded | The Python three-action existing-config menu and _fresh_config replacement path are removed. Rust uses explicit seed input and a separate saved-answer draft; absent input starts blank network/user fields (install_config.rs). |
| `tests/test_install.py::InstallCliTests::test_discover_dispatches_without_loading_config` | ported | discover_without_client_configuration_is_read_only |
| `tests/test_install.py::InstallCliTests::test_install_dispatches_before_config_loading_or_transport` | ported | without_an_access_option_the_missing_options_are_listed |
| `tests/test_install.py::InstallCliTests::test_install_parser_and_help_do_not_require_config` | ported | the_commands_are_parsed, the_help_names_both_commands |
| `tests/test_install.py::InstallCliTests::test_normal_command_still_requires_config` | ported | discover_without_client_configuration_is_read_only |
| `tests/test_install.py::InstallCliTests::test_parser_default_observes_current_xdg_location` | ported | client_configuration_path_observes_current_xdg_location |
| `tests/test_install.py::InstallDraftResumeTests::test_checkpoint_persists_stage_immediately` | ported | the_access_that_connected_is_kept_for_the_next_install_screen, a_new_install_session_automatically_resumes_saved_answers |
| `tests/test_install.py::InstallDraftResumeTests::test_config_restore_rejects_obsolete_sync_stage` | excluded | Python _config_from_install_draft and its completed-stage schema are removed. Rust has a separate option-answer draft and no sync-stage parser or compatibility restoration path (install_flow.rs). |
| `tests/test_install.py::InstallDraftResumeTests::test_draft_round_trip_restores_completed_values_without_placeholder_leakage` | ported | install_draft_round_trip_preserves_order_empty_choices_and_typed_values, a_new_install_session_automatically_resumes_saved_answers |
| `tests/test_install.py::InstallDraftResumeTests::test_resume_action_defaults_to_continue` | ported | a_new_install_session_automatically_resumes_saved_answers |
| `tests/test_install.py::InstallerValueTests::test_detected_ssh_identities_use_number_selection_not_long_path_default` | ported | workspace_keys_are_choices_and_resume_in_the_selected_order, first_install_collects_workspace_settings_without_an_input_file |
| `tests/test_install.py::InstallerValueTests::test_discovered_network_profile_is_selected_as_one_unit` | excluded | Owner-approved settings form collects workspace network fields explicitly and selects the server bridge independently. Python NetworkCandidate auto-acceptance is not part of this flow (phase-13c.md section 9, install_config.rs). |
| `tests/test_install.py::InstallerValueTests::test_install_has_no_sync_stage_or_configuration_hook` | ported | first_install_collects_workspace_settings_without_an_input_file, sync_command_is_rejected |
| `tests/test_install.py::InstallerValueTests::test_manual_network_has_no_placeholder_defaults_on_fresh_install` | ported | reconfiguration_loads_saved_values_while_fresh_input_has_no_network_placeholders, first_install_collects_workspace_settings_without_an_input_file |

Rust resumes form answers automatically; it does not claim to resume the
Python completed-stage ledger. Gold is optional, the network is entered in
the settings form, and cross-node creation uses full clone plus migration.
The existing-config replacement menu, automatic network-profile acceptance,
and binary shell transfer pipeline are deliberately absent.
