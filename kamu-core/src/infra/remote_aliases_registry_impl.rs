// Copyright Kamu Data, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use super::*;
use crate::domain::*;
use opendatafabric::*;

use dill::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;

////////////////////////////////////////////////////////////////////////////////////////

#[derive(Clone)]
pub struct RemoteAliasesRegistryImpl {
    dataset_reg: Arc<dyn DatasetRegistry>,
    workspace_layout: Arc<WorkspaceLayout>,
}

////////////////////////////////////////////////////////////////////////////////////////

#[component(pub)]
impl RemoteAliasesRegistryImpl {
    pub fn new(
        dataset_reg: Arc<dyn DatasetRegistry>,
        workspace_layout: Arc<WorkspaceLayout>,
    ) -> Self {
        Self {
            dataset_reg,
            workspace_layout,
        }
    }

    fn get_dataset_metadata_dir(&self, name: &DatasetName) -> PathBuf {
        self.workspace_layout.datasets_dir.join(name)
    }

    fn read_config(&self, path: &Path) -> Result<DatasetConfig, DomainError> {
        let file = std::fs::File::open(&path).unwrap_or_else(|e| {
            panic!(
                "Failed to open the config file at {}: {}",
                path.display(),
                e
            )
        });

        let manifest: Manifest<DatasetConfig> =
            serde_yaml::from_reader(&file).unwrap_or_else(|e| {
                panic!(
                    "Failed to deserialize the DatasetConfig at {}: {}",
                    path.display(),
                    e
                )
            });

        assert_eq!(manifest.kind, "DatasetConfig");
        Ok(manifest.content)
    }

    fn write_config(&self, path: &Path, config: DatasetConfig) -> Result<(), DomainError> {
        let manifest = Manifest {
            kind: "DatasetConfig".to_owned(),
            version: 1,
            content: config,
        };
        let file = std::fs::File::create(&path).map_err(|e| InfraError::from(e).into())?;
        serde_yaml::to_writer(file, &manifest).map_err(|e| InfraError::from(e).into())?;
        Ok(())
    }

    fn get_config(&self, dataset_name: &DatasetName) -> Result<DatasetConfig, DomainError> {
        let path = self.get_dataset_metadata_dir(dataset_name).join("config");

        if path.exists() {
            self.read_config(&path)
        } else {
            Ok(DatasetConfig::default())
        }
    }

    fn set_config(
        &self,
        dataset_name: &DatasetName,
        config: DatasetConfig,
    ) -> Result<(), DomainError> {
        let path = self.get_dataset_metadata_dir(dataset_name).join("config");
        self.write_config(&path, config)
    }
}

////////////////////////////////////////////////////////////////////////////////////////

impl RemoteAliasesRegistry for RemoteAliasesRegistryImpl {
    fn get_remote_aliases(
        &self,
        dataset_ref: &DatasetRefLocal,
    ) -> Result<Box<dyn RemoteAliases>, DomainError> {
        let hdl = self.dataset_reg.resolve_dataset_ref(dataset_ref)?;
        let config = self.get_config(&hdl.name)?;
        Ok(Box::new(RemoteAliasesImpl::new(self.clone(), hdl, config)))
    }
}

////////////////////////////////////////////////////////////////////////////////////////
// RemoteAliasesImpl
////////////////////////////////////////////////////////////////////////////////////////

struct RemoteAliasesImpl {
    alias_registry: RemoteAliasesRegistryImpl,
    dataset_handle: DatasetHandle,
    config: DatasetConfig,
}

impl RemoteAliasesImpl {
    fn new(
        alias_registry: RemoteAliasesRegistryImpl,
        dataset_handle: DatasetHandle,
        config: DatasetConfig,
    ) -> Self {
        Self {
            alias_registry,
            dataset_handle,
            config,
        }
    }
}

impl RemoteAliases for RemoteAliasesImpl {
    fn get_by_kind<'a>(
        &'a self,
        kind: RemoteAliasKind,
    ) -> Box<dyn Iterator<Item = &'a RemoteDatasetName> + 'a> {
        let aliases = match kind {
            RemoteAliasKind::Pull => &self.config.pull_aliases,
            RemoteAliasKind::Push => &self.config.push_aliases,
        };
        Box::new(aliases.iter())
    }

    fn contains(&self, remote_ref: &RemoteDatasetName, kind: RemoteAliasKind) -> bool {
        let aliases = match kind {
            RemoteAliasKind::Pull => &self.config.pull_aliases,
            RemoteAliasKind::Push => &self.config.push_aliases,
        };
        for a in aliases {
            if *a == *remote_ref {
                return true;
            }
        }
        false
    }

    fn is_empty(&self, kind: RemoteAliasKind) -> bool {
        let aliases = match kind {
            RemoteAliasKind::Pull => &self.config.pull_aliases,
            RemoteAliasKind::Push => &self.config.push_aliases,
        };
        aliases.is_empty()
    }

    fn add(
        &mut self,
        remote_ref: &RemoteDatasetName,
        kind: RemoteAliasKind,
    ) -> Result<bool, DomainError> {
        let aliases = match kind {
            RemoteAliasKind::Pull => &mut self.config.pull_aliases,
            RemoteAliasKind::Push => &mut self.config.push_aliases,
        };

        let remote_ref = remote_ref.to_owned();
        if !aliases.contains(&remote_ref) {
            aliases.push(remote_ref);
            self.alias_registry
                .set_config(&self.dataset_handle.name, self.config.clone())?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn delete(
        &mut self,
        remote_ref: &RemoteDatasetName,
        kind: RemoteAliasKind,
    ) -> Result<bool, DomainError> {
        let aliases = match kind {
            RemoteAliasKind::Pull => &mut self.config.pull_aliases,
            RemoteAliasKind::Push => &mut self.config.push_aliases,
        };

        if let Some(i) = aliases.iter().position(|r| *r == *remote_ref) {
            aliases.remove(i);
            self.alias_registry
                .set_config(&self.dataset_handle.name, self.config.clone())?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn clear(&mut self, kind: RemoteAliasKind) -> Result<usize, DomainError> {
        let aliases = match kind {
            RemoteAliasKind::Pull => &mut self.config.pull_aliases,
            RemoteAliasKind::Push => &mut self.config.push_aliases,
        };
        let len = aliases.len();
        if !aliases.is_empty() {
            aliases.clear();
            self.alias_registry
                .set_config(&self.dataset_handle.name, self.config.clone())?;
        }
        Ok(len)
    }
}

////////////////////////////////////////////////////////////////////////////////////////
// Null
////////////////////////////////////////////////////////////////////////////////////////

pub struct RemoteAliasesRegistryNull;

impl RemoteAliasesRegistry for RemoteAliasesRegistryNull {
    fn get_remote_aliases(
        &self,
        dataset_ref: &DatasetRefLocal,
    ) -> Result<Box<dyn RemoteAliases>, DomainError> {
        Err(DomainError::does_not_exist(
            ResourceKind::Dataset,
            dataset_ref.to_string(),
        ))
    }
}
