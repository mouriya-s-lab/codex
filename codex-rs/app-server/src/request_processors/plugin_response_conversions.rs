use super::*;
use codex_app_server_protocol::PluginAvailability;
use codex_app_server_protocol::PluginSharePrincipalRole;
use codex_app_server_protocol::PluginShareTargetRole;

pub(super) fn plugin_skills_to_info(
    skills: &[codex_core::skills::SkillMetadata],
    disabled_skill_paths: &HashSet<AbsolutePathBuf>,
) -> Vec<SkillSummary> {
    skills
        .iter()
        .map(|skill| SkillSummary {
            name: skill.name.clone(),
            description: skill.description.clone(),
            short_description: skill.short_description.clone(),
            interface: skill.interface.clone().map(|interface| {
                codex_app_server_protocol::SkillInterface {
                    display_name: interface.display_name,
                    short_description: interface.short_description,
                    icon_small: interface.icon_small,
                    icon_large: interface.icon_large,
                    brand_color: interface.brand_color,
                    default_prompt: interface.default_prompt,
                }
            }),
            path: Some(skill.path_to_skills_md.clone()),
            enabled: !disabled_skill_paths.contains(&skill.path_to_skills_md),
        })
        .collect()
}

pub(super) fn local_plugin_interface_to_info(
    interface: PluginManifestInterface,
) -> PluginInterface {
    PluginInterface {
        display_name: interface.display_name,
        short_description: interface.short_description,
        long_description: interface.long_description,
        developer_name: interface.developer_name,
        category: interface.category,
        capabilities: interface.capabilities,
        website_url: interface.website_url,
        privacy_policy_url: interface.privacy_policy_url,
        terms_of_service_url: interface.terms_of_service_url,
        default_prompt: interface.default_prompt,
        brand_color: interface.brand_color,
        composer_icon: interface.composer_icon,
        composer_icon_url: None,
        logo: interface.logo,
        logo_url: None,
        screenshots: interface.screenshots,
        screenshot_urls: Vec::new(),
    }
}

pub(super) fn marketplace_plugin_source_to_info(source: MarketplacePluginSource) -> PluginSource {
    match source {
        MarketplacePluginSource::Local { path } => PluginSource::Local { path },
        MarketplacePluginSource::Git {
            url,
            path,
            ref_name,
            sha,
        } => PluginSource::Git {
            url,
            path,
            ref_name,
            sha,
        },
    }
}

pub(super) fn load_shared_plugin_ids_by_local_path(
    config: &Config,
) -> Result<std::collections::BTreeMap<AbsolutePathBuf, String>, JSONRPCErrorError> {
    codex_core_plugins::remote::load_plugin_share_remote_ids_by_local_path(
        config.codex_home.as_path(),
    )
    .map_err(|err| {
        internal_error(format!(
            "failed to load plugin share local path mapping: {err}"
        ))
    })
}

pub(super) fn share_context_for_source(
    source: &MarketplacePluginSource,
    shared_plugin_ids_by_local_path: &std::collections::BTreeMap<AbsolutePathBuf, String>,
) -> Option<PluginShareContext> {
    match source {
        MarketplacePluginSource::Local { path } => shared_plugin_ids_by_local_path
            .get(path)
            .cloned()
            .map(|remote_plugin_id| PluginShareContext {
                remote_plugin_id,
                remote_version: None,
                discoverability: None,
                share_url: None,
                creator_account_user_id: None,
                creator_name: None,
                share_principals: None,
            }),
        MarketplacePluginSource::Git { .. } => None,
    }
}

pub(super) fn convert_configured_marketplace_to_plugin_marketplace_entry(
    marketplace: codex_core_plugins::ConfiguredMarketplace,
    shared_plugin_ids_by_local_path: &std::collections::BTreeMap<AbsolutePathBuf, String>,
) -> PluginMarketplaceEntry {
    PluginMarketplaceEntry {
        name: marketplace.name,
        path: Some(marketplace.path),
        interface: marketplace.interface.map(|interface| MarketplaceInterface {
            display_name: interface.display_name,
        }),
        plugins: marketplace
            .plugins
            .into_iter()
            .map(|plugin| {
                convert_configured_marketplace_plugin_to_plugin_summary(
                    plugin,
                    shared_plugin_ids_by_local_path,
                )
            })
            .collect(),
    }
}

pub(super) fn convert_configured_marketplace_plugin_to_plugin_summary(
    plugin: codex_core_plugins::ConfiguredMarketplacePlugin,
    shared_plugin_ids_by_local_path: &std::collections::BTreeMap<AbsolutePathBuf, String>,
) -> PluginSummary {
    let share_context = share_context_for_source(&plugin.source, shared_plugin_ids_by_local_path);
    PluginSummary {
        id: plugin.id,
        remote_plugin_id: None,
        local_version: plugin.local_version,
        installed: plugin.installed,
        enabled: plugin.enabled,
        name: plugin.name,
        share_context,
        source: marketplace_plugin_source_to_info(plugin.source),
        install_policy: plugin.policy.installation.into(),
        auth_policy: plugin.policy.authentication.into(),
        availability: PluginAvailability::Available,
        interface: plugin.interface.map(local_plugin_interface_to_info),
        keywords: plugin.keywords,
    }
}

pub(super) fn merge_plugin_marketplace_entry(
    data: &mut Vec<PluginMarketplaceEntry>,
    incoming: PluginMarketplaceEntry,
) {
    let Some(existing) = data
        .iter_mut()
        .find(|marketplace| marketplace.name == incoming.name)
    else {
        data.push(incoming);
        return;
    };

    if existing.interface.is_none() {
        existing.interface = incoming.interface;
    }
    if incoming.path.is_some() {
        existing.path = incoming.path.clone();
    }

    let mut seen_plugin_ids = existing
        .plugins
        .iter()
        .map(|plugin| plugin.id.clone())
        .collect::<HashSet<_>>();
    existing.plugins.extend(
        incoming
            .plugins
            .into_iter()
            .filter(|plugin| seen_plugin_ids.insert(plugin.id.clone())),
    );
}

pub(super) fn convert_marketplace_load_errors(
    errors: Vec<codex_core_plugins::marketplace::MarketplaceListError>,
) -> Vec<codex_app_server_protocol::MarketplaceLoadErrorInfo> {
    errors
        .into_iter()
        .map(|err| codex_app_server_protocol::MarketplaceLoadErrorInfo {
            marketplace_path: err.path,
            message: err.message,
        })
        .collect()
}

pub(super) fn remote_plugin_share_discoverability(
    discoverability: PluginShareDiscoverability,
) -> codex_core_plugins::remote::RemotePluginShareDiscoverability {
    match discoverability {
        PluginShareDiscoverability::Listed => {
            codex_core_plugins::remote::RemotePluginShareDiscoverability::Listed
        }
        PluginShareDiscoverability::Unlisted => {
            codex_core_plugins::remote::RemotePluginShareDiscoverability::Unlisted
        }
        PluginShareDiscoverability::Private => {
            codex_core_plugins::remote::RemotePluginShareDiscoverability::Private
        }
    }
}

pub(super) fn remote_plugin_share_update_discoverability(
    discoverability: PluginShareUpdateDiscoverability,
) -> codex_core_plugins::remote::RemotePluginShareUpdateDiscoverability {
    match discoverability {
        PluginShareUpdateDiscoverability::Unlisted => {
            codex_core_plugins::remote::RemotePluginShareUpdateDiscoverability::Unlisted
        }
        PluginShareUpdateDiscoverability::Private => {
            codex_core_plugins::remote::RemotePluginShareUpdateDiscoverability::Private
        }
    }
}

fn remote_plugin_share_target_role(
    role: PluginShareTargetRole,
) -> codex_core_plugins::remote::RemotePluginShareTargetRole {
    match role {
        PluginShareTargetRole::Reader => {
            codex_core_plugins::remote::RemotePluginShareTargetRole::Reader
        }
        PluginShareTargetRole::Editor => {
            codex_core_plugins::remote::RemotePluginShareTargetRole::Editor
        }
    }
}

fn plugin_share_principal_role_from_remote(
    role: codex_core_plugins::remote::RemotePluginSharePrincipalRole,
) -> PluginSharePrincipalRole {
    match role {
        codex_core_plugins::remote::RemotePluginSharePrincipalRole::Reader => {
            PluginSharePrincipalRole::Reader
        }
        codex_core_plugins::remote::RemotePluginSharePrincipalRole::Editor => {
            PluginSharePrincipalRole::Editor
        }
        codex_core_plugins::remote::RemotePluginSharePrincipalRole::Owner => {
            PluginSharePrincipalRole::Owner
        }
    }
}

pub(super) fn remote_plugin_share_targets(
    targets: Vec<PluginShareTarget>,
) -> Vec<codex_core_plugins::remote::RemotePluginShareTarget> {
    targets
        .into_iter()
        .map(
            |target| codex_core_plugins::remote::RemotePluginShareTarget {
                principal_type: match target.principal_type {
                    PluginSharePrincipalType::User => {
                        codex_core_plugins::remote::RemotePluginSharePrincipalType::User
                    }
                    PluginSharePrincipalType::Group => {
                        codex_core_plugins::remote::RemotePluginSharePrincipalType::Group
                    }
                    PluginSharePrincipalType::Workspace => {
                        codex_core_plugins::remote::RemotePluginSharePrincipalType::Workspace
                    }
                },
                principal_id: target.principal_id,
                role: remote_plugin_share_target_role(target.role),
            },
        )
        .collect()
}

pub(super) fn plugin_share_principal_from_remote(
    principal: codex_core_plugins::remote::RemotePluginSharePrincipal,
) -> PluginSharePrincipal {
    PluginSharePrincipal {
        principal_type: match principal.principal_type {
            codex_core_plugins::remote::RemotePluginSharePrincipalType::User => {
                PluginSharePrincipalType::User
            }
            codex_core_plugins::remote::RemotePluginSharePrincipalType::Group => {
                PluginSharePrincipalType::Group
            }
            codex_core_plugins::remote::RemotePluginSharePrincipalType::Workspace => {
                PluginSharePrincipalType::Workspace
            }
        },
        principal_id: principal.principal_id,
        role: plugin_share_principal_role_from_remote(principal.role),
        name: principal.name,
    }
}

pub(super) fn remote_marketplace_to_info(marketplace: RemoteMarketplace) -> PluginMarketplaceEntry {
    PluginMarketplaceEntry {
        name: marketplace.name,
        path: None,
        interface: Some(MarketplaceInterface {
            display_name: Some(marketplace.display_name),
        }),
        plugins: marketplace
            .plugins
            .into_iter()
            .map(remote_plugin_summary_to_info)
            .collect(),
    }
}

pub(super) fn remote_plugin_summary_to_info(summary: RemoteCatalogPluginSummary) -> PluginSummary {
    PluginSummary {
        id: summary.id,
        remote_plugin_id: Some(summary.remote_plugin_id),
        local_version: None,
        name: summary.name,
        share_context: summary
            .share_context
            .map(remote_plugin_share_context_to_info),
        source: PluginSource::Remote,
        installed: summary.installed,
        enabled: summary.enabled,
        install_policy: summary.install_policy,
        auth_policy: summary.auth_policy,
        availability: summary.availability,
        interface: summary.interface,
        keywords: summary.keywords,
    }
}

pub(super) fn remote_plugin_share_context_to_info(
    context: RemoteCatalogPluginShareContext,
) -> PluginShareContext {
    PluginShareContext {
        remote_plugin_id: context.remote_plugin_id,
        remote_version: context.remote_version,
        discoverability: Some(remote_plugin_share_discoverability_to_info(
            context.discoverability,
        )),
        share_url: context.share_url,
        creator_account_user_id: context.creator_account_user_id,
        creator_name: context.creator_name,
        share_principals: context.share_principals.map(|principals| {
            principals
                .into_iter()
                .map(plugin_share_principal_from_remote)
                .collect()
        }),
    }
}

pub(super) fn remote_plugin_share_discoverability_to_info(
    discoverability: codex_core_plugins::remote::RemotePluginShareDiscoverability,
) -> PluginShareDiscoverability {
    match discoverability {
        codex_core_plugins::remote::RemotePluginShareDiscoverability::Listed => {
            PluginShareDiscoverability::Listed
        }
        codex_core_plugins::remote::RemotePluginShareDiscoverability::Unlisted => {
            PluginShareDiscoverability::Unlisted
        }
        codex_core_plugins::remote::RemotePluginShareDiscoverability::Private => {
            PluginShareDiscoverability::Private
        }
    }
}

pub(super) fn remote_plugin_detail_to_info(
    detail: RemoteCatalogPluginDetail,
    apps: Vec<AppSummary>,
) -> PluginDetail {
    PluginDetail {
        marketplace_name: detail.marketplace_name,
        marketplace_path: None,
        summary: remote_plugin_summary_to_info(detail.summary),
        description: detail.description,
        skills: detail
            .skills
            .into_iter()
            .map(|skill| SkillSummary {
                name: skill.name,
                description: skill.description,
                short_description: skill.short_description,
                interface: skill.interface,
                path: None,
                enabled: skill.enabled,
            })
            .collect(),
        hooks: Vec::new(),
        apps,
        mcp_servers: Vec::new(),
    }
}
