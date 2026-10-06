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


use std::time::{ Duration, Instant };

use tracing::{ error, info };
use tracing_subscriber::fmt as tracing_fmt;
use time::macros::{ format_description as time_description, offset as time_offset };
use error::{ ESPError, Result };

use process::{ Process };
use game::{
    scan_actors, fetch_component_addr,
    STORAGE_ID, fetch_rotation_addr
};


const PKG: &str = "com.mojang.minecraftpe";
const GMODULE: &str = "libminecraftpe.so";
const VPTR_OFFSET: u64 = 0x12FAD708;
const MODULE: &str = "MAIN";
/// 视角默认延后的时间（毫秒），抵消游戏自身的渲染延迟。实测 30 ms 最贴合
const DEFAULT_VIEW_DELAY_MS: u64 = 30;
/// MAILBOX 模式下拿不到 AChoreographer 时的后备帧率
const TARGET_FPS: f64 = 120.0;


fn main() {
    let _guard = logger_service(true, false, false);
    if let Err(e) = _main() {
        error!("{e}");
        return;
    }
}

fn _main() -> Result<()> {
    let Some(pid) = process::find_pid(PKG) else {
        return Err(ESPError::Custom(MODULE, format!("Cannot find the PID of {}.", PKG).into()));
    };
    let mut process = Process::new(pid);
    process.init()?;
    // 渲染循环和实体刷新线程共用同一个 Process（只读，process_vm_readv 本身是线程安全的）
    let process = std::sync::Arc::new(process);
    info!(
        "Game PID: {}, Base address: 0x{:X}, vptr: 0x{:X}",
        process.pid, process.base, process.base + VPTR_OFFSET
    );

    // 投影矩阵：GameRenderer + 0x420，每帧从这里读焦距
    let proj_addr = projection::locate(&process)?;
    info!("Projection matrix: 0x{:X}", proj_addr);

    let actors = scan_actors(&process, process.base + VPTR_OFFSET)?;
    info!("Scaned actors: {}\n{:X?}", actors.len(), actors);
    let mut client_act = 0;
    for v in actors.iter() {
        if process.read_u64(v + 0x88)? == 0x0 {
            client_act = *v;
            break;
        }
    }
    if client_act == 0 {
        return Err(ESPError::ZeroRead(MODULE));
    }

    let entry_addr = fetch_component_addr(
        &process, client_act,
        STORAGE_ID
    )?;
    // 实体列表只由后台线程 actor-tracker 扫描；这里等它的第一份结果，从中找本地玩家
    let mut actors_rx = tracker::spawn(process.clone(), entry_addr);
    let mut actors = tracker::wait_first(&mut actors_rx, Duration::from_secs(3))
        .ok_or(ESPError::Custom(MODULE, "actor-tracker produced no actor list.".into()))?;
    let player = actors
        .iter()
        .find(|v| &*v.rtti == "11LocalPlayer")
        .ok_or(ESPError::Custom(MODULE, "Local player not found.".into()))?
        .clone();
    // 第 4 步要用：LocalPlayer 的 vtable 偏移（以后启动时改为直接扫它，不再依赖牛）
    info!("LocalPlayer: 0x{:X}, vptr = base+0x{:X}", player.addr, process.read_ptr(player.addr)? - process.base);
    let player_eid = process.read_u32(player.addr + 0x18)?;
    let rotation_addr = fetch_rotation_addr(&process, client_act, player_eid)?;
    info!("Player EID: {}", player_eid);

    // diff_actors(&process, &actors)?;

    // 游戏画面（横屏）的尺寸，投影算出的框都在这个坐标系里
    let (w, h) = (overlay::SCREEN_W as f32, overlay::SCREEN_H as f32);
    let window = overlay::create_window(overlay::SCREEN_W, overlay::SCREEN_H)?;
    let mut renderer = renderer::Renderer::new(window, overlay::SCREEN_W, overlay::SCREEN_H)?;

    let (mut pitch, mut yaw) = (0.0f32, 0.0f32);
    // 视角取多久以前的值，用来抵消游戏自身的渲染延迟（见 interp::ViewHistory）。MC_ESP_VIEW_DELAY_MS 可以调
    let view_delay = Duration::from_millis(
        std::env::var("MC_ESP_VIEW_DELAY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_VIEW_DELAY_MS),
    );
    info!("view delay: {:?}（MC_ESP_VIEW_DELAY_MS 可以调整）", view_delay);
    let mut view = interp::ViewHistory::new();
    // 焦距（像素）= 矩阵的 a × 半屏宽。读失败的帧沿用上一帧的值
    let mut k = projection::read_focal(&process, proj_addr).unwrap_or(1.0) * w / 2.0;
    let mut outlines = Vec::new();
    // 每帧一次性读回的内存段：(地址, 长度)，以及接收它们的缓冲区
    let mut segments: Vec<(u64, usize)> = Vec::new();
    let mut bytes: Vec<u8> = Vec::new();

    // MAILBOX 模式下 begin_frame 不等垂直同步，要自己控制节奏：优先跟着屏幕的垂直同步走（AChoreographer），
    // 拿不到时退回按固定帧率 sleep。FIFO 模式下 begin_frame 本身就会等垂直同步，不需要额外控制
    let paced = renderer.present_mode == ash::vk::PresentModeKHR::MAILBOX;
    let mut vsync = if paced { vsync::Vsync::new() } else { None };
    info!("frame pacing: {}", match (paced, vsync.is_some()) {
        (false, _) => "FIFO (driver)",
        (true, true) => "MAILBOX + AChoreographer vsync",
        (true, false) => "MAILBOX + fixed-rate sleep",
    });
    let frame_interval = Duration::from_secs_f64(1.0 / TARGET_FPS);
    let mut next_frame = Instant::now();
    let mut clock = interp::TickClock::new(frame_interval);

    loop {
        if let Some(v) = vsync.as_mut() {
            v.wait();
        } else if paced {
            let now = Instant::now();
            if next_frame > now { std::thread::sleep(next_frame - now); }
            // 落后太多（比如被系统挂起过）时不追帧，从现在重新开始计时
            next_frame = (next_frame + frame_interval).max(Instant::now());
        }

        // 后台线程有新列表就换上（Arc 克隆，不复制列表本身）
        if actors_rx.has_changed().unwrap_or(false) {
            actors = actors_rx.borrow_and_update().clone();
        }

        // 先拿到交换链图像，再读内存：FIFO 下 begin_frame 可能要等垂直同步，等完再读，数据才是最新的
        let frame = renderer.begin_frame()?;

        // ---- 一次系统调用读回本帧要用的所有数据 ----
        // 段 0：玩家的旋转 (pitch, yaw)；段 1：玩家的 pos + posPrev；段 2：投影矩阵开头；之后每个实体一段 pos + posPrev
        const STATE_LEN: usize = 24;
        // 旋转组件 16 字节：pitch, yaw, prevPitch, prevYaw
        const ROT_LEN: usize = 16;
        segments.clear();
        segments.push((rotation_addr, ROT_LEN));
        segments.push((player.state_vector, STATE_LEN));
        segments.push((proj_addr, projection::HEAD_LEN));
        for actor in actors.iter() {
            segments.push((actor.state_vector, STATE_LEN));
        }
        bytes.resize(segments.iter().map(|s| s.1).sum(), 0);
        let ok = process.read_many(&segments, &mut bytes);

        let f32_at = |off: usize| f32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
        let vec3_at = |off: usize| [f32_at(off), f32_at(off + 4), f32_at(off + 8)];

        // 读到 (0,0) 视为读写竞争：沿用上一帧的角度，而不是跳过这一帧（跳过会让框闪烁）
        if ok[0] {
            let (p, y) = (f32_at(0), f32_at(4));
            if p != 0.0 || y != 0.0 { (pitch, yaw) = (p, y); }
        }
        // FOV 随奔跑、飞行变化，所以焦距每帧都读
        if ok[2] {
            if let Some(focal) = projection::focal_from_bytes(&bytes[ROT_LEN + STATE_LEN..ROT_LEN + STATE_LEN + projection::HEAD_LEN]) {
                k = focal * w / 2.0;
            }
        }

        outlines.clear();
        if ok[1] {
            // 把所有实体（含玩家）的 pos 混进指纹，判断是否刚过了 tick 边界，得到插值系数。
            // 视角不放进来：它随游戏的每一帧变化，不是按 tick 变的，放进来会把 tick 计时打乱
            let player_off = ROT_LEN;
            let actors_off = ROT_LEN + STATE_LEN + projection::HEAD_LEN;
            let mut signature = interp::fingerprint(0, &vec3_at(player_off));
            for (i, _) in actors.iter().enumerate().filter(|(i, _)| ok[3 + i]) {
                signature = interp::fingerprint(signature, &vec3_at(actors_off + i * STATE_LEN));
            }
            let now = Instant::now();
            let alpha = clock.update(signature, now);

            // 相机（眼睛）位置也要插值：玩家走动时，游戏的相机同样是按 tick 插值移动的
            let eye = interp::lerp(vec3_at(player_off + 12), vec3_at(player_off), alpha);
            // 视角取 view_delay 之前的值，和游戏实际显示在屏幕上的画面对齐
            view.push(now, pitch, yaw);
            let (cam_pitch, cam_yaw) = view.sample(now.checked_sub(view_delay).unwrap_or(now)).unwrap_or((pitch, yaw));
            let camera = camera::Camera { pos: eye, pitch: cam_pitch, yaw: cam_yaw, w, h, k };

            for (i, actor) in actors.iter().enumerate() {
                if !ok[3 + i] || actor.addr == player.addr { continue; }
                let off = actors_off + i * STATE_LEN;
                let feet = interp::lerp(vec3_at(off + 12), vec3_at(off), alpha);
                if feet.iter().any(|v| !v.is_finite()) { continue; }
                if let Some(rect) = camera.project_box(feet, 1.0, 1.3) {
                    outlines.push(renderer::Outline {
                        rect,
                        color: [1.0, 0.0, 0.0, 1.0],
                        thickness: 2.0,
                    });
                }
            }
        }

        // 不管本帧有没有读到数据都要 end_frame：acquire 到的图像必须 present 回去，否则交换链会被耗尽
        renderer.end_frame(frame, &outlines)?;
    }
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
