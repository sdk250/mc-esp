//! 一次“在世界里”的会话：从进入世界到退出世界
//!
//! 退出世界再进入时，registry、所有实体、本地玩家都会重建，旧地址全部作废。
//! 所以把“进入世界后才能拿到的东西”都放在 Session 里：进入世界时 `start` 一次，
//! `run` 在检测到世界卸载时返回，主循环丢掉整个 Session（连同后台刷新线程），回到等待状态。

use std::sync::Arc;
use std::time::{ Duration, Instant };

use tokio::sync::watch;
use tracing::info;

use crate::camera;
use crate::error::{ ESPError, Result };
use crate::game::{ self, Actor, LOCAL_PLAYER_VPTR_OFFSET, STORAGE_ID };
use crate::interp;
use crate::kinds::{ self, Kind };
use crate::overlay;
use crate::process::Process;
use crate::projection;
use crate::renderer::{ Outline, Renderer };
use crate::tracker;
use crate::vsync::Pacer;


const MODULE: &str = "Session";

/// 视角默认延后的时间（毫秒），抵消游戏自身的渲染延迟。实测 30 ms 最贴合
const DEFAULT_VIEW_DELAY_MS: u64 = 30;
/// 连续这么多帧读不到本地玩家（或者它的 vptr 变了），就认为世界已经卸载
const LOST_FRAMES_LIMIT: u32 = 30;


pub struct Session {
    process: Arc<Process>,
    player: Actor,
    player_vptr: u64,
    rotation_addr: u64,
    proj_addr: u64,
    actors_rx: watch::Receiver<Arc<Vec<Actor>>>,
    actors: Arc<Vec<Actor>>,
}

impl Session {
    /// 尝试进入会话：在堆里找本地玩家，再由它找到 registry、实体列表、旋转组件和投影矩阵
    ///
    /// 不在世界里（主菜单、加载中）时返回错误，调用方过一会儿再试。
    pub fn start(process: Arc<Process>) -> Result<Self> {
        // 本地玩家：只要进了世界就一定存在，而且它本身就是客户端的 Actor，可以直接通向客户端 registry。
        // 堆里可能残留着已销毁对象的旧数据，所以每个候选都要能完整走通 registry 才算数
        let player_vptr = process.base + LOCAL_PLAYER_VPTR_OFFSET;
        let candidates = game::scan_actors(&process, player_vptr)?;
        let (player_addr, entry_addr) = candidates
            .iter()
            .find_map(|&addr| game::fetch_component_addr(&process, addr, STORAGE_ID).ok().map(|e| (addr, e)))
            .ok_or(ESPError::Custom(MODULE, format!("no usable LocalPlayer among {} candidates", candidates.len()).into()))?;

        // 实体列表只由后台线程 actor-tracker 扫描；等它的第一份结果，从中确认本地玩家
        // 碰撞箱组件的存储池：找不到也不致命，框退回默认尺寸
        let aabb_storage = game::fetch_component_addr(&process, player_addr, game::AABB_SHAPE_ID).ok();
        if aabb_storage.is_none() { tracing::warn!("AABBShapeComponent not found, using default box size"); }
        let mut actors_rx = tracker::spawn(process.clone(), entry_addr, aabb_storage);
        let actors = tracker::wait_first(&mut actors_rx, Duration::from_secs(3))
            .ok_or(ESPError::Custom(MODULE, "actor-tracker produced no actor list".into()))?;
        let player = actors
            .iter()
            .find(|a| a.addr == player_addr && &*a.rtti == "11LocalPlayer")
            .ok_or(ESPError::Custom(MODULE, "LocalPlayer not in actor list".into()))?
            .clone();

        let player_eid = process.read_u32(player.addr + 0x18)?;
        let rotation_addr = game::fetch_rotation_addr(&process, player.addr, player_eid)?;
        // GameRenderer 不一定跟着世界重建，但每次进入世界重新定位一次最稳妥
        let proj_addr = projection::locate(&process)?;
        for a in actors.iter() {
            info!("  actor 0x{:X}: {} / {:?} ({}) {:.2}x{:.2} foot {:.2}", a.addr, a.type_name, a.kind, a.rtti, a.size[0], a.size[1], a.foot_offset);
        }
        info!(
            "Session started: PID {}, base 0x{:X}, LocalPlayer 0x{:X} (EID {}), {} actors, projection 0x{:X}",
            process.pid, process.base, player.addr, player_eid, actors.len(), proj_addr
        );

        Ok(Self { process, player, player_vptr, rotation_addr, proj_addr, actors_rx, actors })
    }

    /// 每帧画框，直到检测到世界卸载才返回（Ok）。渲染出错时返回 Err
    pub fn run(&mut self, renderer: &mut Renderer, pacer: &mut Pacer) -> Result<()> {
        let (w, h) = (overlay::SCREEN_W as f32, overlay::SCREEN_H as f32);
        // 视角取多久以前的值，用来抵消游戏自身的渲染延迟（见 interp::ViewHistory）。MC_ESP_VIEW_DELAY_MS 可以调
        let view_delay = Duration::from_millis(
            std::env::var("MC_ESP_VIEW_DELAY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_VIEW_DELAY_MS),
        );
        let mut view = interp::ViewHistory::new();
        let mut clock = interp::TickClock::new(pacer.frame_interval());
        let (mut pitch, mut yaw) = (0.0f32, 0.0f32);
        // 焦距（像素）= 矩阵的 a × 半屏宽。读失败的帧沿用上一帧的值
        let mut k = projection::read_focal(&self.process, self.proj_addr).unwrap_or(1.0) * w / 2.0;
        let mut outlines = Vec::new();
        // 每帧一次性读回的内存段：(地址, 长度)，以及接收它们的缓冲区
        let mut segments: Vec<(u64, usize)> = Vec::new();
        let mut bytes: Vec<u8> = Vec::new();
        let mut lost_frames = 0;

        loop {
            pacer.wait();

            // 后台线程有新列表就换上（Arc 克隆，不复制列表本身）
            if self.actors_rx.has_changed().unwrap_or(false) {
                self.actors = self.actors_rx.borrow_and_update().clone();
            }

            // 先拿到交换链图像，再读内存：FIFO 下 begin_frame 可能要等垂直同步，等完再读，数据才是最新的
            let frame = renderer.begin_frame()?;

            // ---- 一次系统调用读回本帧要用的所有数据 ----
            // 段 0：本地玩家的 vptr（检测世界是否还在）；段 1：玩家的旋转 (pitch, yaw, prevPitch, prevYaw)；
            // 段 2：玩家的 pos + posPrev；段 3：投影矩阵开头；之后每个实体一段 pos + posPrev
            const VPTR_LEN: usize = 8;
            const ROT_LEN: usize = 16;
            const STATE_LEN: usize = 24;
            const ROT_OFF: usize = VPTR_LEN;
            const PLAYER_OFF: usize = ROT_OFF + ROT_LEN;
            const PROJ_OFF: usize = PLAYER_OFF + STATE_LEN;
            const ACTORS_OFF: usize = PROJ_OFF + projection::HEAD_LEN;
            const FIRST_ACTOR_SEG: usize = 4;
            segments.clear();
            segments.push((self.player.addr, VPTR_LEN));
            segments.push((self.rotation_addr, ROT_LEN));
            segments.push((self.player.state_vector, STATE_LEN));
            segments.push((self.proj_addr, projection::HEAD_LEN));
            for actor in self.actors.iter() {
                segments.push((actor.state_vector, STATE_LEN));
            }
            bytes.resize(segments.iter().map(|s| s.1).sum(), 0);
            let ok = self.process.read_many(&segments, &mut bytes);

            let u64_at = |off: usize| u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap());
            let f32_at = |off: usize| f32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
            let vec3_at = |off: usize| [f32_at(off), f32_at(off + 4), f32_at(off + 8)];

            // 世界还在吗：本地玩家被销毁后，它的内存要么读不了，要么 vptr 变了（析构时改写、或被别的对象复用）。
            // 偶尔一帧读失败不算，连续一段时间都这样才判定为退出世界
            let alive = ok[0] && (u64_at(0) & crate::process::TAG_MASK) == self.player_vptr;
            lost_frames = if alive { 0 } else { lost_frames + 1 };
            if lost_frames >= LOST_FRAMES_LIMIT {
                // 交一帧空画面，免得最后一帧的框一直留在屏幕上
                renderer.end_frame(frame, &[])?;
                info!("LocalPlayer gone, session ended");
                return Ok(());
            }

            // 读写竞争：游戏写旋转组件时，我们可能读到写了一半的值。之前见过整体为 (0,0)，
            // 也可能只有一个分量是 0。真实视角恰好精确等于 0.0 的概率极低，所以任何一个分量
            // 精确为 0 都当作坏数据，沿用上一帧的角度（直接跳过这一帧会让框闪烁）
            if ok[1] {
                let (p, y) = (f32_at(ROT_OFF), f32_at(ROT_OFF + 4));
                let torn = p == 0.0 || y == 0.0 || !p.is_finite() || !y.is_finite();
                if !torn { (pitch, yaw) = (p, y); }
            }
            // FOV 随奔跑、飞行变化，所以焦距每帧都读
            if ok[3] {
                if let Some(focal) = projection::focal_from_bytes(&bytes[PROJ_OFF..PROJ_OFF + projection::HEAD_LEN]) {
                    k = focal * w / 2.0;
                }
            }

            // 本地玩家这一帧没读到（偶发的读取失败）时，沿用上一帧的框，而不是画一帧空画面——那样框会闪一下
            if alive && ok[2] {
                outlines.clear();
                // 把所有实体（含玩家）的 pos 混进指纹，判断是否刚过了 tick 边界，得到插值系数。
                // 视角不放进来：它随游戏的每一帧变化，不是按 tick 变的，放进来会把 tick 计时打乱
                let mut signature = interp::fingerprint(0, &vec3_at(PLAYER_OFF));
                for (i, _) in self.actors.iter().enumerate().filter(|(i, _)| ok[FIRST_ACTOR_SEG + i]) {
                    signature = interp::fingerprint(signature, &vec3_at(ACTORS_OFF + i * STATE_LEN));
                }
                let now = Instant::now();
                let alpha = clock.update(signature, now);

                // 相机（眼睛）位置也要插值：玩家走动时，游戏的相机同样是按 tick 插值移动的
                let eye = interp::lerp(vec3_at(PLAYER_OFF + 12), vec3_at(PLAYER_OFF), alpha);
                // 视角取 view_delay 之前的值，和游戏实际显示在屏幕上的画面对齐
                view.push(now, pitch, yaw);
                let (cam_pitch, cam_yaw) = view.sample(now.checked_sub(view_delay).unwrap_or(now)).unwrap_or((pitch, yaw));
                let camera = camera::Camera { pos: eye, pitch: cam_pitch, yaw: cam_yaw, w, h, k };

                for (i, actor) in self.actors.iter().enumerate() {
                    if !ok[FIRST_ACTOR_SEG + i] || actor.addr == self.player.addr { continue; }
                    if actor.kind == Kind::Ignored { continue; }
                    let off = ACTORS_OFF + i * STATE_LEN;
                    let mut feet = interp::lerp(vec3_at(off + 12), vec3_at(off), alpha);
                    feet[1] -= actor.foot_offset;
                    if feet.iter().any(|v| !v.is_finite()) { continue; }
                    if let Some(rect) = camera.project_box(feet, actor.size[0], actor.size[1]) {
                        outlines.push(Outline {
                            rect,
                            color: kinds::color(actor.kind),
                            thickness: 2.0,
                            fill: kinds::fill(actor.kind),
                        });
                    }
                }
            }

            // 不管本帧有没有读到数据都要 end_frame：acquire 到的图像必须 present 回去，否则交换链会被耗尽
            renderer.end_frame(frame, &outlines)?;
        }
    }
}

