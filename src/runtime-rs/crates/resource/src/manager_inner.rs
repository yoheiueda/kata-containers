// Copyright (c) 2019-2022 Alibaba Cloud
// Copyright (c) 2019-2022 Ant Group
//
// SPDX-License-Identifier: Apache-2.0
//

use std::{sync::Arc, thread, vec};

use crate::{network::NetworkConfig, resource_persist::ResourceState};
use agent::{types::Device, Agent, Storage};
use anyhow::{anyhow, Context, Ok, Result};
use async_trait::async_trait;

use hypervisor::{
    device::{
        device_manager::{do_handle_device, DeviceManager},
        DeviceConfig, DeviceType,
    },
    BlockConfig, Hypervisor,
};
use kata_types::config::TomlConfig;
use kata_types::mount::Mount;
use oci::{Linux, LinuxResources};
use persist::sandbox_persist::Persist;
use tokio::{runtime, sync::RwLock};

use crate::{
    cgroups::{CgroupArgs, CgroupsResource},
    manager::ManagerArgs,
    network::{self, Network},
    rootfs::{RootFsResource, Rootfs},
    share_fs::{self, sandbox_bind_mounts::SandboxBindMounts, ShareFs},
    volume::{Volume, VolumeResource},
    ResourceConfig,
};

pub(crate) struct ResourceManagerInner {
    sid: String,
    toml_config: Arc<TomlConfig>,
    agent: Arc<dyn Agent>,
    hypervisor: Arc<dyn Hypervisor>,
    device_manager: Arc<RwLock<DeviceManager>>,
    network: Option<Arc<dyn Network>>,
    share_fs: Option<Arc<dyn ShareFs>>,

    pub rootfs_resource: RootFsResource,
    pub volume_resource: VolumeResource,
    pub cgroups_resource: CgroupsResource,
}

impl ResourceManagerInner {
    pub(crate) fn new(
        sid: &str,
        agent: Arc<dyn Agent>,
        hypervisor: Arc<dyn Hypervisor>,
        toml_config: Arc<TomlConfig>,
    ) -> Result<Self> {
        let cgroups_resource = CgroupsResource::new(sid, &toml_config)?;

        // create device manager
        let dev_manager =
            DeviceManager::new(hypervisor.clone()).context("failed to create device manager")?;

        Ok(Self {
            sid: sid.to_string(),
            toml_config,
            agent,
            hypervisor,
            device_manager: Arc::new(RwLock::new(dev_manager)),
            network: None,
            share_fs: None,
            rootfs_resource: RootFsResource::new(),
            volume_resource: VolumeResource::new(),
            cgroups_resource,
        })
    }

    pub fn config(&self) -> Arc<TomlConfig> {
        self.toml_config.clone()
    }

    pub fn get_device_manager(&self) -> Arc<RwLock<DeviceManager>> {
        self.device_manager.clone()
    }

    pub async fn prepare_before_start_vm(
        &mut self,
        device_configs: Vec<ResourceConfig>,
    ) -> Result<()> {
        for dc in device_configs {
            match dc {
                ResourceConfig::ShareFs(c) => {
                    self.share_fs = if self
                        .hypervisor
                        .capabilities()
                        .await?
                        .is_fs_sharing_supported()
                    {
                        let share_fs = share_fs::new(&self.sid, &c).context("new share fs")?;
                        share_fs
                            .setup_device_before_start_vm(self.hypervisor.as_ref())
                            .await
                            .context("setup share fs device before start vm")?;

                        // setup sandbox bind mounts: setup = true
                        self.handle_sandbox_bindmounts(true)
                            .await
                            .context("failed setup sandbox bindmounts")?;

                        Some(share_fs)
                    } else {
                        None
                    };
                }
                ResourceConfig::Network(c) => {
                    self.handle_network(c)
                        .await
                        .context("failed to handle network")?;
                }
            };
        }

        Ok(())
    }

    pub async fn handle_network(&mut self, network_config: NetworkConfig) -> Result<()> {
        // 1. When using Rust asynchronous programming, we use .await to
        //    allow other task to run instead of waiting for the completion of the current task.
        // 2. Also, when handling the pod network, we need to set the shim threads
        //    into the network namespace to perform those operations.
        // However, as the increase of the I/O intensive tasks, two issues could be caused by the two points above:
        // a. When the future is blocked, the current thread (which is in the pod netns)
        //    might be take over by other tasks. After the future is finished, the thread take over
        //    the current task might not be in the pod netns. But the current task still need to run in pod netns
        // b. When finish setting up the network, the current thread will be set back to the host namespace.
        //    In Rust Async, if the current thread is taken over by other task, the netns is dropped on another thread,
        //    but it is not in netns. So, the previous thread would still remain in the pod netns.
        // The solution is to block the future on the current thread, it is enabled by spawn an os thread, create a
        // tokio runtime, and block the task on it.
        let hypervisor = self.hypervisor.clone();
        let network = thread::spawn(move || -> Result<Arc<dyn Network>> {
            let rt = runtime::Builder::new_current_thread().enable_io().build()?;
            let d = rt
                .block_on(network::new(&network_config))
                .context("new network")?;
            rt.block_on(d.setup(hypervisor.as_ref()))
                .context("setup network")?;
            Ok(d)
        })
        .join()
        .map_err(|e| anyhow!("{:?}", e))
        .context("Couldn't join on the associated thread")?
        .context("failed to set up network")?;
        self.network = Some(network);
        Ok(())
    }

    async fn handle_interfaces(&self, network: &dyn Network) -> Result<()> {
        for i in network.interfaces().await.context("get interfaces")? {
            // update interface
            info!(sl!(), "update interface {:?}", i);
            self.agent
                .update_interface(agent::UpdateInterfaceRequest { interface: Some(i) })
                .await
                .context("update interface")?;
        }

        Ok(())
    }

    async fn handle_neighbours(&self, network: &dyn Network) -> Result<()> {
        let neighbors = network.neighs().await.context("neighs")?;
        if !neighbors.is_empty() {
            info!(sl!(), "update neighbors {:?}", neighbors);
            self.agent
                .add_arp_neighbors(agent::AddArpNeighborRequest {
                    neighbors: Some(agent::ARPNeighbors { neighbors }),
                })
                .await
                .context("update neighbors")?;
        }
        Ok(())
    }

    async fn handle_routes(&self, network: &dyn Network) -> Result<()> {
        let routes = network.routes().await.context("routes")?;
        if !routes.is_empty() {
            info!(sl!(), "update routes {:?}", routes);
            self.agent
                .update_routes(agent::UpdateRoutesRequest {
                    route: Some(agent::Routes { routes }),
                })
                .await
                .context("update routes")?;
        }
        Ok(())
    }

    pub async fn setup_after_start_vm(&mut self) -> Result<()> {
        if let Some(share_fs) = self.share_fs.as_ref() {
            share_fs
                .setup_device_after_start_vm(self.hypervisor.as_ref())
                .await
                .context("setup share fs device after start vm")?;
        }

        if let Some(network) = self.network.as_ref() {
            let network = network.as_ref();
            self.handle_interfaces(network)
                .await
                .context("handle interfaces")?;
            self.handle_neighbours(network)
                .await
                .context("handle neighbors")?;
            self.handle_routes(network).await.context("handle routes")?;
        }
        Ok(())
    }

    pub async fn get_storage_for_sandbox(&self) -> Result<Vec<Storage>> {
        let mut storages = vec![];
        if let Some(d) = self.share_fs.as_ref() {
            let mut s = d.get_storages().await.context("get storage")?;
            storages.append(&mut s);
        }
        Ok(storages)
    }

    pub async fn handler_rootfs(
        &self,
        cid: &str,
        root: &oci::Root,
        bundle_path: &str,
        rootfs_mounts: &[Mount],
    ) -> Result<Arc<dyn Rootfs>> {
        self.rootfs_resource
            .handler_rootfs(
                &self.share_fs,
                self.device_manager.as_ref(),
                self.hypervisor.as_ref(),
                &self.sid,
                cid,
                root,
                bundle_path,
                rootfs_mounts,
            )
            .await
    }

    pub async fn handler_volumes(
        &self,
        cid: &str,
        spec: &oci::Spec,
    ) -> Result<Vec<Arc<dyn Volume>>> {
        self.volume_resource
            .handler_volumes(
                &self.share_fs,
                cid,
                spec,
                self.device_manager.as_ref(),
                &self.sid,
                self.agent.clone(),
            )
            .await
    }

    pub async fn handler_devices(&self, _cid: &str, linux: &Linux) -> Result<Vec<Device>> {
        let mut devices = vec![];
        for d in linux.devices.iter() {
            match d.r#type.as_str() {
                "b" => {
                    let dev_info = DeviceConfig::BlockCfg(BlockConfig {
                        major: d.major,
                        minor: d.minor,
                        ..Default::default()
                    });

                    let device_info = do_handle_device(&self.device_manager, &dev_info)
                        .await
                        .context("do handle device")?;

                    // create agent device
                    if let DeviceType::Block(device) = device_info {
                        let agent_device = Device {
                            id: device.device_id.clone(),
                            container_path: d.path.clone(),
                            field_type: device.config.driver_option,
                            vm_path: device.config.virt_path,
                            ..Default::default()
                        };
                        devices.push(agent_device);
                    }
                }
                _ => {
                    // TODO enable other devices type
                    continue;
                }
            }
        }
        Ok(devices)
    }

    async fn handle_sandbox_bindmounts(&self, setup: bool) -> Result<()> {
        let bindmounts = self.toml_config.runtime.sandbox_bind_mounts.clone();
        if bindmounts.is_empty() {
            info!(sl!(), "sandbox bindmounts empty, just skip it.");
            return Ok(());
        }

        let sb_bindmnt = SandboxBindMounts::new(self.sid.clone(), bindmounts)?;

        if setup {
            sb_bindmnt.setup_sandbox_bind_mounts()
        } else {
            sb_bindmnt.cleanup_sandbox_bind_mounts()
        }
    }

    pub async fn update_cgroups(
        &self,
        cid: &str,
        linux_resources: Option<&LinuxResources>,
    ) -> Result<()> {
        self.cgroups_resource
            .update_cgroups(cid, linux_resources, self.hypervisor.as_ref())
            .await
    }

    pub async fn cleanup(&self) -> Result<()> {
        // clean up cgroup
        self.cgroups_resource
            .delete()
            .await
            .context("delete cgroup")?;

        // cleanup sandbox bind mounts: setup = false
        self.handle_sandbox_bindmounts(false)
            .await
            .context("failed to cleanup sandbox bindmounts")?;

        // clean up share fs mount
        if let Some(share_fs) = &self.share_fs {
            share_fs
                .get_share_fs_mount()
                .cleanup(&self.sid)
                .await
                .context("failed to cleanup host path")?;
        }
        // TODO cleanup other resources
        Ok(())
    }

    pub async fn dump(&self) {
        self.rootfs_resource.dump().await;
        self.volume_resource.dump().await;
    }
}

#[async_trait]
impl Persist for ResourceManagerInner {
    type State = ResourceState;
    type ConstructorArgs = ManagerArgs;

    /// Save a state of ResourceManagerInner
    async fn save(&self) -> Result<Self::State> {
        let mut endpoint_state = vec![];
        if let Some(network) = &self.network {
            if let Some(ens) = network.save().await {
                endpoint_state = ens;
            }
        }
        let cgroup_state = self.cgroups_resource.save().await?;
        Ok(ResourceState {
            endpoint: endpoint_state,
            cgroup_state: Some(cgroup_state),
        })
    }

    /// Restore ResourceManagerInner
    async fn restore(
        resource_args: Self::ConstructorArgs,
        resource_state: Self::State,
    ) -> Result<Self> {
        let args = CgroupArgs {
            sid: resource_args.sid.clone(),
            config: resource_args.config,
        };
        Ok(Self {
            sid: resource_args.sid,
            agent: resource_args.agent,
            hypervisor: resource_args.hypervisor.clone(),
            device_manager: Arc::new(RwLock::new(DeviceManager::new(resource_args.hypervisor)?)),
            network: None,
            share_fs: None,
            rootfs_resource: RootFsResource::new(),
            volume_resource: VolumeResource::new(),
            cgroups_resource: CgroupsResource::restore(
                args,
                resource_state.cgroup_state.unwrap_or_default(),
            )
            .await?,
            toml_config: Arc::new(TomlConfig::default()),
        })
    }
}
