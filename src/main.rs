mod process;
mod error;
mod camera;
mod game;
mod overlay;
mod renderer;
mod projection;
mod tracker;
mod interp;
mod vsync;
mod session;
mod kinds;
mod skeleton;


use std::sync::Arc;
use std::time::Duration;

use tracing::{ error, info };
use tracing_subscriber::fmt as tracing_fmt;
use time::macros::{ format_description as time_description, offset as time_offset };
use error::{ ESPError, Result };

use process::Process;


const PKG: &str = "com.mojang.minecraftpe";
const GMODULE: &str = "libminecraftpe.so";
const MODULE: &str = "MAIN";
/// MAILBOX 模式下拿不到 AChoreographer 时的后备帧率
const TARGET_FPS: f64 = 120.0;
/// 没找到游戏进程、或者不在世界里时，隔多久再试一次
const RETRY_INTERVAL: Duration = Duration::from_secs(2);


fn main() {
    let _guard = logger_service(true, false, false);
    if let Err(e) = _main() {
        error!("{e}");
        return;
    }
}

/// 整体流程：
///   等待游戏进程 → 等待进入世界 → 画框，直到退出世界 → 回到“等待进入世界”
///
/// 图层和渲染器不依赖游戏，启动时建一次，一直用到程序结束；
/// 每次进入世界都重新附加游戏进程、重建整个 Session。
fn _main() -> Result<()> {
    let window = overlay::create_window(overlay::SCREEN_W, overlay::SCREEN_H)?;
    let mut renderer = renderer::Renderer::new(window, overlay::SCREEN_W, overlay::SCREEN_H)?;
    let mut pacer = vsync::Pacer::new(renderer.present_mode == ash::vk::PresentModeKHR::MAILBOX, TARGET_FPS);
    info!("frame pacing: {}", pacer.describe());

    // 等待期间同样的原因只打印一次，避免每 2 秒刷一行
    let mut last_wait_reason = String::new();

    loop {
        // ---- 1. 游戏进程：每次尝试进入世界前都重新附加 ----
        // 不只是为了应对游戏重启（PID 会变）：堆会随着游戏运行不断扩展，扫堆用的 maps 必须是最新的，
        // 否则重新进入世界后，新分配在新区域里的本地玩家会扫不到
        let proc = match attach() {
            Ok(p) => p,
            Err(e) => { wait(&mut last_wait_reason, e.to_string()); continue; }
        };

        // ---- 2. 进入世界：拿不到本地玩家说明在主菜单或者加载中，过一会儿再试 ----
        match session::Session::start(proc) {
            Ok(mut s) => {
                last_wait_reason.clear();
                s.run(&mut renderer, &mut pacer)?;
                // run 返回 = 退出了世界。丢掉 Session，后台刷新线程随之结束
            }
            Err(e) => wait(&mut last_wait_reason, format!("not in a world yet ({e})")),
        }
    }
}

/// 等待一会儿再重试；同样的原因只打印一次
fn wait(last_reason: &mut String, reason: String) {
    if reason != *last_reason {
        info!("waiting: {reason}");
        *last_reason = reason;
    }
    std::thread::sleep(RETRY_INTERVAL);
}

/// 找到游戏进程，读 maps，确定模块基址
fn attach() -> Result<Arc<Process>> {
    let pid = process::find_pid(PKG)
        .ok_or(ESPError::Custom(MODULE, format!("cannot find the PID of {PKG}").into()))?;
    let mut process = Process::new(pid);
    process.init()?;
    // debug 级别：等待期间每 2 秒附加一次，info 会刷屏
    tracing::debug!("Game PID: {}, base address: 0x{:X}", process.pid, process.base);
    // 渲染循环和实体刷新线程共用同一个 Process（只读，process_vm_readv 本身是线程安全的）
    Ok(Arc::new(process))
}

pub fn logger_service(with_ansi: bool, with_file: bool, with_line_number: bool)
    -> tracing_appender::non_blocking::WorkerGuard
{
    use std::io::IsTerminal;

    // 自定义时间格式 +8时区
    let timer = tracing_fmt::time::OffsetTime::new(
        time_offset!(+8),
        time_description!("[year]-[month]-[day] [hour]:[minute]:[second].[subsecond digits:6]")
    );

    // 被 agent 托管时 stdout 是管道而非 TTY，此时关掉颜色，落库的是纯文本
    let with_ansi = with_ansi && std::io::stdout().is_terminal();

    // 无锁日志写入
    let (non_blocking, _guard) =
        tracing_appender::non_blocking(std::io::stdout());

    let log_subscriber = tracing_fmt::Subscriber::builder()
        .with_writer(non_blocking)
        .with_target(false)
        .with_timer(timer)
        .with_max_level(tracing::Level::INFO)
        .with_ansi(with_ansi)
        .with_file(with_file)
        .with_line_number(with_line_number)
        .finish();
    tracing::subscriber::set_global_default(log_subscriber).unwrap();

    _guard
}
