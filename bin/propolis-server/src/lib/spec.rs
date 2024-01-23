// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Helper functions for building instance specs from server parameters.

use std::str::FromStr;

use crate::config;
use propolis_api_types::instance_spec::{
    components,
    v0::{
        builder::{SpecBuilder, SpecBuilderError},
        *,
    },
    PciPath,
};
use propolis_api_types::{
    self as api, DiskRequest, InstanceProperties, NetworkInterfaceRequest,
};
use thiserror::Error;

/// Errors that can occur while building an instance spec from component parts.
#[derive(Debug, Error)]
pub enum ServerSpecBuilderError {
    #[error(transparent)]
    InnerBuilderError(#[from] SpecBuilderError),

    #[error("The string {0} could not be converted to a PCI path")]
    PciPathNotParseable(String),

    #[error(
        "Could not translate PCI slot {0} for device type {1:?} to a PCI path"
    )]
    PciSlotInvalid(u8, SlotType),

    #[error("Unrecognized storage device interface {0}")]
    UnrecognizedStorageDevice(String),

    #[error("Unrecognized storage backend type {0}")]
    UnrecognizedStorageBackend(String),

    #[error("Device {0} requested missing backend {1}")]
    DeviceMissingBackend(String, String),

    #[error("Error in server config TOML: {0}")]
    ConfigTomlError(String),

    #[error("Error serializing {0} into spec element: {1}")]
    SerializationError(String, serde_json::error::Error),
}

/// A type of PCI device. Device numbers on the PCI bus are partitioned by slot
/// type. If a client asks to attach a device of type X to PCI slot Y, the
/// server will assign the Yth device number in X's partition. The partitioning
/// scheme is defined by the implementation of the `slot_to_pci_path` utility
/// function.
#[derive(Clone, Copy, Debug)]
pub enum SlotType {
    Nic,
    Disk,
    CloudInit,
}

/// Translates a device type and PCI slot (as presented in an instance creation
/// request) into a concrete PCI path. See the documentation for [`SlotType`].
pub(crate) fn slot_to_pci_path(
    slot: api::Slot,
    ty: SlotType,
) -> Result<PciPath, ServerSpecBuilderError> {
    match ty {
        // Slots for NICS: 0x08 -> 0x0F
        SlotType::Nic if slot.0 <= 7 => PciPath::new(0, slot.0 + 0x8, 0),
        // Slots for Disks: 0x10 -> 0x17
        SlotType::Disk if slot.0 <= 7 => PciPath::new(0, slot.0 + 0x10, 0),
        // Slot for CloudInit
        SlotType::CloudInit if slot.0 == 0 => PciPath::new(0, slot.0 + 0x18, 0),
        _ => return Err(ServerSpecBuilderError::PciSlotInvalid(slot.0, ty)),
    }
    .map_err(|_| ServerSpecBuilderError::PciSlotInvalid(slot.0, ty))
}

/// Generates NIC device and backend names from the NIC's PCI path. This is
/// needed because the `name` field in a propolis-client
/// `NetworkInterfaceRequest` is actually the name of the host vNIC to bind to,
/// and that can change between incarnations of an instance. The PCI path is
/// unique to each NIC but must remain stable over a migration, so it's suitable
/// for use in this naming scheme.
///
/// N.B. Migrating a NIC requires the source and target to agree on these names,
///      so changing this routine's behavior will prevent Propolis processes
///      with the old behavior from migrating processes with the new behavior.
fn pci_path_to_nic_names(path: PciPath) -> (String, String) {
    (format!("vnic-{}", path), format!("vnic-{}-backend", path))
}

fn make_storage_backend_from_config(
    name: &str,
    backend: &config::BlockDevice,
) -> Result<StorageBackendV0, ServerSpecBuilderError> {
    let backend_spec = match backend.bdtype.as_str() {
        "file" => {
            StorageBackendV0::File(components::backends::FileStorageBackend {
                path: backend
                    .options
                    .get("path")
                    .ok_or_else(|| {
                        ServerSpecBuilderError::ConfigTomlError(format!(
                            "Couldn't get path for file backend {}",
                            name
                        ))
                    })?
                    .as_str()
                    .ok_or_else(|| {
                        ServerSpecBuilderError::ConfigTomlError(format!(
                            "Couldn't parse path for file backend {}",
                            name
                        ))
                    })?
                    .to_string(),
                readonly: match backend.options.get("readonly") {
                    Some(toml::Value::Boolean(ro)) => Some(*ro),
                    Some(toml::Value::String(v)) => v.parse().ok(),
                    _ => None,
                }
                .unwrap_or(false),
            })
        }
        _ => {
            return Err(ServerSpecBuilderError::UnrecognizedStorageBackend(
                backend.bdtype.clone(),
            ));
        }
    };

    Ok(backend_spec)
}

fn make_storage_device_from_config(
    name: &str,
    device: &config::Device,
) -> Result<StorageDeviceV0, ServerSpecBuilderError> {
    enum DeviceInterface {
        Virtio,
        Nvme,
    }

    let interface = match device.driver.as_str() {
        "pci-virtio-block" => DeviceInterface::Virtio,
        "pci-nvme" => DeviceInterface::Nvme,
        _ => {
            return Err(ServerSpecBuilderError::ConfigTomlError(format!(
                "storage device {} has invalid driver {}",
                name, device.driver
            )))
        }
    };

    let backend_name = device
        .options
        .get("block_dev")
        .ok_or_else(|| {
            ServerSpecBuilderError::ConfigTomlError(format!(
                "Couldn't get block_dev for storage device {}",
                name
            ))
        })?
        .as_str()
        .ok_or_else(|| {
            ServerSpecBuilderError::ConfigTomlError(format!(
                "Couldn't parse block_dev for storage device {}",
                name
            ))
        })?
        .to_owned();

    let pci_path: PciPath = device.get("pci-path").ok_or_else(|| {
        ServerSpecBuilderError::ConfigTomlError(format!(
            "Failed to get PCI path for storage device {}",
            name
        ))
    })?;

    Ok(match interface {
        DeviceInterface::Virtio => {
            StorageDeviceV0::VirtioDisk(components::devices::VirtioDisk {
                backend_name,
                pci_path,
            })
        }
        DeviceInterface::Nvme => {
            StorageDeviceV0::NvmeDisk(components::devices::NvmeDisk {
                backend_name,
                pci_path,
            })
        }
    })
}

/// A helper for building instance specs out of component parts.
pub struct ServerSpecBuilder {
    builder: SpecBuilder,
}

impl ServerSpecBuilder {
    /// Creates a new spec builder from an instance's properties (supplied via
    /// the instance APIs) and the config TOML supplied at server startup.
    pub fn new(
        properties: &InstanceProperties,
        config: &config::Config,
    ) -> Result<Self, ServerSpecBuilderError> {
        let enable_pcie =
            config.chipset.options.get("enable-pcie").map_or_else(
                || Ok(false),
                |v| {
                    v.as_bool().ok_or_else(|| {
                        ServerSpecBuilderError::ConfigTomlError(format!(
                            "Invalid value {} for enable-pcie flag in chipset",
                            v
                        ))
                    })
                },
            )?;

        let mut builder =
            SpecBuilder::new(properties.vcpus, properties.memory, enable_pcie);

        builder.add_pvpanic_device(components::devices::QemuPvpanic {
            enable_isa: true,
        })?;

        Ok(Self { builder })
    }

    /// Converts an HTTP API request to add a NIC to an instance into
    /// device/backend entries in the spec under construction.
    pub fn add_nic_from_request(
        &mut self,
        nic: &NetworkInterfaceRequest,
    ) -> Result<(), ServerSpecBuilderError> {
        let pci_path = slot_to_pci_path(nic.slot, SlotType::Nic)?;
        let (device_name, backend_name) = pci_path_to_nic_names(pci_path);
        let device_spec =
            NetworkDeviceV0::VirtioNic(components::devices::VirtioNic {
                backend_name: backend_name.clone(),
                pci_path,
            });

        let backend_spec = NetworkBackendV0::Virtio(
            components::backends::VirtioNetworkBackend {
                vnic_name: nic.name.to_string(),
            },
        );

        self.builder.add_network_device(
            device_name,
            device_spec,
            backend_name,
            backend_spec,
        )?;

        Ok(())
    }

    /// Converts an HTTP API request to add a disk to an instance into
    /// device/backend entries in the spec under construction.
    pub fn add_disk_from_request(
        &mut self,
        disk: &DiskRequest,
    ) -> Result<(), ServerSpecBuilderError> {
        let pci_path = slot_to_pci_path(disk.slot, SlotType::Disk)?;
        let backend_name = disk.name.clone();

        let backend_spec = StorageBackendV0::Crucible(
            components::backends::CrucibleStorageBackend {
                request_json: serde_json::to_string(
                    &disk.volume_construction_request,
                )
                .map_err(|e| {
                    ServerSpecBuilderError::SerializationError(
                        disk.name.clone(),
                        e,
                    )
                })?,
                readonly: disk.read_only,
            },
        );

        let device_name = disk.name.clone();
        let device_spec = match disk.device.as_ref() {
            "virtio" => {
                StorageDeviceV0::VirtioDisk(components::devices::VirtioDisk {
                    backend_name: disk.name.to_string(),
                    pci_path,
                })
            }
            "nvme" => {
                StorageDeviceV0::NvmeDisk(components::devices::NvmeDisk {
                    backend_name: disk.name.to_string(),
                    pci_path,
                })
            }
            _ => {
                return Err(ServerSpecBuilderError::UnrecognizedStorageDevice(
                    disk.device.clone(),
                ))
            }
        };

        self.builder.add_storage_device(
            device_name,
            device_spec,
            backend_name,
            backend_spec,
        )?;

        Ok(())
    }

    /// Converts an HTTP API request to add a cloud-init disk to an instance
    /// into device/backend entries in the spec under construction.
    pub fn add_cloud_init_from_request(
        &mut self,
        base64: String,
    ) -> Result<(), ServerSpecBuilderError> {
        let name = "cloud-init";
        let pci_path = slot_to_pci_path(api::Slot(0), SlotType::CloudInit)?;
        let backend_name = name.to_string();
        let backend_spec =
            StorageBackendV0::Blob(components::backends::BlobStorageBackend {
                base64,
                readonly: true,
            });

        let device_name = name.to_string();
        let device_spec =
            StorageDeviceV0::VirtioDisk(components::devices::VirtioDisk {
                backend_name: name.to_string(),
                pci_path,
            });

        self.builder.add_storage_device(
            device_name,
            device_spec,
            backend_name,
            backend_spec,
        )?;

        Ok(())
    }

    fn add_network_device_from_config(
        &mut self,
        name: &str,
        device: &config::Device,
    ) -> Result<(), ServerSpecBuilderError> {
        let vnic_name = device.get_string("vnic").ok_or_else(|| {
            ServerSpecBuilderError::ConfigTomlError(format!(
                "Failed to get vNIC name for device {}",
                name
            ))
        })?;

        let pci_path: PciPath = device.get("pci-path").ok_or_else(|| {
            ServerSpecBuilderError::ConfigTomlError(format!(
                "Failed to get PCI path for network device {}",
                name
            ))
        })?;

        let (device_name, backend_name) = pci_path_to_nic_names(pci_path);
        let backend_spec = NetworkBackendV0::Virtio(
            components::backends::VirtioNetworkBackend {
                vnic_name: vnic_name.to_string(),
            },
        );

        let device_spec =
            NetworkDeviceV0::VirtioNic(components::devices::VirtioNic {
                backend_name: backend_name.clone(),
                pci_path,
            });

        self.builder.add_network_device(
            device_name,
            device_spec,
            backend_name,
            backend_spec,
        )?;

        Ok(())
    }

    fn add_pci_bridge_from_config(
        &mut self,
        bridge: &config::PciBridge,
    ) -> Result<(), ServerSpecBuilderError> {
        let name = format!("pci-bridge-{}", bridge.downstream_bus);
        let pci_path = PciPath::from_str(&bridge.pci_path).map_err(|_| {
            ServerSpecBuilderError::PciPathNotParseable(bridge.pci_path.clone())
        })?;

        self.builder.add_pci_bridge(
            name,
            components::devices::PciPciBridge {
                downstream_bus: bridge.downstream_bus,
                pci_path,
            },
        )?;

        Ok(())
    }

    /// Adds all the devices and backends specified in the supplied
    /// configuration TOML to the spec under construction.
    pub fn add_devices_from_config(
        &mut self,
        config: &config::Config,
    ) -> Result<(), ServerSpecBuilderError> {
        for (device_name, device) in config.devices.iter() {
            let driver = device.driver.as_str();
            match driver {
                // If this is a storage device, parse its "block_dev" property
                // to get the name of its corresponding backend.
                "pci-virtio-block" | "pci-nvme" => {
                    let device_spec =
                        make_storage_device_from_config(device_name, device)?;

                    let backend_name = match &device_spec {
                        StorageDeviceV0::VirtioDisk(disk) => {
                            disk.backend_name.clone()
                        }
                        StorageDeviceV0::NvmeDisk(disk) => {
                            disk.backend_name.clone()
                        }
                    };

                    let backend_config = config
                        .block_devs
                        .get(&backend_name)
                        .ok_or_else(|| {
                        ServerSpecBuilderError::DeviceMissingBackend(
                            device_name.clone(),
                            backend_name.clone(),
                        )
                    })?;

                    let backend_spec = make_storage_backend_from_config(
                        &backend_name,
                        backend_config,
                    )?;

                    self.builder.add_storage_device(
                        device_name.clone(),
                        device_spec,
                        backend_name,
                        backend_spec,
                    )?;
                }
                "pci-virtio-viona" => {
                    self.add_network_device_from_config(device_name, device)?
                }
                #[cfg(feature = "falcon")]
                "softnpu-pci-port" => {
                    self.add_softnpu_pci_port_from_config(device_name, device)?
                }
                #[cfg(feature = "falcon")]
                "softnpu-port" => {
                    self.add_softnpu_device_from_config(device_name, device)?
                }
                #[cfg(feature = "falcon")]
                "softnpu-p9" => {
                    self.add_softnpu_p9_from_config(device_name, device)?
                }
                #[cfg(feature = "falcon")]
                "pci-virtio-9p" => {
                    self.add_p9fs_from_config(device_name, device)?
                }
                _ => {
                    return Err(ServerSpecBuilderError::ConfigTomlError(
                        format!("Unrecognized device type {}", driver),
                    ))
                }
            }
        }

        for bridge in config.pci_bridges.iter() {
            self.add_pci_bridge_from_config(bridge)?;
        }

        Ok(())
    }

    #[cfg(feature = "falcon")]
    fn add_softnpu_p9_from_config(
        &mut self,
        name: &str,
        device: &config::Device,
    ) -> Result<(), ServerSpecBuilderError> {
        let pci_path: PciPath = device.get("pci-path").ok_or_else(|| {
            ServerSpecBuilderError::ConfigTomlError(format!(
                "Failed to get PCI path for storage device {}",
                name
            ))
        })?;

        self.builder
            .set_softnpu_p9(components::devices::SoftNpuP9 { pci_path })?;
        Ok(())
    }

    #[cfg(feature = "falcon")]
    fn add_softnpu_pci_port_from_config(
        &mut self,
        name: &str,
        device: &config::Device,
    ) -> Result<(), ServerSpecBuilderError> {
        let pci_path: PciPath = device.get("pci-path").ok_or_else(|| {
            ServerSpecBuilderError::ConfigTomlError(format!(
                "Failed to get PCI path for network device {}",
                name
            ))
        })?;

        self.builder.set_softnpu_pci_port(
            components::devices::SoftNpuPciPort { pci_path },
        )?;

        Ok(())
    }

    #[cfg(feature = "falcon")]
    fn add_softnpu_device_from_config(
        &mut self,
        name: &str,
        device: &config::Device,
    ) -> Result<(), ServerSpecBuilderError> {
        let vnic_name = device.get_string("vnic").ok_or_else(|| {
            ServerSpecBuilderError::ConfigTomlError(format!(
                "Failed to parse vNIC name for device {}",
                name
            ))
        })?;

        self.builder.add_softnpu_port(
            name.to_string(),
            components::devices::SoftNpuPort {
                name: name.to_string(),
                backend_name: vnic_name.to_string(),
            },
        )?;

        Ok(())
    }

    #[cfg(feature = "falcon")]
    fn add_p9fs_from_config(
        &mut self,
        name: &str,
        device: &config::Device,
    ) -> Result<(), ServerSpecBuilderError> {
        let source: String = device.get("source").ok_or_else(|| {
            ServerSpecBuilderError::ConfigTomlError(format!(
                "Failed to get source for p9 device {}",
                name
            ))
        })?;

        let target: String = device.get("target").ok_or_else(|| {
            ServerSpecBuilderError::ConfigTomlError(format!(
                "Failed to get target for p9 device {}",
                name
            ))
        })?;

        let chunk_size: u32 = device.get("chunk_size").unwrap_or(65536);
        let pci_path: PciPath = device.get("pci-path").ok_or_else(|| {
            ServerSpecBuilderError::ConfigTomlError(format!(
                "Failed to get PCI path for p9 device {}",
                name
            ))
        })?;

        self.builder.set_p9fs(components::devices::P9fs {
            source,
            target,
            chunk_size,
            pci_path,
        })?;

        Ok(())
    }

    /// Adds a serial port specification to the spec under construction.
    pub fn add_serial_port(
        &mut self,
        port: components::devices::SerialPortNumber,
    ) -> Result<(), ServerSpecBuilderError> {
        self.builder.add_serial_port(port)?;
        Ok(())
    }

    pub fn finish(self) -> InstanceSpecV0 {
        self.builder.finish()
    }
}

#[cfg(test)]
mod test {
    use crucible_client_types::VolumeConstructionRequest;
    use propolis_api_types::InstanceMetadata;
    use propolis_api_types::Slot;
    use uuid::Uuid;

    use crate::config::Config;

    use super::*;

    fn test_metadata() -> InstanceMetadata {
        InstanceMetadata {
            silo_id: uuid::uuid!("556a67f8-8b14-4659-bd9f-d8f85ecd36bf"),
            project_id: uuid::uuid!("75f60038-daeb-4a1d-916a-5fa5b7237299"),
        }
    }

    fn default_spec_builder(
    ) -> Result<ServerSpecBuilder, ServerSpecBuilderError> {
        ServerSpecBuilder::new(
            &InstanceProperties {
                id: Default::default(),
                name: Default::default(),
                description: Default::default(),
                metadata: test_metadata(),
                image_id: Default::default(),
                bootrom_id: Default::default(),
                memory: 512,
                vcpus: 4,
            },
            &Config::default(),
        )
    }

    #[test]
    fn make_default_builder() {
        assert!(default_spec_builder().is_ok());
    }

    #[test]
    fn duplicate_pci_slot() {
        let mut builder = default_spec_builder().unwrap();

        // Adding the same disk device twice should fail.
        assert!(builder
            .add_disk_from_request(&DiskRequest {
                name: "disk1".to_string(),
                slot: Slot(0),
                read_only: true,
                device: "nvme".to_string(),
                volume_construction_request: VolumeConstructionRequest::File {
                    id: Uuid::new_v4(),
                    block_size: 512,
                    path: "disk1.img".to_string()
                },
            })
            .is_ok());
        assert!(matches!(
            builder
                .add_disk_from_request(&DiskRequest {
                    name: "disk2".to_string(),
                    slot: Slot(0),
                    read_only: true,
                    device: "virtio".to_string(),
                    volume_construction_request:
                        VolumeConstructionRequest::File {
                            id: Uuid::new_v4(),
                            block_size: 512,
                            path: "disk2.img".to_string()
                        },
                })
                .err(),
            Some(ServerSpecBuilderError::InnerBuilderError(
                SpecBuilderError::PciPathInUse(_)
            ))
        ));
    }

    #[test]
    fn duplicate_serial_port() {
        use components::devices::SerialPortNumber;

        let mut builder = default_spec_builder().unwrap();
        assert!(builder.add_serial_port(SerialPortNumber::Com1).is_ok());
        assert!(builder.add_serial_port(SerialPortNumber::Com2).is_ok());
        assert!(builder.add_serial_port(SerialPortNumber::Com3).is_ok());
        assert!(builder.add_serial_port(SerialPortNumber::Com4).is_ok());
        assert!(matches!(
            builder.add_serial_port(SerialPortNumber::Com1).err(),
            Some(ServerSpecBuilderError::InnerBuilderError(
                SpecBuilderError::SerialPortInUse(_)
            ))
        ));
    }

    #[test]
    fn unknown_storage_device_type() {
        let mut builder = default_spec_builder().unwrap();
        assert!(matches!(
            builder
                .add_disk_from_request(&DiskRequest {
                    name: "disk3".to_string(),
                    slot: Slot(0),
                    read_only: true,
                    device: "virtio-scsi".to_string(),
                    volume_construction_request:
                        VolumeConstructionRequest::File {
                            id: Uuid::new_v4(),
                            block_size: 512,
                            path: "disk3.img".to_string()
                        },
                })
                .err(),
            Some(ServerSpecBuilderError::UnrecognizedStorageDevice(_))
        ));
    }
}
