use crate::api_config::ApiConfig;
use crate::config::ConfigStore;
use crate::pb;
use crate::rpc::api_node::sync_updating_server_list_once;
use crate::rpc::cache::sync_cache_tasks;
use crate::rpc::client::SharedRpcClient;
use crate::rpc::ip_list::sync_ip_items_incremental;
use crate::rpc::plan::sync_active_plans;
use crate::rpc::server::{sync_single_server_config, sync_user_servers_state};
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
                let success = match task_type {
                    t if crate::node_upgrade::parse_upgrade_task_version(t).is_some() => {
                        // The upgrade payload is downloaded from the admin
                        // management address over plain HTTP(S); the gRPC
                        // channel only carried this task reference.
                        match crate::node_upgrade::execute_node_upgrade(api_config, t, task.id)
                            .await
                        {
                            Ok(needs_restart) => {
                                pending_upgrade_restart = needs_restart;
                                true
                            }
                            Err(err) => {
                                warn!("NodeTask {} upgrade failed: {}", task.id, err);
                                task_error = format!("{err:#}");
                                false
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
                            true
                        } else {
                            warn!(
                                "Deferring NodeTask {} ({}) because config sync has not completed successfully in this round.",
                                task.id, task.r#type
                            );
                            break;
                        }
                    }
                    "nodeLevelChanged" => {
                        crate::rpc::find_node_level_info(api_config, config_store).await
                    }
                    "planChanged" => sync_active_plans(api_config, config_store).await,
                    "purgeServerCache" | "purgePathCache" | "preheatCache" => {
                        sync_cache_tasks(
                            client.channel(),
                            api_config,
                            config_store,
                            task.id,
                            task.server_id,
                        )
                        .await
                    }
                    "ipItemChanged" => sync_ip_items_incremental(api_config, ip_list_manager).await,
                    "updatingServers" => {
                        let mut last_id = 0i64;
                        sync_updating_server_list_once(
                            api_config,
                            config_store,
                            health_manager,
                            cert_selector,
                            &mut last_id,
                        )
                        .await
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
                        // so we cannot mark this from the node side. cloudsidra-edge does not
                        // execute these lifecycle tasks locally, so acknowledge them to
                        // avoid retry loops.
                        info!(
                            "Received node lifecycle task '{}'. Acknowledging locally.",
                            task.r#type
                        );
                        true
                    }
                    "scriptsChanged" => {
                        let unsupported = crate::unsupported::request_scripts::unsupported();
                        warn!(
                            "Ignoring unsupported task 'scriptsChanged': {} ({})",
                            unsupported.reason, unsupported.code
                        );
                        true
                    }
                    "plusChanged" | "nodeVersionChanged" => true,
                    other => {
                        warn!(
                            "Unknown NodeTask type '{}' (task id={}). Marking done to avoid retry loop.",
                            other, task.id
                        );
                        true
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

                let reported = crate::rpc::track_rpc(task_service.report_node_task_done(
                    pb::ReportNodeTaskDoneRequest {
                        node_task_id: task.id,
                        is_ok: success,
                        error: if success {
                            "".to_string()
                        } else if task_error.is_empty() {
                            "Task failed".to_string()
                        } else {
                            task_error.clone()
                        },
                    },
                ))
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
        Err(e) => {
            debug!("Failed to find node tasks: {}", e);
        }
    }
}
