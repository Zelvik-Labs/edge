use crate::api_config::ApiConfig;
use crate::config::ConfigStore;
use crate::pb;
use crate::rpc::api_node::sync_updating_server_list_once;
use crate::rpc::cache::sync_cache_tasks;
use crate::rpc::client::SharedRpcClient;
use crate::rpc::ip_list::sync_ip_items_incremental;
use crate::rpc::plan::sync_active_plans;
use crate::rpc::server::{SyncOutcome, sync_single_server_config, sync_user_servers_state};
use std::sync::LazyLock as Lazy;
use tokio::sync::Notify;
use tracing::{debug, info, warn};

static TASK_SYNC_NOTIFY: Lazy<Notify> = Lazy::new(Notify::new);

pub fn trigger_task_sync() {
    info!("Triggering immediate task sync...");
    TASK_SYNC_NOTIFY.notify_one();
}

pub async fn wait_for_task_sync() {
    TASK_SYNC_NOTIFY.notified().await;
}

pub async fn sync_node_tasks(
    api_config: &ApiConfig,
    config_store: &ConfigStore,
    health_manager: &crate::health_manager::GlobalHealthManager,
    cert_selector: &crate::ssl::DynamicCertSelector,
    ip_list_manager: &crate::firewall::lists::GlobalIpListManager,
    task_version: &mut i64,
    config_synced: bool,
) {
    let client = match SharedRpcClient::get(api_config).await {
        Ok(shared) => shared.as_rpc_client(),
        Err(_) => return,
    };
    let mut task_service = client.node_task_service();

    let req = pb::FindNodeTasksRequest {
        version: *task_version,
    };
    match crate::rpc::track_rpc(task_service.find_node_tasks(req)).await {
        Ok(resp) => {
            let tasks = resp.into_inner().node_tasks;
            for task in tasks {
                info!("Processing NodeTask: {} (Type: {})", task.id, task.r#type);
                let task_type = task.r#type.as_str();
                // The admin may embed a target version as upgradeNode@{"version":…}.
                let mut pending_upgrade_restart = false;
                let mut task_error = String::new();
                let outcome = match task_type {
                    t if crate::node_upgrade::parse_upgrade_task_version(t).is_some() => {
                        // The upgrade payload is downloaded from the admin
                        // management address over plain HTTP(S); the gRPC
                        // channel only carried this task reference.
                        match crate::node_upgrade::execute_node_upgrade(api_config, t, task.id)
                            .await
                        {
                            Ok(needs_restart) => {
                                pending_upgrade_restart = needs_restart;
                                SyncOutcome::Applied
                            }
                            Err(err) => {
                                warn!("NodeTask {} upgrade failed: {}", task.id, err);
                                task_error = format!("{err:#}");
                                SyncOutcome::Failed
                            }
                        }
                    }
                    "configChanged"
                    | "ddosProtectionChanged"
                    | "globalServerConfigChanged"
                    | "uamPolicyChanged"
                    | "httpCCPolicyChanged"
                    | "http3PolicyChanged"
                    | "httpPagesPolicyChanged"
                    | "toaChanged"
                    | "networkSecurityPolicyChanged"
                    | "webPPolicyChanged"
                    | "accessLogChanged"
                    | "dnsResolverChanged"
                    | "grpcPolicyChanged"
                    | "scheduleChanged"
                    | "apiConfigChanged"
                    | "indexNodeConfigChanged"
                    | "cachePolicyChanged"
                    | "firewallPolicyChanged" => {
                        if task.server_id > 0 {
                            sync_single_server_config(
                                api_config,
                                config_store,
                                health_manager,
                                task.server_id,
                            )
                            .await
                        } else if config_synced {
                            trigger_task_sync();
                            SyncOutcome::Applied
                        } else {
                            warn!(
                                "Deferring NodeTask {} ({}) because config sync has not completed successfully in this round.",
                                task.id, task.r#type
                            );
                            SyncOutcome::Deferred
                        }
                    }
                    "nodeLevelChanged" => SyncOutcome::from_applied(
                        crate::rpc::find_node_level_info(api_config, config_store).await,
                    ),
                    "planChanged" => {
                        SyncOutcome::from_applied(sync_active_plans(api_config, config_store).await)
                    }
                    "purgeServerCache" | "purgePathCache" | "preheatCache" => {
                        SyncOutcome::from_applied(
                            sync_cache_tasks(
                                client.channel(),
                                api_config,
                                config_store,
                                task.id,
                                task.server_id,
                            )
                            .await,
                        )
                    }
                    "ipItemChanged" => SyncOutcome::from_applied(
                        sync_ip_items_incremental(api_config, ip_list_manager).await,
                    ),
                    "updatingServers" => {
                        let mut last_id = 0i64;
                        SyncOutcome::from_applied(
                            sync_updating_server_list_once(
                                api_config,
                                config_store,
                                health_manager,
                                cert_selector,
                                &mut last_id,
                            )
                            .await,
                        )
                    }
                    "userServersStateChanged" => {
                        sync_user_servers_state(
                            api_config,
                            config_store,
                            health_manager,
                            task.user_id,
                        )
                        .await
                    }
                    "installNode" | "startNode" => {
                        // updateNodeIsInstalled requires admin credentials on cloud API,
                        // so we cannot mark this from the node side. zelvik-edge does not
                        // execute these lifecycle tasks locally, so acknowledge them to
                        // avoid retry loops.
                        info!(
                            "Received node lifecycle task '{}'. Acknowledging locally.",
                            task.r#type
                        );
                        SyncOutcome::Applied
                    }
                    "scriptsChanged" => {
                        let unsupported = crate::unsupported::request_scripts::unsupported();
                        warn!(
                            "Ignoring unsupported task 'scriptsChanged': {} ({})",
                            unsupported.reason, unsupported.code
                        );
                        SyncOutcome::Applied
                    }
                    "plusChanged" | "nodeVersionChanged" => SyncOutcome::Applied,
                    other => {
                        warn!(
                            "Unknown NodeTask type '{}' (task id={}). Marking done to avoid retry loop.",
                            other, task.id
                        );
                        SyncOutcome::Applied
                    }
                };

                if let Some(options) = task_type.strip_prefix("ipListDeleted@")
                    && let Ok(value) = serde_json::from_str::<serde_json::Value>(options)
                    && let Some(list_id) = value.get("listId").and_then(|v| v.as_i64())
                {
                    ip_list_manager.remove_list(list_id);
                }

                if pending_upgrade_restart {
                    // Completion belongs to the independent supervisor after startup,
                    // including a failure result if it restores the old package.
                    info!(
                        "NodeTask {} installation staged; supervisor will report restart result",
                        task.id
                    );
                    if task.version > *task_version {
                        *task_version = task.version;
                    }
                    return;
                }

                match completion_request(task.id, outcome, &task_error) {
                    None => {
                        // `ReportNodeTaskDone` marks the task done in the
                        // control plane even with `is_ok=false`, and
                        // `FindNodeTasks` only returns tasks that are not done.
                        // A locally deferred install therefore must not be
                        // acknowledged, or the update would never be retried.
                        warn!(
                            task_id = task.id,
                            task_type = task.r#type,
                            "NodeTask deferred locally; leaving it pending for a later retry"
                        );
                        break;
                    }
                    Some(request) => {
                        let reported = crate::rpc::track_rpc(
                            task_service.report_node_task_done(request),
                        )
                        .await
                        .is_ok();

                        if reported {
                            if task.version > *task_version {
                                *task_version = task.version;
                            }
                        } else {
                            warn!("Failed to report NodeTask {} completion", task.id);
                            break;
                        }
                    }
                }
            }
        }
        Err(e) => {
            debug!("Failed to find node tasks: {}", e);
        }
    }
}

/// The acknowledgement to send for a finished task, or `None` when the task
/// must stay pending.
///
/// The control plane marks a task done on any `ReportNodeTaskDone`, including
/// `is_ok=false`, and only returns tasks that are not done, so a retryable
/// local deferral (exhausted metric family, governor rejection) must produce
/// no report at all. Terminal failures keep the previous behavior and are
/// reported so the failure is visible and does not retry forever.
fn completion_request(
    task_id: i64,
    outcome: SyncOutcome,
    task_error: &str,
) -> Option<pb::ReportNodeTaskDoneRequest> {
    match outcome {
        SyncOutcome::Deferred => None,
        SyncOutcome::Applied => Some(pb::ReportNodeTaskDoneRequest {
            node_task_id: task_id,
            is_ok: true,
            error: String::new(),
        }),
        SyncOutcome::Failed => Some(pb::ReportNodeTaskDoneRequest {
            node_task_id: task_id,
            is_ok: false,
            error: if task_error.is_empty() {
                "Task failed".to_string()
            } else {
                task_error.to_string()
            },
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferred_install_is_not_acknowledged() {
        // Regression: a capacity rejection reported as `is_ok=false` used to
        // mark the task done in the control plane, permanently losing the
        // config update.
        assert!(completion_request(7, SyncOutcome::Deferred, "capacity").is_none());
    }

    #[test]
    fn applied_task_reports_success() {
        let request = completion_request(7, SyncOutcome::Applied, "").expect("applied reports");
        assert_eq!(request.node_task_id, 7);
        assert!(request.is_ok);
        assert!(request.error.is_empty());
    }

    #[test]
    fn terminal_failure_reports_its_error() {
        let request =
            completion_request(7, SyncOutcome::Failed, "decode error").expect("failed reports");
        assert_eq!(request.node_task_id, 7);
        assert!(!request.is_ok);
        assert_eq!(request.error, "decode error");
    }

    #[test]
    fn terminal_failure_without_detail_reports_a_generic_error() {
        let request = completion_request(7, SyncOutcome::Failed, "").expect("failed reports");
        assert!(!request.is_ok);
        assert_eq!(request.error, "Task failed");
    }
}
