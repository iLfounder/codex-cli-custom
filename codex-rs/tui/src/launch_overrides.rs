use std::path::Path;

use codex_config::LoaderOverrides;
use codex_utils_home_dir::ManagedAccountCatalog;

use crate::Cli;
use crate::canonical_launch_projection::CanonicalLaunchProjection;

pub const MANAGED_ACCOUNT_HINT_ENV_VAR: &str = "CODEX_MANAGED_ACCOUNT_ID";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaunchOperation {
    NewThread,
    ExistingThread,
    PassiveLocal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestedLaunchMode {
    StandardLocal,
    ExplicitEmbedded,
    ExplicitRemote,
    WorkloadIdentity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvocationOverrides {
    None,
    CanonicalSafe,
    EmbeddedOnly,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaunchDisposition {
    UnmanagedUpstream,
    CanonicalLocal,
    ExplicitEmbedded,
    ExplicitRemote,
    WorkloadIdentity,
}

struct LaunchClassification<'a> {
    managed_account_hint: Option<&'a str>,
    codex_home: &'a Path,
    operation: LaunchOperation,
    requested_mode: RequestedLaunchMode,
    overrides: InvocationOverrides,
}

fn classify_launch(
    input: LaunchClassification<'_>,
    catalog: &ManagedAccountCatalog,
) -> std::io::Result<LaunchDisposition> {
    let Some(hint) = input.managed_account_hint else {
        return Ok(LaunchDisposition::UnmanagedUpstream);
    };
    catalog
        .match_hint(hint, input.codex_home)
        .map_err(std::io::Error::other)?;

    match input.requested_mode {
        RequestedLaunchMode::ExplicitRemote => Ok(LaunchDisposition::ExplicitRemote),
        RequestedLaunchMode::WorkloadIdentity => Ok(LaunchDisposition::WorkloadIdentity),
        RequestedLaunchMode::ExplicitEmbedded => {
            if input.operation != LaunchOperation::NewThread {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "explicit embedded mode is limited to new threads; existing managed threads remain owned by the canonical app server",
                ));
            }
            Ok(LaunchDisposition::ExplicitEmbedded)
        }
        RequestedLaunchMode::StandardLocal => match input.overrides {
            InvocationOverrides::None | InvocationOverrides::CanonicalSafe => {
                Ok(LaunchDisposition::CanonicalLocal)
            }
            InvocationOverrides::EmbeddedOnly => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "this invocation-local configuration requires an explicit embedded new thread; add --embedded or remove the override",
            )),
        },
    }
}

pub fn classify_current_launch(
    codex_home: &Path,
    operation: LaunchOperation,
    requested_mode: RequestedLaunchMode,
    overrides: InvocationOverrides,
) -> std::io::Result<LaunchDisposition> {
    let managed_account_hint = current_managed_account_hint()?;
    if managed_account_hint.is_none() {
        return Ok(LaunchDisposition::UnmanagedUpstream);
    }
    let catalog = ManagedAccountCatalog::load().map_err(std::io::Error::other)?;
    classify_launch(
        LaunchClassification {
            managed_account_hint: managed_account_hint.as_deref(),
            codex_home,
            operation,
            requested_mode,
            overrides,
        },
        &catalog,
    )
}

pub(crate) fn current_managed_account_hint() -> std::io::Result<Option<String>> {
    std::env::var_os(MANAGED_ACCOUNT_HINT_ENV_VAR)
        .map(|value| {
            value.into_string().map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "managed account hint is malformed",
                )
            })
        })
        .transpose()
}

pub fn managed_account_hint_is_present() -> std::io::Result<bool> {
    Ok(current_managed_account_hint()?.is_some())
}

pub(crate) fn canonical_projection(
    cli: &Cli,
    parsed_overrides: &[(String, toml::Value)],
    loader_overrides: &LoaderOverrides,
) -> std::io::Result<CanonicalLaunchProjection> {
    if cli.strict_config
        || cli.config_profile_v2.is_some()
        || cli.oss_provider.is_some()
        || !loader_overrides.is_default_for_canonical_launch()
        || parsed_overrides
            .iter()
            .any(|(key, _)| !canonical_safe_config_key(key))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "this invocation-local configuration requires an explicit embedded new thread; add --embedded or remove the override",
        ));
    }
    Ok(CanonicalLaunchProjection::from_invocation(
        cli,
        parsed_overrides,
    ))
}

pub(crate) fn invocation_overrides(
    cli: &Cli,
    parsed_overrides: &[(String, toml::Value)],
    loader_overrides: &LoaderOverrides,
) -> InvocationOverrides {
    match canonical_projection(cli, parsed_overrides, loader_overrides) {
        Ok(projection) if projection.has_explicit_overrides() => InvocationOverrides::CanonicalSafe,
        Ok(_) => InvocationOverrides::None,
        Err(_) => InvocationOverrides::EmbeddedOnly,
    }
}

fn canonical_safe_config_key(key: &str) -> bool {
    matches!(
        key,
        "model"
            | "model_reasoning_effort"
            | "model_reasoning_summary"
            | "model_verbosity"
            | "personality"
            | "web_search"
            | "service_tier"
            | "approval_policy"
            | "approvals_reviewer"
            | "sandbox_mode"
    )
}

#[cfg(test)]
#[path = "launch_overrides_tests.rs"]
mod tests;
