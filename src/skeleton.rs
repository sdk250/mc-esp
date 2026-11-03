//! 骨骼透视：按游戏模型的比例搭一副简笔骨架
//!
//! 不读游戏渲染时真正的骨骼矩阵（那些在渲染线程的动画数据里，逆向成本高），而是用已经验证过的数据自己算：
//!   - 位置、身高、体宽：StateVector + AABBShape；
//!   - 身体朝向：MobBodyRotationComponent；头的朝向：ActorHeadRotationComponent 的 yaw + ActorRotation 的 pitch；
//!   - 四肢摆动：照搬游戏模型的公式 `cos(相位 × 0.6662) × 1.4 × 幅度`，相位和幅度由每 tick 的水平位移累积出来
//!     （和游戏的 WalkAnimation 一样的算法，所以不需要再逆向那个组件）。
//!
//! 坐标约定：以脚底为原点的“身体坐标系”，x 向右、y 向上、z 向前，单位是格。
//! 模型尺寸按 MC 的像素写（1 像素 = 1/16 格），再按实体实际身高缩放。

use std::collections::HashMap;

use crate::camera::Camera;
use crate::interp::direction;
use crate::renderer::Shape;


/// 骨架的体型
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Body {
    /// 两足人形：玩家、僵尸、骷髅、村民……
    Humanoid,
    /// 人形，但手臂平举向前：僵尸一类
    Zombie,
    /// 四足：牛、猪、羊、马、狼……各自的比例不同
    Quadruped(Quad),
    /// 两足鸟类：鸡、鹦鹉
    Bird,
}

/// 四足动物的比例。高度都是碰撞箱高度的倍数，前后方向是碰撞箱宽度的倍数
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quad {
    /// 腿长
    leg: f32,
    /// 脊柱高度
    spine: f32,
    /// 半个身长
    half_len: f32,
    /// 脖子顶端的高度
    neck: f32,
}

/// 牛的模型：腿 12 像素，身体厚 10 像素，头和背差不多高
const QUAD_COW: Quad = Quad { leg: 0.55, spine: 0.8, half_len: 0.6, neck: 0.9 };
/// 马的模型：碰撞箱的高度算上了抬起的头，背比牛低得多，身子更长
const QUAD_HORSE: Quad = Quad { leg: 0.45, spine: 0.62, half_len: 0.5, neck: 0.88 };

const HUMANOID: &[&str] = &[
    "player", "skeleton", "stray", "bogged", "wither_skeleton", "villager", "villager_v2", "wandering_trader",
    "witch", "pillager", "vindicator", "evoker", "illusioner", "piglin", "piglin_brute", "enderman", "npc",
    "armor_stand", "creaking",
];
const ZOMBIE: &[&str] = &[
    "zombie", "zombie_villager", "zombie_villager_v2", "husk", "drowned", "zombie_pigman", "zombified_piglin",
];
const QUADRUPED: &[&str] = &[
    "cow", "mooshroom", "pig", "sheep", "goat", "polar_bear", "panda", "wolf", "fox", "cat", "ocelot", "sniffer",
    "armadillo", "hoglin", "zoglin",
];
const HORSE_LIKE: &[&str] = &[
    "horse", "donkey", "mule", "skeleton_horse", "zombie_horse", "llama", "trader_llama", "camel",
];
const BIRD: &[&str] = &["chicken", "parrot"];

/// 按类型名（去掉 "minecraft:" 前缀后）决定体型；不认识的返回 None，只画框
pub fn body_of(short_name: &str) -> Option<Body> {
    if HUMANOID.contains(&short_name) { Some(Body::Humanoid) }
    else if ZOMBIE.contains(&short_name) { Some(Body::Zombie) }
    else if QUADRUPED.contains(&short_name) { Some(Body::Quadruped(QUAD_COW)) }
    else if HORSE_LIKE.contains(&short_name) { Some(Body::Quadruped(QUAD_HORSE)) }
    else if BIRD.contains(&short_name) { Some(Body::Bird) }
    else { None }
}


/// 每个实体的走路动画状态（跨帧保存）
#[derive(Debug, Clone, Copy, Default)]
struct Walk {
    /// 累积的相位（游戏里的 walkAnimPos）
    phase: f32,
    /// 摆动幅度 0..=1（游戏里的 walkAnimSpeed），平滑地跟随目标值
    amp: f32,
}

/// 所有实体的走路动画状态，按 Actor 地址索引
#[derive(Default)]
pub struct WalkTracker {
    walks: HashMap<u64, Walk>,
}

impl WalkTracker {
    /// 推进一帧。`step` = 这一 tick 的水平位移（格），`ticks` = 这一帧经过了多少 tick（帧间隔 / 50 ms）
    ///
    /// 游戏每 tick：目标幅度 = min(位移 × 4, 1)，幅度向目标靠近 40%，相位 += 幅度。这里按帧连续地做同样的事
    fn advance(&mut self, addr: u64, step: f32, ticks: f32) -> Walk {
        let w = self.walks.entry(addr).or_default();
        let target = (step * 4.0).min(1.0);
        w.amp += (target - w.amp) * (1.0 - 0.6f32.powf(ticks));
        w.phase += w.amp * ticks;
        *w
    }

    /// 丢掉已经不在列表里的实体
    pub fn retain(&mut self, alive: impl Fn(u64) -> bool) {
        self.walks.retain(|&addr, _| alive(addr));
    }
}


/// 眼睛在头中心之上多高（头半径的倍数）
const EYE_RAISE: f32 = 0.4;
/// 视线长度（头半径的倍数）
const GAZE_LEN: f32 = 4.5;
/// 视线的颜色（橙色），所有实体统一，不跟随框的颜色
const GAZE_COLOR: [f32; 4] = [1.0, 0.55, 0.0, 1.0];

/// 画一副骨架需要的全部输入（都已经插值好）
pub struct Pose {
    pub addr: u64,
    pub body: Body,
    /// 脚底中心（世界坐标）
    pub feet: [f32; 3],
    /// 碰撞箱的宽、高
    pub size: [f32; 2],
    pub body_yaw: f32,
    pub head_yaw: f32,
    pub head_pitch: f32,
    /// 这一 tick 的水平位移（格）
    pub step: f32,
}

/// 身体坐标系 → 世界坐标
struct Frame {
    origin: [f32; 3],
    right: [f32; 3],
    up: [f32; 3],
    fwd: [f32; 3],
}

impl Frame {
    fn at(&self, [x, y, z]: [f32; 3]) -> [f32; 3] {
        std::array::from_fn(|i| self.origin[i] + self.right[i] * x + self.up[i] * y + self.fwd[i] * z)
    }
}

/// 绕 x 轴（身体的左右方向）转 θ 弧度后，从关节往下 `len` 的那个点（身体坐标）。θ > 0 向前摆
fn swing(pivot: [f32; 3], len: f32, theta: f32) -> [f32; 3] {
    [pivot[0], pivot[1] - len * theta.cos(), pivot[2] + len * theta.sin()]
}

/// 生成一副骨架的线段和头部圆圈，追加到 `out`
pub fn draw(
    pose: &Pose,
    walks: &mut WalkTracker,
    ticks: f32,
    camera: &Camera,
    color: [f32; 4],
    thickness: f32,
    out: &mut Vec<Shape>,
) {
    let walk = walks.advance(pose.addr, pose.step, ticks);
    // 游戏模型的摆腿公式
    let leg = (walk.phase * 0.6662).cos() * 1.4 * walk.amp;

    let fwd = direction(pose.body_yaw, 0.0);
    let frame = Frame {
        origin: pose.feet,
        right: [-fwd[2], 0.0, fwd[0]],
        up: [0.0, 1.0, 0.0],
        fwd,
    };
    // 头的朝向直接在世界坐标里算：前方 = 头的 yaw + pitch，上方 = pitch 再抬 90°
    let head_fwd = direction(pose.head_yaw, pose.head_pitch);
    let head_up = direction(pose.head_yaw, pose.head_pitch - 90.0);

    // 先收集世界坐标里的线段，最后统一投影
    let mut segs: Vec<([f32; 3], [f32; 3])> = Vec::with_capacity(12);
    let mut line = |a: [f32; 3], b: [f32; 3]| segs.push((a, b));
    // 脖子顶端、头的中心、头的半径（格）
    let (neck_top, head_center, head_radius): ([f32; 3], [f32; 3], f32);

    match pose.body {
        Body::Humanoid | Body::Zombie => {
            // 人形模型高 32 像素：腿 12、身体 12、头 8；肩宽 ±5（手臂中心），髋宽 ±2
            let u = pose.size[1] / 32.0;
            let p = |x: f32, y: f32, z: f32| [x * u, y * u, z * u];
            let hip = p(0.0, 12.0, 0.0);
            let neck = p(0.0, 24.0, 0.0);
            line(frame.at(hip), frame.at(neck));
            // 髋、肩横线
            line(frame.at(p(-2.0, 12.0, 0.0)), frame.at(p(2.0, 12.0, 0.0)));
            line(frame.at(p(-5.0, 22.0, 0.0)), frame.at(p(5.0, 22.0, 0.0)));
            // 腿：左右相反
            for (x, theta) in [(-2.0, leg), (2.0, -leg)] {
                let pivot = p(x, 12.0, 0.0);
                line(frame.at(pivot), frame.at(swing(pivot, 12.0 * u, theta)));
            }
            // 手臂：和同侧的腿反向摆，幅度是腿的一半多；僵尸平举向前（90°）
            let raised = if pose.body == Body::Zombie { std::f32::consts::FRAC_PI_2 } else { 0.0 };
            for (x, theta) in [(-5.0, -leg * 0.7), (5.0, leg * 0.7)] {
                let pivot = p(x, 22.0, 0.0);
                line(frame.at(pivot), frame.at(swing(pivot, 10.0 * u, raised + theta * if raised > 0.0 { 0.2 } else { 1.0 })));
            }
            // 头：脖子往上 4 像素是头的中心
            neck_top = frame.at(neck);
            head_center = std::array::from_fn(|i| neck_top[i] + head_up[i] * 4.0 * u);
            head_radius = 4.0 * u;
        }
        Body::Quadruped(q) => {
            let (w, h) = (pose.size[0], pose.size[1]);
            let leg_len = h * q.leg;
            let spine_y = h * q.spine;
            let half = w * q.half_len;
            let back = [0.0, spine_y, -half];
            let front = [0.0, spine_y, half];
            line(frame.at(back), frame.at(front));
            // 四条腿：对角线上的两条同相（右后 + 左前，左后 + 右前）。从脊柱斜向下连到腿根，再往下是腿
            let lx = w * 0.3;
            for (x, z, theta) in [
                (lx, -half * 0.85, leg), (-lx, -half * 0.85, -leg),
                (lx, half * 0.85, -leg), (-lx, half * 0.85, leg),
            ] {
                let pivot = [x, leg_len, z];
                line(frame.at([0.0, spine_y, z]), frame.at(pivot));
                line(frame.at(pivot), frame.at(swing(pivot, leg_len, theta)));
            }
            // 脖子从前肩斜向上；头的中心在脖子前方，跟着头的朝向转
            let neck = frame.at([0.0, h * q.neck, half + w * 0.1]);
            line(frame.at(front), neck);
            neck_top = neck;
            head_center = std::array::from_fn(|i| neck[i] + head_fwd[i] * h * 0.15);
            head_radius = h * 0.12;
        }
        Body::Bird => {
            // 鸡的模型（高约 16 像素）：腿 5 像素，身体是一段略微前高后低的躯干，脖子竖直向上，头在最高处
            let (w, h) = (pose.size[0], pose.size[1]);
            let leg_len = h * 0.3;
            let tail = [0.0, h * 0.45, -w * 0.5];
            let chest = [0.0, h * 0.5, w * 0.35];
            line(frame.at(tail), frame.at(chest));
            // 两条腿，从躯干中部的腿根往下
            for (x, theta) in [(-w * 0.15, leg), (w * 0.15, -leg)] {
                let pivot = [x, leg_len, 0.0];
                line(frame.at([0.0, h * 0.47, 0.0]), frame.at(pivot));
                line(frame.at(pivot), frame.at(swing(pivot, leg_len, theta)));
            }
            let neck = frame.at([0.0, h * 0.75, w * 0.4]);
            line(frame.at(chest), neck);
            neck_top = neck;
            head_center = std::array::from_fn(|i| neck[i] + head_up[i] * h * 0.1);
            head_radius = h * 0.1;
        }
    }

    // 头：脖子连到头的中心，再从脸的正面画两条视线（两只眼睛），跟着头的 yaw 和 pitch 转
    segs.push((neck_top, head_center));
    let head_right = {
        let f = direction(pose.head_yaw, 0.0);
        [-f[2], 0.0, f[0]]
    };
    let mut gazes = Vec::with_capacity(2);
    for side in [-1.0, 1.0] {
        let eye: [f32; 3] = std::array::from_fn(|i| {
            head_center[i] + head_fwd[i] * head_radius
                + head_right[i] * side * head_radius * 0.5
                + head_up[i] * head_radius * EYE_RAISE
        });
        let gaze: [f32; 3] = std::array::from_fn(|i| eye[i] + head_fwd[i] * head_radius * GAZE_LEN);
        gazes.push((eye, gaze));
    }

    let gazes = gazes.into_iter().map(|g| (g, GAZE_COLOR));
    for ((a, b), color) in segs.into_iter().map(|g| (g, color)).chain(gazes) {
        if let (Some(p), Some(q)) = (camera.world_to_screen(a), camera.world_to_screen(b)) {
            out.push(Shape::Line { from: [p.0, p.1], to: [q.0, q.1], color, thickness });
        }
    }
}
