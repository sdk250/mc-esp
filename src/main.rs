mod process;
mod error;
mod camera;
mod game;
mod overlay;
mod renderer;
mod projection;


use tracing::{ error, info };
use tracing_subscriber::fmt as tracing_fmt;
use time::macros::{ format_description as time_description, offset as time_offset };
use error::{ ESPError, Result };

use process::{ Process };
use game::{
    scan_actors, list_actors, fetch_component_addr,
    STORAGE_ID, fetch_rotation_addr
};


const PKG: &str = "com.mojang.minecraftpe";
const GMODULE: &str = "libminecraftpe.so";
const VPTR_OFFSET: u64 = 0x12FAD708;
const MODULE: &str = "MAIN";


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
    let actors = list_actors(&process, entry_addr)?;
    let player = actors
        .iter()
        .filter(| v | v.rtti == "11LocalPlayer").next()
        .ok_or(ESPError::Custom(MODULE, "Local player not found.".into()))?;
    let player_eid = process.read_u32(player.addr + 0x18)?;
    let rotation_addr = fetch_rotation_addr(&process, client_act, player_eid)?;
    let mut buf = [0u8; 8];
    info!("Player EID: {}", player_eid);

    // diff_actors(&process, &actors)?;

    // 游戏画面（横屏）的尺寸，投影算出的框都在这个坐标系里
    let (w, h) = (overlay::SCREEN_W as f32, overlay::SCREEN_H as f32);
    let window = overlay::create_window(overlay::SCREEN_W, overlay::SCREEN_H)?;
    let mut renderer = renderer::Renderer::new(window, overlay::SCREEN_W, overlay::SCREEN_H)?;

    let mut actors = list_actors(&process, entry_addr)?;
    let (mut pitch, mut yaw) = (0.0f32, 0.0f32);
    // 焦距（像素）= 矩阵的 a × 半屏宽。读失败的帧沿用上一帧的值
    let mut k = projection::read_focal(&process, proj_addr).unwrap_or(1.0) * w / 2.0;
    let mut outlines = Vec::new();
    for frame in 0u64.. {
        // 实体列表每秒左右刷新一次（rtti_name 要读好几次内存，没必要每帧都做）
        if frame % 60 == 0 {
            actors = list_actors(&process, entry_addr)?;
        }

        // 读到 (0,0) 视为读写竞争：沿用上一帧的角度，而不是跳过这一帧（跳过会让框闪烁）
        process.read_into(rotation_addr, &mut buf);
        let p = f32::from_le_bytes(buf[..4].try_into().unwrap());
        let y = f32::from_le_bytes(buf[4..].try_into().unwrap());
        if p != 0.0 || y != 0.0 {
            (pitch, yaw) = (p, y);
        }

        let Ok(player_xyz) = process.read_vec3(process.read_ptr(
            player.addr + 0x208
        )?) else { continue; };
        // FOV 随奔跑、飞行变化，所以焦距每帧都读
        if let Some(focal) = projection::read_focal(&process, proj_addr) {
            k = focal * w / 2.0;
        }
        let camera = camera::Camera { pos: player_xyz, pitch, yaw, w, h, k };

        outlines.clear();
        for actor in &actors {
            if actor.rtti != "6Animal" { continue; }
            let Ok(svc) = process.read_ptr(actor.addr + 0x208) else { continue; };
            let Ok(feet) = process.read_vec3(svc) else { continue; };
            if let Some(rect) = camera.project_box(feet, 1.0, 1.3) {
                outlines.push(renderer::Outline {
                    rect,
                    color: [1.0, 0.0, 0.0, 1.0],
                    thickness: 2.0,
                });
            }
        }

        // FIFO 模式下 draw 会等到下一次垂直同步，循环自然就按屏幕刷新率运行
        renderer.draw(&outlines)?;
    }

    Ok(())
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
