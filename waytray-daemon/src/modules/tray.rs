//! Tray module - wraps the existing StatusNotifierItem functionality as a module

use async_trait::async_trait;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;
use tokio::time::{self, Duration, MissedTickBehavior};
use zbus::Connection;

use crate::cache::ItemCache;
use crate::config::TrayModuleConfig;
use crate::host::Host;
use crate::TrayItem;

use super::{ItemAction, Module, ModuleContext, ModuleItem};

/// The system tray module - wraps SNI protocol as a module
pub struct TrayModule {
    config: RwLock<TrayModuleConfig>,
    host: RwLock<Option<Arc<Host>>>,
    cache: Arc<ItemCache>,
    connection: Connection,
}

impl TrayModule {
    pub fn new(config: TrayModuleConfig, connection: Connection) -> Self {
        Self {
            config: RwLock::new(config),
            host: RwLock::new(None),
            cache: ItemCache::new(),
            connection,
        }
    }

    /// Convert a TrayItem to a ModuleItem
    fn tray_item_to_module_item(item: &TrayItem) -> ModuleItem {
        let mut module_item = ModuleItem {
            id: format!("tray:{}", item.id),
            module: "tray".to_string(),
            label: item.title.clone(),
            icon_name: item.icon_name.clone(),
            icon_pixmap: item.icon_pixmap.clone(),
            icon_width: item.icon_width,
            icon_height: item.icon_height,
            tooltip: item.tooltip.clone(),
            actions: Vec::new(),
        };

        // Add actions based on item capabilities
        if item.item_is_menu {
            // Menu-only items just have a context menu action
            module_item
                .actions
                .push(ItemAction::default_action("context_menu", "Show Menu"));
        } else {
            // Regular items have activate as default
            module_item
                .actions
                .push(ItemAction::default_action("activate", "Activate"));
            module_item
                .actions
                .push(ItemAction::new("secondary_activate", "Secondary Action"));
        }

        // All items with a menu can show context menu
        if item.has_menu {
            if !item.item_is_menu {
                module_item
                    .actions
                    .push(ItemAction::new("context_menu", "Show Menu"));
            }
        }

        module_item
    }


    fn module_items_equal_ignoring_tooltip(a: &[ModuleItem], b: &[ModuleItem]) -> bool {
        if a.len() != b.len() {
            return false;
        }

        a.iter().zip(b.iter()).all(|(left, right)| {
            left.id == right.id
                && left.module == right.module
                && left.label == right.label
                && left.icon_name == right.icon_name
                && left.icon_pixmap == right.icon_pixmap
                && left.icon_width == right.icon_width
                && left.icon_height == right.icon_height
                && left.actions == right.actions
        })
    }

    /// Get the underlying host for direct access (used by dbus_service for backwards compat)
    pub async fn get_host(&self) -> Option<Arc<Host>> {
        self.host.read().await.clone()
    }

    /// Get the cache for direct access
    pub fn get_cache(&self) -> Arc<ItemCache> {
        self.cache.clone()
    }
}

#[async_trait]
impl Module for TrayModule {
    fn name(&self) -> &str {
        "tray"
    }

    fn enabled(&self) -> bool {
        self.config.try_read().map(|c| c.enabled).unwrap_or(true)
    }

    async fn start(&self, ctx: Arc<ModuleContext>) {
        if !self.config.read().await.enabled {
            return;
        }

        // Create and start the host
        let host = match Host::new(self.connection.clone(), self.cache.clone()).await {
            Ok(h) => Arc::new(h),
            Err(e) => {
                tracing::error!("Failed to create SNI host: {}", e);
                return;
            }
        };

        // Store the host
        {
            let mut host_lock = self.host.write().await;
            *host_lock = Some(host.clone());
        }

        // Start the host
        if let Err(e) = host.start().await {
            tracing::error!("Failed to start SNI host: {}", e);
            return;
        }

        // Watch for D-Bus name changes
        if let Err(e) =
            crate::host::watch_name_changes(self.connection.clone(), self.cache.clone()).await
        {
            tracing::warn!("Failed to set up name change watcher: {}", e);
        }

        // Send initial items
        let tray_items = self.cache.get_all().await;
        let module_items: Vec<ModuleItem> = tray_items
            .iter()
            .map(TrayModule::tray_item_to_module_item)
            .collect();
        ctx.send_items("tray", module_items.clone());
        let mut last_sent = Some(module_items);

        // Subscribe to cache events and forward them to the module context
        let mut receiver = self.cache.subscribe();

        tracing::info!("Tray module started");

        // Coalesce bursts of tray updates to avoid flooding downstream with
        // full snapshot refreshes on every single signal.
        let mut refresh_pending = false;
        let mut refresh_tick = time::interval(Duration::from_millis(250));
        refresh_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

        // Detect and rate-limit tooltip-only update floods (e.g. apps updating
        // progress text many times per second). Normal updates still flow through.
        let mut tooltip_only_change_times: VecDeque<Instant> = VecDeque::new();
        let tooltip_flood_window = Duration::from_secs(2);
        let tooltip_flood_threshold = 6usize;
        let tooltip_flood_cooldown = Duration::from_secs(10);
        let tooltip_flood_emit_interval = Duration::from_secs(5);
        let mut tooltip_flood_until: Option<Instant> = None;
        let mut last_tooltip_publish: Option<Instant> = None;

        // Run event loop directly (not spawned) so start() doesn't return
        loop {
            tokio::select! {
                _ = ctx.cancelled() => {
                    tracing::debug!("Tray module event loop cancelled");
                    break;
                }
                result = receiver.recv() => {
                    match result {
                        Ok(_event) => {
                            refresh_pending = true;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!("Tray module lagged by {} events", n);
                            refresh_pending = true;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            tracing::info!("Tray module cache channel closed");
                            break;
                        }
                    }
                }
                _ = refresh_tick.tick() => {
                    if refresh_pending {
                        let tray_items = self.cache.get_all().await;
                        let module_items: Vec<ModuleItem> = tray_items
                            .iter()
                            .map(TrayModule::tray_item_to_module_item)
                            .collect();

                        let now = Instant::now();
                        let mut should_send = false;
                        let mut tooltip_only_change = false;

                        match last_sent.as_ref() {
                            None => {
                                should_send = true;
                            }
                            Some(previous) => {
                                let full_changed = previous != &module_items;
                                if full_changed {
                                    let changed_beyond_tooltip =
                                        !TrayModule::module_items_equal_ignoring_tooltip(
                                            previous,
                                            &module_items,
                                        );

                                    if changed_beyond_tooltip {
                                        should_send = true;
                                        tooltip_only_change_times.clear();
                                        tooltip_flood_until = None;
                                    } else {
                                        tooltip_only_change = true;

                                        tooltip_only_change_times.push_back(now);
                                        while let Some(front) = tooltip_only_change_times.front() {
                                            if now.duration_since(*front) > tooltip_flood_window {
                                                tooltip_only_change_times.pop_front();
                                            } else {
                                                break;
                                            }
                                        }

                                        if tooltip_only_change_times.len() >= tooltip_flood_threshold {
                                            let already_in_flood =
                                                tooltip_flood_until.map(|until| until > now).unwrap_or(false);
                                            tooltip_flood_until = Some(now + tooltip_flood_cooldown);
                                            if !already_in_flood {
                                                tracing::debug!(
                                                    "Detected tray tooltip flood; throttling tooltip-only updates to every {}s",
                                                    tooltip_flood_emit_interval.as_secs()
                                                );
                                            }
                                        }

                                        let in_flood =
                                            tooltip_flood_until.map(|until| until > now).unwrap_or(false);
                                        if in_flood {
                                            should_send = last_tooltip_publish
                                                .map(|last| {
                                                    now.duration_since(last)
                                                        >= tooltip_flood_emit_interval
                                                })
                                                .unwrap_or(true);
                                        } else {
                                            should_send = true;
                                        }
                                    }
                                }
                            }
                        }

                        if should_send {
                            ctx.send_items("tray", module_items.clone());
                            last_sent = Some(module_items);
                            if tooltip_only_change {
                                last_tooltip_publish = Some(now);
                            }
                        }

                        refresh_pending = false;
                    }
                }
            }
        }
    }

    async fn stop(&self) {
        // Clear the host reference
        let mut host_lock = self.host.write().await;
        *host_lock = None;
        tracing::info!("Tray module stopped");
    }

    async fn invoke_action(&self, item_id: &str, action_id: &str, x: i32, y: i32) {
        // Parse the item ID - format is "tray:{original_id}"
        let original_id = match item_id.strip_prefix("tray:") {
            Some(id) => id,
            None => {
                tracing::warn!("Invalid tray item ID: {}", item_id);
                return;
            }
        };

        let host = match self.host.read().await.clone() {
            Some(h) => h,
            None => {
                tracing::warn!("Tray host not available");
                return;
            }
        };

        let result = match action_id {
            "activate" => host.activate_item(original_id, x, y).await,
            "secondary_activate" => host.secondary_activate_item(original_id, x, y).await,
            "context_menu" => host.context_menu_item(original_id, x, y).await,
            _ => {
                tracing::warn!("Unknown action: {}", action_id);
                return;
            }
        };

        if let Err(e) = result {
            tracing::warn!("Failed to invoke {} on {}: {}", action_id, original_id, e);
        }
    }

    async fn reload_config(&self, config: &crate::config::Config) -> bool {
        let mut current = self.config.write().await;
        *current = config.modules.tray.clone();
        tracing::debug!("Tray module config reloaded");
        true
    }

    async fn get_menu_items(
        &self,
        item_id: &str,
    ) -> anyhow::Result<Vec<crate::dbusmenu::MenuItem>> {
        // Parse the item ID - format is "tray:{original_id}"
        let original_id = item_id
            .strip_prefix("tray:")
            .ok_or_else(|| anyhow::anyhow!("Invalid tray item ID: {}", item_id))?;

        let host = self
            .host
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Tray host not available"))?;

        host.get_menu_items(original_id).await
    }

    async fn activate_menu_item(&self, item_id: &str, menu_item_id: i32) -> anyhow::Result<()> {
        // Parse the item ID - format is "tray:{original_id}"
        let original_id = item_id
            .strip_prefix("tray:")
            .ok_or_else(|| anyhow::anyhow!("Invalid tray item ID: {}", item_id))?;

        let host = self
            .host
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Tray host not available"))?;

        host.activate_menu_item(original_id, menu_item_id).await
    }
}
