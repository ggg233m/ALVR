use std::fs;
use std::path::Path;
use std::process::Command;

use alvr_common::anyhow::bail;
use alvr_common::{debug, error, info, warn};
use sysinfo::Process;

pub fn launch_steamvr_with_steam() {
    if let Err(e) = super::spawn_and_reap(Command::new("steam").args(["steam://rungameid/250820"]))
    {
        error!("通过 Steam 启动 SteamVR 失败：{e}");
    }
}

pub fn terminate_process(process: &Process) {
    process.kill_with(sysinfo::Signal::Term);
}

pub fn maybe_wrap_vrcompositor_launcher() -> alvr_common::anyhow::Result<()> {
    let steamvr_bin_dir = alvr_server_io::steamvr_root_dir()?
        .join("bin")
        .join("linux64");
    let steamvr_vrserver_path = steamvr_bin_dir.join("vrserver");
    debug!(
        "用于检查 Linux 文件的路径：{}",
        steamvr_vrserver_path.display()
    );
    match steamvr_vrserver_path.try_exists() {
        Ok(exists) => {
            if !exists {
                bail!(
                    "缺少 SteamVR Linux 文件，已中止启动。请重新检查 SteamVR 的兼容工具、验证 SteamVR 文件完整性，并确认没有把 Flatpak Steam 与非 Flatpak 的 ALVR 混用。"
                );
            }
        }
        Err(e) => {
            return Err(e.into());
        }
    };

    let launcher_path = steamvr_bin_dir.join("vrcompositor");
    // In case of SteamVR update, vrcompositor will be restored
    if fs::read_link(&launcher_path).is_ok() {
        fs::remove_file(&launcher_path)?; // recreate the link
    } else {
        fs::rename(&launcher_path, steamvr_bin_dir.join("vrcompositor.real"))?;
    }

    std::os::unix::fs::symlink(
        crate::get_filesystem_layout().vrcompositor_wrapper(),
        &launcher_path,
    )?;

    Ok(())
}
#[derive(PartialEq)]
enum DeviceInfo {
    Nvidia,
    Amd { device_type: wgpu::DeviceType },
    Intel { device_type: wgpu::DeviceType },
    Unknown,
}

pub fn linux_hardware_checks() {
    let wgpu_adapters = pollster::block_on(
        wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            flags: Default::default(),
            memory_budget_thresholds: Default::default(),
            backend_options: Default::default(),
            display: Default::default(),
        })
        .enumerate_adapters(wgpu::Backends::VULKAN),
    );
    let device_infos = wgpu_adapters
        .iter()
        .filter(|adapter| {
            adapter.get_info().device_type == wgpu::DeviceType::DiscreteGpu
                || adapter.get_info().device_type == wgpu::DeviceType::IntegratedGpu
        })
        .map(|adapter| {
            let vendor = match adapter.get_info().vendor {
                0x10de => DeviceInfo::Nvidia,
                0x1002 => DeviceInfo::Amd {
                    device_type: adapter.get_info().device_type,
                },
                0x8086 => DeviceInfo::Intel {
                    device_type: adapter.get_info().device_type,
                },
                _ => DeviceInfo::Unknown,
            };

            (adapter, vendor)
        })
        .collect::<Vec<_>>();
    linux_gpu_checks(&device_infos);
    linux_encoder_checks(&device_infos);
}

fn linux_gpu_checks(device_infos: &[(&wgpu::Adapter, DeviceInfo)]) {
    let have_intel_igpu = device_infos.iter().any(|gpu| {
        gpu.1
            == DeviceInfo::Intel {
                device_type: wgpu::DeviceType::IntegratedGpu,
            }
    });
    debug!("是否存在 Intel 核显：{}", have_intel_igpu);
    let have_amd_igpu = device_infos.iter().any(|gpu| {
        gpu.1
            == DeviceInfo::Amd {
                device_type: wgpu::DeviceType::IntegratedGpu,
            }
    });
    debug!("是否存在 AMD 核显：{}", have_amd_igpu);

    let have_igpu = have_intel_igpu || have_amd_igpu;
    debug!("是否存在核显：{}", have_igpu);

    let have_nvidia_dgpu = device_infos.iter().any(|gpu| gpu.1 == DeviceInfo::Nvidia);
    debug!("是否存在 NVIDIA 独显：{}", have_nvidia_dgpu);

    let have_amd_dgpu = device_infos.iter().any(|gpu| {
        gpu.1
            == DeviceInfo::Amd {
                device_type: wgpu::DeviceType::DiscreteGpu,
            }
    });
    debug!("是否存在 AMD 独显：{}", have_amd_dgpu);

    if have_amd_igpu || have_amd_dgpu {
        let is_any_amd_driver_invalid = device_infos.iter().any(|gpu| {
            info!("驱动名称：{}", gpu.0.get_info().driver);
            match gpu.0.get_info().driver.as_str() {
                "AMD proprietary driver" | "AMD open-source driver" => true, // AMDGPU-Pro | AMDVLK
                _ => false,
            }
        });
        if is_any_amd_driver_invalid {
            error!(
                "检测到 Amdvlk 或 amdgpu-pro vulkan 驱动，SteamVR 可能无法正常工作。 \
            请移除它们，或使其对 SteamVR 及你要启动的游戏不可用。\n\
            更多详细信息请访问 wiki： \
            https://github.com/alvr-org/ALVR/wiki/Linux-Troubleshooting#artifacting-no-steamvr-overlay-or-graphical-glitches-in-streaming-view"
            )
        }
    }

    let have_intel_dgpu = device_infos.iter().any(|gpu| {
        gpu.1
            == DeviceInfo::Intel {
                device_type: wgpu::DeviceType::DiscreteGpu,
            }
    });
    debug!("是否存在 Intel 独显：{}", have_intel_dgpu);

    let steamvr_root_dir = match alvr_server_io::steamvr_root_dir() {
        Ok(dir) => dir,
        Err(e) => {
            error!(
                "未找到 OpenVR 或 SteamVR 文件。 \
                请确保已安装并至少运行过一次 SteamVR。 \
                如果使用 Flatpak Steam，请确保使用 Flatpak ALVR 中的 ALVR Dashboard。{e}"
            );
            return;
        }
    };

    let vrmonitor_path_string = steamvr_root_dir
        .join("bin")
        .join("vrmonitor.sh")
        .into_os_string()
        .into_string()
        .unwrap();
    debug!("vrmonitor 路径：{}", vrmonitor_path_string);

    let steamvr_opts = "为使 VR 正常工作，需要将以下一行填入 SteamVR 的启动选项并重启 SteamVR：";
    let game_opts = "并把类似的一行填入你要从 Steam 启动的所有游戏的启动选项：";

    let mut vrmonitor_path_written = false;
    if have_igpu {
        if have_nvidia_dgpu {
            let base_path = "/usr/share/vulkan/icd.d/nvidia_icd";
            let nvidia_vk_override_path = if Path::new(&format!("{base_path}.json")).exists() {
                format!("VK_DRIVER_FILES={base_path}.json")
            } else if Path::new(&format!("{base_path}.x86_64.json")).exists() {
                format!("VK_DRIVER_FILES={base_path}.x86_64.json")
            } else {
                "__VK_LAYER_NV_optimus=NVIDIA_only".to_string()
            };
            let nv_options = format!(
                "__GLX_VENDOR_LIBRARY_NAME=nvidia __NV_PRIME_RENDER_OFFLOAD=1 {nvidia_vk_override_path}"
            );

            warn!("{steamvr_opts}\n{nv_options} {vrmonitor_path_string} %command%");
            warn!("{game_opts}\n{nv_options} %command%");

            vrmonitor_path_written = true;
        } else if have_intel_dgpu || have_amd_dgpu {
            warn!("{steamvr_opts}\nDRI_PRIME=1 {vrmonitor_path_string} %command%");
            warn!("{game_opts}\nDRI_PRIME=1 %command%");
            vrmonitor_path_written = true;
        } else {
            warn!(
                "注意：仅使用集成显卡可能导致 SteamVR 和 VR 游戏性能非常差。"
            );
            warn!(
                "更多信息请参阅 wiki：https://github.com/alvr-org/ALVR/wiki/Linux-Troubleshooting"
            )
        }
    }
    if !vrmonitor_path_written {
        warn!(
            "请确保已在 SteamVR 启动选项中填入以下一行并重启 SteamVR：\n\
            {vrmonitor_path_string} %command%"
        )
    }
}

fn linux_encoder_checks(device_infos: &[(&wgpu::Adapter, DeviceInfo)]) {
    for device_info in device_infos {
        match device_info.1 {
            DeviceInfo::Nvidia => {
                match nvml_wrapper::Nvml::init() {
                    Ok(nvml) => {
                        let device_count = nvml.device_count().unwrap();
                        debug!("NVML 设备数量：{}", device_count);
                        // fixme: on multi-gpu nvidia system will do it twice,
                        for index in 0..device_count {
                            match nvml.device_by_index(index) {
                                Ok(device) => {
                                    debug!("NVML 设备名称：{}", device.name().unwrap());
                                    probe_nvenc_encoder_profile(
                                        &device,
                                        nvml_wrapper::enum_wrappers::device::EncoderType::H264,
                                        "H264",
                                    );
                                    probe_nvenc_encoder_profile(
                                        &device,
                                        nvml_wrapper::enum_wrappers::device::EncoderType::HEVC,
                                        "HEVC",
                                    );
                                    // todo: probe for AV1 when will be available in nvml-wrapper
                                }
                                Err(e) => {
                                    error!("获取 NVML 设备失败，错误：{}", e)
                                }
                            }
                        }
                    }
                    Err(e) => {
                        alvr_common::show_e(format!("无法初始化 NVML 引擎，错误：{e}。"))
                    }
                }
            }
            DeviceInfo::Amd { device_type: _ } | DeviceInfo::Intel { device_type: _ } => {
                let libva_display_open = libva::Display::open();
                if let Some(libva_display) = libva_display_open {
                    if let Ok(vendor_string) = libva_display.query_vendor_string() {
                            info!("GPU 编码器厂商：{}", vendor_string);
                    }
                    probe_libva_encoder_profile(
                        &libva_display,
                        libva::VAProfile::VAProfileH264Main,
                        "H264",
                        true,
                    );
                    probe_libva_encoder_profile(
                        &libva_display,
                        libva::VAProfile::VAProfileHEVCMain,
                        "HEVC",
                        true,
                    );
                    probe_libva_encoder_profile(
                        &libva_display,
                        libva::VAProfile::VAProfileAV1Profile0,
                        "AV1",
                        false,
                    );
                } else {
                    alvr_common::show_e(
                        "系统中未找到 VA-API 运行时，\
                        可能无法使用硬件编码。\
                        请为你的发行版安装 VA-API 运行时，\
                        并确认其可用（Manjaro、Fedora 受影响）。\
                        详细建议请查看 wiki：\
                        https://github.com/alvr-org/ALVR/wiki/Linux-Troubleshooting#failed-to-create-vaapi-encoder",
                    );
                }
            }
            _ => alvr_common::show_e(
                "无法确定用于硬件编码的 GPU。\
            可能会回退到软件编码。",
            ),
        }
    }
}

fn probe_nvenc_encoder_profile(
    device: &nvml_wrapper::Device,
    encoder_type: nvml_wrapper::enum_wrappers::device::EncoderType,
    profile_name: &str,
) {
    match device.encoder_capacity(encoder_type) {
        Ok(_) => {
            info!("GPU 支持 {} 规格。", profile_name);
        }
        Err(e) => {
            if matches!(e, nvml_wrapper::error::NvmlError::NotSupported) {
                alvr_common::show_e(format!(
                    "你的 NVIDIA GPU 不支持 {profile_name}。请确认 CUDA 已正确安装。错误：{e}"
                ))
            } else {
                error!("{}", e)
            }
        }
    }
}

fn probe_libva_encoder_profile(
    libva_display: &std::rc::Rc<libva::Display>,
    profile_type: libva::VAProfile::Type,
    profile_name: &str,
    is_critical: bool,
) {
    let profile_probe = libva_display.query_config_entrypoints(profile_type);
    let mut message = String::new();
    if profile_probe.is_err() {
        message = format!("未找到 {profile_name} 编码器。");
    } else if let Ok(profile) = profile_probe {
        if profile.is_empty() {
            message = format!("{profile_name} 规格入口点为空。");
        } else if !profile.contains(&libva::VAEntrypoint::VAEntrypointEncSlice) {
            message = format!("{profile_name} 规格不包含编码入口点。");
        }
    }
    if !message.is_empty() {
        if is_critical {
            error!("{} 你的 GPU 可能无法据此进行编码。", message);
        } else {
            info!(
                "{}
                你的 GPU 可能无法据此进行编码。 \
            如果未使用此编码器，可忽略此消息。",
                message
            );
        }
    }
}
