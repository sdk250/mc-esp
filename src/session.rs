//! 一次“在世界里”的会话：从进入世界到退出世界
//!
//! 退出世界再进入时，registry、所有实体、本地玩家都会重建，旧地址全部作废。
//! 所以把“进入世界后才能拿到的东西”都放在 Session 里：进入世界时 `start` 一次，
//! `run` 在检测到世界卸载时返回，主循环丢掉整个 Session（连同后台刷新线程），回到等待状态。

use std::sync::Arc;
use std::time::{ Duration, Instant };

use android_overlay::{ Overlay, Shape };
use tokio::sync::watch;
use tracing::info;

use crate::camera;
use crate::error::{ ESPError, Result };
use crate::game::{ self, Actor, LOCAL_PLAYER_VPTR_OFFSET, STORAGE_ID };
use crate::interp;
use crate::kinds::{ self, Kind };
use crate::process::Process;
use crate::projection;
use crate::skeleton;
use crate::tracker;


const MODULE: &str = "Session";

/// 视角默认延后的时间（毫秒），抵消游戏自身的渲染延迟。实测 30 ms 最贴合
const DEFAULT_VIEW_DELAY_MS: u64 = 30;
/// 连续这么多帧读不到本地玩家（或者它的 vptr 变了），就认为世界已经卸载
const LOST_FRAMES_LIMIT: u32 = 30;


/// 方框的样式，由环境变量 MC_ESP_BOX 决定
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BoxMode {
    /// 0 / off / false：不画方框（名字标签和骨架照常画）
    Off,
    /// 2d：屏幕上的矩形
    Flat,
    /// 其他值或不设置：3D 方框
    Solid,
}

impl BoxMode {
    fn from_env() -> Self {
        match std::env::var("MC_ESP_BOX").unwrap_or_default().to_ascii_lowercase().as_str() {
            "0" | "off" | "false" => Self::Off,
            "2d" => Self::Flat,
            _ => Self::Solid,
        }
    }
}


pub struct Session {
    process: Arc<Process>,
    player: Actor,
    player_vptr: u64,
    rotation_addr: u64,
    proj_addr: u64,
    /// 屏幕宽高比，校验投影矩阵的形状时用
    aspect: f32,
    actors_rx: watch::Receiver<Arc<Vec<Actor>>>,
    actors: Arc<Vec<Actor>>,
}

impl Session {
    /// 尝试进入会话：在堆里找本地玩家，再由它找到 registry、实体列表、旋转组件和投影矩阵
    ///
    /// 不在世界里（主菜单、加载中）时返回错误，调用方过一会儿再试。
    pub fn start(process: Arc<Process>, aspect: f32) -> Result<Self> {
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
        let pools = game::Pools::find(&process, player_addr);
        if pools.aabb.is_none() { tracing::warn!("AABBShapeComponent not found, using default box size"); }
        info!("component pools: {pools:X?}");
        let mut actors_rx = tracker::spawn(process.clone(), entry_addr, pools);
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
        let proj_addr = projection::locate(&process, aspect)?;
        for a in actors.iter() {
            info!("  actor 0x{:X}: {} / {:?} ({}) {:.2}x{:.2} foot {:.2}", a.addr, a.type_name, a.kind, a.rtti, a.size[0], a.size[1], a.foot_offset);
        }
        if std::env::var_os("MC_ESP_PROBE").is_some() {
            game::probe_components(&process, player.addr, &actors);
        }
        info!(
            "Session started: PID {}, base 0x{:X}, LocalPlayer 0x{:X} (EID {}), {} actors, projection 0x{:X}",
            process.pid, process.base, player.addr, player_eid, actors.len(), proj_addr
        );

        Ok(Self { process, player, player_vptr, rotation_addr, proj_addr, aspect, actors_rx, actors })
    }

    /// 每帧画框，直到检测到世界卸载才返回（Ok）。渲染出错时返回 Err
    pub fn run(&mut self, overlay: &mut Overlay) -> Result<()> {
        let (w, h) = overlay.size();
        let (w, h) = (w as f32, h as f32);
        // 视角取多久以前的值，用来抵消游戏自身的渲染延迟（见 interp::ViewHistory）。MC_ESP_VIEW_DELAY_MS 可以调
        let view_delay = Duration::from_millis(
            std::env::var("MC_ESP_VIEW_DELAY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_VIEW_DELAY_MS),
        );
        let box_mode = BoxMode::from_env();
        info!("box: {box_mode:?} (MC_ESP_BOX)");
        // MC_ESP_SKELETON=1 / on / true 时画骨架；默认只画视线
        let bones = matches!(
            std::env::var("MC_ESP_SKELETON").unwrap_or_default().to_ascii_lowercase().as_str(),
            "1" | "on" | "true"
        );
        info!("skeleton: {} (MC_ESP_SKELETON)", if bones { "on" } else { "off, gaze only" });
        let mut view = interp::ViewHistory::new();
        let mut clock = interp::TickClock::new(overlay.frame_interval());
        let (mut pitch, mut yaw) = (0.0f32, 0.0f32);
        // 焦距（像素）= 矩阵的 a × 半屏宽。读失败的帧沿用上一帧的值
        let mut k = projection::read_focal(&self.process, self.proj_addr, self.aspect).unwrap_or(1.0) * w / 2.0;
        let mut shapes: Vec<Shape> = Vec::new();
        // 每帧一次性读回的内存段：(地址, 长度)，以及接收它们的缓冲区
        let mut segments: Vec<(u64, usize)> = Vec::new();
        let mut bytes: Vec<u8> = Vec::new();
        let mut lost_frames = 0;
        // 每个实体的走路动画状态；ticks = 上一帧到这一帧经过了多少 tick
        let mut walks = skeleton::WalkTracker::default();
        let mut last_frame = Instant::now();

        loop {
            // 回调返回 true 表示世界已经卸载。回调要等完垂直同步才执行，在里面读内存，数据才是最新的
            let ended = overlay.frame(|frame| {
                // 后台线程有新列表就换上（Arc 克隆，不复制列表本身）
                if self.actors_rx.has_changed().unwrap_or(false) {
                    self.actors = self.actors_rx.borrow_and_update().clone();
                    let actors = &self.actors;
                    walks.retain(|addr| actors.iter().any(|a| a.addr == addr));
                }

                // ---- 一次系统调用读回本帧要用的所有数据 ----
                // 段 0：本地玩家的 vptr（检测世界是否还在）；段 1：玩家的旋转 (pitch, yaw, prevPitch, prevYaw)；
                // 段 2：玩家的 pos + posPrev；段 3：投影矩阵开头；
                // 之后每个实体 4 段：pos + posPrev、旋转（16）、身体朝向（8）、头部朝向（8）
                const VPTR_LEN: usize = 8;
                const ROT_LEN: usize = 16;
                const STATE_LEN: usize = 24;
                const ROT_OFF: usize = VPTR_LEN;
                const PLAYER_OFF: usize = ROT_OFF + ROT_LEN;
                const PROJ_OFF: usize = PLAYER_OFF + STATE_LEN;
                const ACTORS_OFF: usize = PROJ_OFF + projection::HEAD_LEN;
                const FIRST_ACTOR_SEG: usize = 4;
                const SEGS_PER_ACTOR: usize = 4;
                const A_ROT: usize = STATE_LEN;
                const A_BODY: usize = A_ROT + 16;
                const A_HEAD: usize = A_BODY + 8;
                const ACTOR_LEN: usize = A_HEAD + 8;
                segments.clear();
                segments.push((self.player.addr, VPTR_LEN));
                segments.push((self.rotation_addr, ROT_LEN));
                segments.push((self.player.state_vector, STATE_LEN));
                segments.push((self.proj_addr, projection::HEAD_LEN));
                for actor in self.actors.iter() {
                    // 没有某个组件时，读一段肯定可读的内存（它自己的 StateVector）占位，结果不用。
                    // 不能用地址 0：读失败会让 read_many 对后面所有段逐个重试
                    let sv = actor.state_vector;
                    segments.push((sv, STATE_LEN));
                    segments.push((actor.rotation.unwrap_or(sv), 16));
                    segments.push((actor.body_rotation.unwrap_or(sv), 8));
                    segments.push((actor.head_rotation.unwrap_or(sv), 8));
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
                    // 什么都不画，交一帧空画面，免得最后一帧的框一直留在屏幕上
                    return true;
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
                    if let Some(focal) = projection::focal_from_bytes(&bytes[PROJ_OFF..PROJ_OFF + projection::HEAD_LEN], self.aspect) {
                        k = focal * w / 2.0;
                    }
                }

                // 本地玩家这一帧没读到（偶发的读取失败）时，沿用上一帧的框，而不是画一帧空画面——那样框会闪一下
                if alive && ok[2] {
                    shapes.clear();
                    // 把所有实体（含玩家）的 pos 混进指纹，判断是否刚过了 tick 边界，得到插值系数。
                    // 视角不放进来：它随游戏的每一帧变化，不是按 tick 变的，放进来会把 tick 计时打乱
                    let mut signature = interp::fingerprint(0, &vec3_at(PLAYER_OFF));
                    for (i, _) in self.actors.iter().enumerate().filter(|(i, _)| ok[FIRST_ACTOR_SEG + i * SEGS_PER_ACTOR]) {
                        signature = interp::fingerprint(signature, &vec3_at(ACTORS_OFF + i * ACTOR_LEN));
                    }
                    let now = Instant::now();
                    let alpha = clock.update(signature, now);
                    let ticks = (now - last_frame).as_secs_f32() / 0.05;
                    last_frame = now;

                    // 相机（眼睛）位置也要插值：玩家走动时，游戏的相机同样是按 tick 插值移动的
                    let eye = interp::lerp(vec3_at(PLAYER_OFF + 12), vec3_at(PLAYER_OFF), alpha);
                    // 视角取 view_delay 之前的值，和游戏实际显示在屏幕上的画面对齐
                    view.push(now, pitch, yaw);
                    let (cam_pitch, cam_yaw) = view.sample(now.checked_sub(view_delay).unwrap_or(now)).unwrap_or((pitch, yaw));
                    let camera = camera::Camera { pos: eye, pitch: cam_pitch, yaw: cam_yaw, w, h, k };

                    for (i, actor) in self.actors.iter().enumerate() {
                        let seg = FIRST_ACTOR_SEG + i * SEGS_PER_ACTOR;
                        if !ok[seg] || actor.addr == self.player.addr { continue; }
                        if actor.kind == Kind::Ignored { continue; }
                        let off = ACTORS_OFF + i * ACTOR_LEN;
                        let mut feet = interp::lerp(vec3_at(off + 12), vec3_at(off), alpha);
                        feet[1] -= actor.foot_offset;
                        if feet.iter().any(|v| !v.is_finite()) { continue; }
                        let color = kinds::color(actor.kind);
                        let fill = kinds::fill(actor.kind);
                        // 体型和朝向：只有认识体型、而且朝向组件都读到了的生物才有
                        let angle = |o: usize| f32_at(off + o);
                        let have = |o: usize, addr: Option<u64>| addr.is_some() && ok[seg + o];
                        let body = skeleton::body_of(kinds::short_name(&actor.type_name))
                            .filter(|_| have(2, actor.body_rotation) && have(3, actor.head_rotation));
                        let body_yaw = interp::lerp_angle(angle(A_BODY + 4), angle(A_BODY), alpha);

                        if box_mode == BoxMode::Solid {
                            // 认识体型的跟着身体转、罩住整个模型；其他实体就是不会转的碰撞箱
                            let (yaw, extents) = match body {
                                Some(body) => (body_yaw, skeleton::Extents::of_body(body, actor.size)),
                                None => (0.0, skeleton::Extents::of_hitbox(actor.size)),
                            };
                            skeleton::draw_box(feet, yaw, extents, &camera, color, 2.0, fill, &mut shapes);
                        }
                        if let Some(rect) = camera.project_box(feet, actor.size[0], actor.size[1]) {
                            if box_mode == BoxMode::Flat {
                                shapes.push(Shape::Rect { rect, color, thickness: 2.0, fill });
                            }
                            // 框的正上方：名字 + 距离（从眼睛到脚底，米）
                            let dist = (0..3).map(|j| (feet[j] - eye[j]).powi(2)).sum::<f32>().sqrt();
                            shapes.push(Shape::Text {
                                anchor: [(rect[0] + rect[2]) / 2.0, rect[1] - 4.0],
                                text: format!("{} {:.0}m", kinds::short_name(&actor.type_name), dist),
                                color,
                            });
                        }

                        if let Some(body) = body {
                            // 两个组件都是 (当前, 上一 tick)；ActorRotation 是 (pitch, yaw, prevPitch, prevYaw)
                            let (pitch, prev_pitch) = if have(1, actor.rotation) {
                                (angle(A_ROT), angle(A_ROT + 8))
                            } else {
                                (0.0, 0.0)
                            };
                            let (cur, prev) = (vec3_at(off), vec3_at(off + 12));
                            let pose = skeleton::Pose {
                                addr: actor.addr,
                                body,
                                feet,
                                size: actor.size,
                                body_yaw,
                                head_yaw: interp::lerp_angle(angle(A_HEAD + 4), angle(A_HEAD), alpha),
                                head_pitch: prev_pitch + (pitch - prev_pitch) * alpha,
                                step: (cur[0] - prev[0]).hypot(cur[2] - prev[2]),
                            };
                            skeleton::draw(&pose, bones, &mut walks, ticks, &camera, color, 3.0, &mut shapes);
                        }
                    }
                }

                frame.draw(&shapes);
                false
            })?;
            if ended {
                info!("LocalPlayer gone, session ended");
                return Ok(());
            }
        }
    }
}

