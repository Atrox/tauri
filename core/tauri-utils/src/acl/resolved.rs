// Copyright 2019-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

//! Resolved ACL for runtime usage.

use std::{
  collections::{hash_map::DefaultHasher, BTreeMap, HashSet},
  fmt,
  hash::{Hash, Hasher},
};

use glob::Pattern;

use crate::platform::Target;

use super::{
  capability::{Capability, CapabilityContext, PermissionEntry},
  plugin::Manifest,
  Error, ExecutionContext, Permission, PermissionSet, Scopes, Value,
};

/// A key for a scope, used to link a [`ResolvedCommand#structfield.scope`] to the store [`Resolved#structfield.scopes`].
pub type ScopeKey = usize;

/// Metadata for what referenced a [`ResolvedCommand`].
#[cfg(debug_assertions)]
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct ResolvedCommandReference {
  /// Identifier of the capability.
  pub capability: String,
  /// Identifier of the permission.
  pub permission: String,
}

/// A resolved command permission.
#[derive(Default, Clone, PartialEq, Eq)]
pub struct ResolvedCommand {
  /// The list of capability/permission that referenced this command.
  #[cfg(debug_assertions)]
  pub referenced_by: Vec<ResolvedCommandReference>,
  /// The list of window label patterns that was resolved for this command.
  pub windows: Vec<glob::Pattern>,
  /// The reference of the scope that is associated with this command. See [`Resolved#structfield.scopes`].
  pub scope: Option<ScopeKey>,
}

impl fmt::Debug for ResolvedCommand {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("ResolvedCommand")
      .field("windows", &self.windows)
      .field("scope", &self.scope)
      .finish()
  }
}

/// A resolved scope. Merges all scopes defined for a single command.
#[derive(Debug, Default)]
pub struct ResolvedScope {
  /// Allows something on the command.
  pub allow: Vec<Value>,
  /// Denies something on the command.
  pub deny: Vec<Value>,
}

/// A command key for the map of allowed and denied commands.
/// Takes into consideration the command name and the execution context.
#[derive(Debug, Clone, Eq, PartialEq, PartialOrd, Ord, Hash)]
pub struct CommandKey {
  /// The full command name.
  pub name: String,
  /// The context of the command.
  pub context: ExecutionContext,
}

/// Resolved access control list.
#[derive(Default)]
pub struct Resolved {
  /// ACL plugin manifests.
  #[cfg(debug_assertions)]
  pub acl: BTreeMap<String, Manifest>,
  /// The commands that are allowed. Map each command with its context to a [`ResolvedCommand`].
  pub allowed_commands: BTreeMap<CommandKey, ResolvedCommand>,
  /// The commands that are denied. Map each command with its context to a [`ResolvedCommand`].
  pub denied_commands: BTreeMap<CommandKey, ResolvedCommand>,
  /// The store of scopes referenced by a [`ResolvedCommand`].
  pub command_scope: BTreeMap<ScopeKey, ResolvedScope>,
  /// The global scope.
  pub global_scope: BTreeMap<String, ResolvedScope>,
}

impl fmt::Debug for Resolved {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("Resolved")
      .field("allowed_commands", &self.allowed_commands)
      .field("denied_commands", &self.denied_commands)
      .field("command_scope", &self.command_scope)
      .field("global_scope", &self.global_scope)
      .finish()
  }
}

impl Resolved {
  /// Resolves the ACL for the given plugin permissions and app capabilities.
  pub fn resolve(
    acl: BTreeMap<String, Manifest>,
    capabilities: BTreeMap<String, Capability>,
    target: Target,
  ) -> Result<Self, Error> {
    let mut allowed_commands = BTreeMap::new();
    let mut denied_commands = BTreeMap::new();

    let mut current_scope_id = 0;
    let mut command_scopes = BTreeMap::new();
    let mut global_scope: BTreeMap<String, Vec<Scopes>> = BTreeMap::new();

    // resolve commands
    for capability in capabilities.values() {
      if !capability.platforms.contains(&target) {
        continue;
      }

      for permission_entry in &capability.permissions {
        let permission_id = permission_entry.identifier();
        let permission_name = permission_id.get_base();

        if let Some(plugin_name) = permission_id.get_prefix() {
          let permissions = get_permissions(plugin_name, permission_name, &acl)?;

          for permission in permissions {
            let scope = match permission_entry {
              PermissionEntry::PermissionRef(_) => permission.scope.clone(),
              PermissionEntry::ExtendedPermission {
                identifier: _,
                scope,
              } => {
                let mut merged = permission.scope.clone();
                if let Some(allow) = scope.allow.clone() {
                  merged
                    .allow
                    .get_or_insert_with(Default::default)
                    .extend(allow);
                }
                if let Some(deny) = scope.deny.clone() {
                  merged
                    .deny
                    .get_or_insert_with(Default::default)
                    .extend(deny);
                }
                merged
              }
            };

            if permission.commands.allow.is_empty() && permission.commands.deny.is_empty() {
              // global scope
              global_scope
                .entry(plugin_name.to_string())
                .or_default()
                .push(scope.clone());
            } else {
              let has_scope = scope.allow.is_some() || scope.deny.is_some();
              if has_scope {
                current_scope_id += 1;
                command_scopes.insert(current_scope_id, scope.clone());
              }

              let scope_id = if has_scope {
                Some(current_scope_id)
              } else {
                None
              };

              for allowed_command in &permission.commands.allow {
                resolve_command(
                  &mut allowed_commands,
                  format!("plugin:{plugin_name}|{allowed_command}"),
                  capability,
                  scope_id,
                  #[cfg(debug_assertions)]
                  permission,
                );
              }

              for denied_command in &permission.commands.deny {
                resolve_command(
                  &mut denied_commands,
                  format!("plugin:{plugin_name}|{denied_command}"),
                  capability,
                  scope_id,
                  #[cfg(debug_assertions)]
                  permission,
                );
              }
            }
          }
        }
      }
    }

    // resolve scopes
    let mut resolved_scopes = BTreeMap::new();

    for allowed in allowed_commands.values_mut() {
      if !allowed.scope.is_empty() {
        allowed.scope.sort();

        let mut hasher = DefaultHasher::new();
        allowed.scope.hash(&mut hasher);
        let hash = hasher.finish() as usize;

        allowed.resolved_scope_key.replace(hash);

        let resolved_scope = ResolvedScope {
          allow: allowed
            .scope
            .iter()
            .flat_map(|s| command_scopes.get(s).unwrap().allow.clone())
            .flatten()
            .collect(),
          deny: allowed
            .scope
            .iter()
            .flat_map(|s| command_scopes.get(s).unwrap().deny.clone())
            .flatten()
            .collect(),
        };

        resolved_scopes.insert(hash, resolved_scope);
      }
    }

    let global_scope = global_scope
      .into_iter()
      .map(|(plugin_name, scopes)| {
        let mut resolved_scope = ResolvedScope::default();
        for scope in scopes {
          if let Some(allow) = scope.allow {
            resolved_scope.allow.extend(allow);
          }
          if let Some(deny) = scope.deny {
            resolved_scope.deny.extend(deny);
          }
        }
        (plugin_name, resolved_scope)
      })
      .collect();

    let resolved = Self {
      #[cfg(debug_assertions)]
      acl,
      allowed_commands: allowed_commands
        .into_iter()
        .map(|(key, cmd)| {
          Ok((
            key,
            ResolvedCommand {
              #[cfg(debug_assertions)]
              referenced_by: cmd.referenced_by,
              windows: parse_window_patterns(cmd.windows)?,
              scope: cmd.resolved_scope_key,
            },
          ))
        })
        .collect::<Result<_, Error>>()?,
      denied_commands: denied_commands
        .into_iter()
        .map(|(key, cmd)| {
          Ok((
            key,
            ResolvedCommand {
              #[cfg(debug_assertions)]
              referenced_by: cmd.referenced_by,
              windows: parse_window_patterns(cmd.windows)?,
              scope: cmd.resolved_scope_key,
            },
          ))
        })
        .collect::<Result<_, Error>>()?,
      command_scope: resolved_scopes,
      global_scope,
    };

    Ok(resolved)
  }
}

fn parse_window_patterns(windows: HashSet<String>) -> Result<Vec<glob::Pattern>, Error> {
  let mut patterns = Vec::new();
  for window in windows {
    patterns.push(glob::Pattern::new(&window)?);
  }
  Ok(patterns)
}

#[derive(Debug, Default)]
struct ResolvedCommandTemp {
  #[cfg(debug_assertions)]
  pub referenced_by: Vec<ResolvedCommandReference>,
  pub windows: HashSet<String>,
  pub scope: Vec<usize>,
  pub resolved_scope_key: Option<usize>,
}

fn resolve_command(
  commands: &mut BTreeMap<CommandKey, ResolvedCommandTemp>,
  command: String,
  capability: &Capability,
  scope_id: Option<usize>,
  #[cfg(debug_assertions)] permission: &Permission,
) {
  let contexts = match &capability.context {
    CapabilityContext::Local => {
      vec![ExecutionContext::Local]
    }
    CapabilityContext::Remote { domains } => domains
      .iter()
      .map(|domain| ExecutionContext::Remote {
        domain: Pattern::new(domain)
          .unwrap_or_else(|e| panic!("invalid glob pattern for remote domain {domain}: {e}")),
      })
      .collect(),
  };

  for context in contexts {
    let resolved = commands
      .entry(CommandKey {
        name: command.clone(),
        context,
      })
      .or_default();

    #[cfg(debug_assertions)]
    resolved.referenced_by.push(ResolvedCommandReference {
      capability: capability.identifier.clone(),
      permission: permission.identifier.clone(),
    });

    resolved.windows.extend(capability.windows.clone());
    if let Some(id) = scope_id {
      resolved.scope.push(id);
    }
  }
}

// get the permissions from a permission set
fn get_permission_set_permissions<'a>(
  manifest: &'a Manifest,
  set: &'a PermissionSet,
) -> Result<Vec<&'a Permission>, Error> {
  let mut permissions = Vec::new();

  for p in &set.permissions {
    if let Some(permission) = manifest.permissions.get(p) {
      permissions.push(permission);
    } else if let Some(permission_set) = manifest.permission_sets.get(p) {
      permissions.extend(get_permission_set_permissions(manifest, permission_set)?);
    } else {
      return Err(Error::SetPermissionNotFound {
        permission: p.to_string(),
        set: set.identifier.clone(),
      });
    }
  }

  Ok(permissions)
}

fn get_permissions<'a>(
  plugin_name: &'a str,
  permission_name: &'a str,
  acl: &'a BTreeMap<String, Manifest>,
) -> Result<Vec<&'a Permission>, Error> {
  let manifest = acl.get(plugin_name).ok_or_else(|| Error::UnknownPlugin {
    plugin: plugin_name.to_string(),
    available: acl.keys().cloned().collect::<Vec<_>>().join(", "),
  })?;

  if permission_name == "default" {
    manifest
      .default_permission
      .as_ref()
      .ok_or_else(|| Error::UnknownPermission {
        plugin: plugin_name.to_string(),
        permission: permission_name.to_string(),
      })
      .and_then(|default| get_permission_set_permissions(manifest, default))
  } else if let Some(set) = manifest.permission_sets.get(permission_name) {
    get_permission_set_permissions(manifest, set)
  } else if let Some(permission) = manifest.permissions.get(permission_name) {
    Ok(vec![permission])
  } else {
    Err(Error::UnknownPermission {
      plugin: plugin_name.to_string(),
      permission: permission_name.to_string(),
    })
  }
}

#[cfg(feature = "build")]
mod build {
  use proc_macro2::TokenStream;
  use quote::{quote, ToTokens, TokenStreamExt};
  use std::convert::identity;

  use super::*;
  use crate::{literal_struct, tokens::*};

  impl ToTokens for CommandKey {
    fn to_tokens(&self, tokens: &mut TokenStream) {
      let name = str_lit(&self.name);
      let context = &self.context;
      literal_struct!(
        tokens,
        ::tauri::utils::acl::resolved::CommandKey,
        name,
        context
      )
    }
  }

  #[cfg(debug_assertions)]
  impl ToTokens for ResolvedCommandReference {
    fn to_tokens(&self, tokens: &mut TokenStream) {
      let capability = str_lit(&self.capability);
      let permission = str_lit(&self.permission);
      literal_struct!(
        tokens,
        ::tauri::utils::acl::resolved::ResolvedCommandReference,
        capability,
        permission
      )
    }
  }

  impl ToTokens for ResolvedCommand {
    fn to_tokens(&self, tokens: &mut TokenStream) {
      #[cfg(debug_assertions)]
      let referenced_by = vec_lit(&self.referenced_by, identity);

      let windows = vec_lit(&self.windows, |window| {
        let w = window.as_str();
        quote!(#w.parse().unwrap())
      });
      let scope = opt_lit(self.scope.as_ref());

      #[cfg(debug_assertions)]
      {
        literal_struct!(
          tokens,
          ::tauri::utils::acl::resolved::ResolvedCommand,
          referenced_by,
          windows,
          scope
        )
      }
      #[cfg(not(debug_assertions))]
      literal_struct!(
        tokens,
        ::tauri::utils::acl::resolved::ResolvedCommand,
        windows,
        scope
      )
    }
  }

  impl ToTokens for ResolvedScope {
    fn to_tokens(&self, tokens: &mut TokenStream) {
      let allow = vec_lit(&self.allow, identity);
      let deny = vec_lit(&self.deny, identity);
      literal_struct!(
        tokens,
        ::tauri::utils::acl::resolved::ResolvedScope,
        allow,
        deny
      )
    }
  }

  impl ToTokens for Resolved {
    fn to_tokens(&self, tokens: &mut TokenStream) {
      #[cfg(debug_assertions)]
      let acl = map_lit(
        quote! { ::std::collections::BTreeMap },
        &self.acl,
        str_lit,
        identity,
      );

      let allowed_commands = map_lit(
        quote! { ::std::collections::BTreeMap },
        &self.allowed_commands,
        identity,
        identity,
      );

      let denied_commands = map_lit(
        quote! { ::std::collections::BTreeMap },
        &self.denied_commands,
        identity,
        identity,
      );

      let command_scope = map_lit(
        quote! { ::std::collections::BTreeMap },
        &self.command_scope,
        identity,
        identity,
      );

      let global_scope = map_lit(
        quote! { ::std::collections::BTreeMap },
        &self.global_scope,
        str_lit,
        identity,
      );

      #[cfg(debug_assertions)]
      {
        literal_struct!(
          tokens,
          ::tauri::utils::acl::resolved::Resolved,
          acl,
          allowed_commands,
          denied_commands,
          command_scope,
          global_scope
        )
      }
      #[cfg(not(debug_assertions))]
      literal_struct!(
        tokens,
        ::tauri::utils::acl::resolved::Resolved,
        allowed_commands,
        denied_commands,
        command_scope,
        global_scope
      )
    }
  }
}
