#[cfg(target_os = "linux")]
mod linux_steamvr;
#[cfg(windows)]
mod windows_steamvr;

use crate::data_sources;
use alvr_adb::commands as adb;
use alvr_common::{
    anyhow::{Context, Result},
    debug, error,
    glam::bool,
    parking_lot::Mutex,
    warn,
};
use alvr_filesystem::{self as afs};
use serde_json::{self, json};
use std::{
    ffi::OsStr,
    fs,
    marker::PhantomData,
    process::Command,
    thread,
    time::{Duration, Instant},
};
use sysinfo::{ProcessesToUpdate, System};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

// On Linux, spawned processes are children of the current process.
// We need to make sure to reap SteamVR processes spawned by us or they would not be able to be
// relaunched.
pub(super) fn spawn_and_reap(command: &mut Command) -> Result<()> {
    let mut child = command.spawn()?;

    thread::spawn(move || {
        child.wait().ok();
    });

    Ok(())
}
const DRIVER_KEY: &str = "driver_alvr_server";
const BLOCKED_KEY: &str = "blocked_by_safe_mode";

pub fn is_steamvr_running() -> bool {
    System::new_all()
        .processes_by_name(OsStr::new(&afs::exec_fname("vrserver")))
        .count()
        != 0
}

pub fn maybe_kill_steamvr() {
    let mut system = System::new_all();

    #[allow(unused_variables)]
    for process in system.processes_by_name(OsStr::new(&afs::exec_fname("vrmonitor"))) {
        debug!("正在结束 vrmonitor");

        #[cfg(target_os = "linux")]
        linux_steamvr::terminate_process(process);
        #[cfg(windows)]
        windows_steamvr::kill_process(process.pid().as_u32());

        thread::sleep(Duration::from_secs(1));
    }

    system.refresh_processes(ProcessesToUpdate::All, true);

    #[allow(unused_variables)]
    for process in system.processes_by_name(OsStr::new(&afs::exec_fname("vrserver"))) {
        debug!("正在结束 vrserver");

        #[cfg(target_os = "linux")]
        linux_steamvr::terminate_process(process);
        #[cfg(windows)]
        windows_steamvr::kill_process(process.pid().as_u32());

        thread::sleep(Duration::from_secs(1));
    }
}

fn unblock_alvr_driver() -> Result<()> {
    if !cfg!(target_os = "linux") {
        return Ok(());
    }

    let path = alvr_server_io::steamvr_settings_file_path()?;
    let text = fs::read_to_string(&path).with_context(|| format!("读取 {path:?} 失败"))?;
    let new_text = unblock_alvr_driver_within_vrsettings(text.as_str())
        .with_context(|| "重写 .vrsettings 失败。")?;
    fs::write(&path, new_text)
        .with_context(|| "修改后写回 .vrsettings 失败。")?;
    Ok(())
}

// Reads and writes back steamvr.vrsettings in order to
// ensure the ALVR driver is not blocked (safe mode).
fn unblock_alvr_driver_within_vrsettings(text: &str) -> Result<String> {
    let mut settings = serde_json::from_str::<serde_json::Value>(text)?;
    let values = settings
        .as_object_mut()
        .with_context(|| "解析 .vrsettings 失败。")?;
    let blocked = values
        .get(DRIVER_KEY)
        .and_then(|driver| driver.get(BLOCKED_KEY))
        .and_then(|blocked| blocked.as_bool())
        .unwrap_or(false);

    if blocked {
        debug!("正在解除 SteamVR 中对 ALVR 驱动的阻止。");
        if !values.contains_key(DRIVER_KEY) {
            values.insert(DRIVER_KEY.into(), json!({}));
        }
        let driver = settings[DRIVER_KEY]
            .as_object_mut()
            .with_context(|| "在设置中未找到 ALVR 键。")?;
        driver.insert(BLOCKED_KEY.into(), json!(false)); // overwrites if present
    } else {
        debug!("ALVR 未在 SteamVR 中被阻止。");
    }

    Ok(serde_json::to_string_pretty(&settings)?)
}

pub struct Launcher {
    _phantom: PhantomData<()>,
}

impl Launcher {
    pub fn launch_steamvr(&self) {
        // The ADB server might be left running because of an unclean termination of SteamVR.
        // Kill it unconditionally to ensure clean state, regardless of current connection mode.
        // Note: this will also kill a system-wide ADB server not started by ALVR.
        if let Some(path) = adb::get_adb_path(&crate::get_filesystem_layout()) {
            adb::kill_server(&path).ok();
        }

        #[cfg(target_os = "linux")]
        linux_steamvr::linux_hardware_checks();

        let alvr_driver_dir = crate::get_filesystem_layout().openvr_driver_root_dir;

        // Make sure to unregister any other ALVR driver because it would cause a socket conflict
        let other_alvr_dirs = alvr_server_io::get_registered_drivers()
            .unwrap_or_default()
            .into_iter()
            .filter(|path| {
                path.to_string_lossy().to_lowercase().contains("alvr") && *path != alvr_driver_dir
            })
            .collect::<Vec<_>>();
        alvr_server_io::driver_registration(&other_alvr_dirs, false).ok();

        alvr_server_io::driver_registration(&[alvr_driver_dir], true).ok();

        if let Err(err) = unblock_alvr_driver() {
            warn!("解除 ALVR 驱动阻止失败：{:?}", err);
        }

        #[cfg(target_os = "linux")]
        {
            let vrcompositor_wrap_result = linux_steamvr::maybe_wrap_vrcompositor_launcher();
            alvr_common::show_err(linux_steamvr::maybe_wrap_vrcompositor_launcher());
            if vrcompositor_wrap_result.is_err() {
                return;
            }
        }

        if is_steamvr_running() {
            return;
        }

        debug!("SteamVR 未运行，正在启动……");

        if data_sources::get_read_only_local_session()
            .settings()
            .extra
            .steamvr_launcher
            .direct_launch
        {
            let start_script = afs::filesystem_layout_invalid().server_start_script();

            if start_script.exists() {
                debug!("正在运行 VR 服务器启动脚本：{}", start_script.display());

                if let Err(e) = spawn_and_reap(&mut Command::new(&start_script)) {
                    error!("运行 VR 服务器启动脚本失败：{e}");
                }
            } else if let Ok(steamvr_bin_dir) =
                alvr_server_io::steamvr_root_dir().map(|root| root.join("bin"))
            {
                let steamvr_path = if cfg!(windows) {
                    steamvr_bin_dir.join("win64").join("vrstartup.exe")
                } else {
                    steamvr_bin_dir.join("vrmonitor.sh")
                };

                debug!("正在从以下路径启动 SteamVR：{}", steamvr_path.display());

                if let Err(e) = spawn_and_reap(&mut Command::new(&steamvr_path)) {
                    error!(
                        "从自动检测到的路径 {} 启动 SteamVR 失败，错误：{e}",
                        steamvr_path.display()
                    );
                }
            } else {
                error!("未找到用于直接启动 SteamVR 的 SteamVR 文件");
            }
        } else {
            #[cfg(windows)]
            windows_steamvr::launch_steamvr_with_steam();

            #[cfg(target_os = "linux")]
            linux_steamvr::launch_steamvr_with_steam();
        }
    }

    pub fn ensure_steamvr_shutdown(&self) {
        debug!("正在等待 SteamVR 关闭……");
        let start_time = Instant::now();
        while start_time.elapsed() < SHUTDOWN_TIMEOUT && is_steamvr_running() {
            thread::sleep(Duration::from_millis(500));
        }

        maybe_kill_steamvr();
    }

    pub fn restart_steamvr(&self) {
        self.ensure_steamvr_shutdown();
        self.launch_steamvr();
    }
}

// Singleton with exclusive access
pub static LAUNCHER: Mutex<Launcher> = Mutex::new(Launcher {
    _phantom: PhantomData,
});
