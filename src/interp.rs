//! tick 插值：让框的位置和游戏画面里的模型一致
//!
//! 游戏逻辑每秒更新 20 次（每 50 ms 一个 tick），StateVectorComponent 里的 pos 只在 tick 时更新，
//! 同时把旧值存进 posPrev。游戏渲染时用的不是 pos，而是按“这一帧处在 tick 内的哪个时刻”插值出来的
//!     画面位置 = posPrev + (pos - posPrev) × α，α ∈ [0, 1]
//! 直接用 pos 画框，框会每 50 ms 跳一次，而且比模型超前最多一个 tick。
//!
//! α 要靠自己估计。tick 是等间隔的，所以只要知道“某一个 tick 从什么时候开始”（锚点），
//! 任何时刻的 α = ((现在 - 锚点) mod 50 ms) / 50 ms。锚点这样得到：
//!   - 所有实体都在同一个 tick 里更新位置，只要发现任何一个 pos 变了，就说明刚刚过了一个 tick 边界
//!     （视角不算：它随游戏的每一帧更新，不是按 tick）；
//!   - 每次检测到边界，就把锚点往检测到的时刻拉近一点（锁相），而不是直接跳过去：
//!     我们每帧（约 8 ms）才看一次，检测时刻本身有误差，直接跳会让 α 抖动；
//!   - 没有任何东西在变的时候，锚点保持不动，α 照样按 50 ms 周期推进。

use std::time::{ Duration, Instant };


/// 一个游戏 tick 的时长
const TICK: Duration = Duration::from_millis(50);


pub struct TickClock {
    /// 某一个 tick 的开始时刻；之后每隔 TICK 一个 tick
    anchor: Instant,
    last_signature: u64,
    /// 检测时刻相对真实 tick 边界的平均延迟：我们每帧才检查一次，平均晚半帧
    detect_delay: Duration,
}

impl TickClock {
    pub fn new(frame_interval: Duration) -> Self {
        Self { anchor: Instant::now(), last_signature: 0, detect_delay: frame_interval / 2 }
    }

    /// 每帧调用一次。`signature` 是本帧读到的所有会随 tick 变化的值的指纹（见 `fingerprint`），返回插值系数 α
    pub fn update(&mut self, signature: u64, now: Instant) -> f32 {
        let tick = TICK.as_secs_f32();
        if signature != self.last_signature {
            self.last_signature = signature;
            // 估计的真实边界时刻，和锚点推算出的最近一个边界比较，误差折算到 [-tick/2, tick/2)
            let boundary = now.checked_sub(self.detect_delay).unwrap_or(now);
            let since = boundary.saturating_duration_since(self.anchor).as_secs_f32();
            let error = (since + tick / 2.0).rem_euclid(tick) - tick / 2.0;
            if error.abs() < tick * 0.3 {
                // 误差不大：每次只修正 30%，平滑掉检测时刻的抖动
                self.anchor += Duration::from_secs_f32((error * 0.3).max(0.0));
                self.anchor -= Duration::from_secs_f32((-error * 0.3).max(0.0));
            } else {
                // 误差太大（刚启动、游戏卡顿过）：直接对齐
                self.anchor = boundary;
            }
        }
        let since = now.saturating_duration_since(self.anchor).as_secs_f32();
        since.rem_euclid(tick) / tick
    }
}

/// 把一组 float 的比特位混合成一个 64 位指纹。任何一个值变了，指纹几乎一定会变
pub fn fingerprint(acc: u64, values: &[f32]) -> u64 {
    values.iter().fold(acc, |h, v| (h ^ v.to_bits() as u64).wrapping_mul(0x100000001b3))
}

pub fn lerp(prev: [f32; 3], cur: [f32; 3], alpha: f32) -> [f32; 3] {
    std::array::from_fn(|i| prev[i] + (cur[i] - prev[i]) * alpha)
}

/// 角度插值，走最短的方向：yaw 在 ±180° 处会跳变，直接线性插值会绕一大圈
pub fn lerp_angle(prev: f32, cur: f32, alpha: f32) -> f32 {
    let delta = (cur - prev + 540.0).rem_euclid(360.0) - 180.0;
    prev + delta * alpha
}

/// 视角的历史记录，用来“延后”取视角，和游戏画面对齐
///
/// 视角不是按 tick 更新的：触屏滑动时它随游戏的每一帧更新（实测约 60 次/秒，间隔 8~17 ms）。
/// 我们的覆盖层几乎没有渲染延迟，而游戏的一帧从读视角到真正显示在屏幕上，要经过它自己的渲染管线，
/// 会晚一两帧。直接用最新视角，框就会“跑在画面前面”；所以记下每次视角变化的时刻，
/// 绘制时取 `delay` 之前的视角（在相邻两次变化之间线性插值）。
pub struct ViewHistory {
    /// (检测到变化的时刻, pitch, yaw)，按时间先后排列
    samples: std::collections::VecDeque<(Instant, f32, f32)>,
}

impl ViewHistory {
    /// 只保留这么久以内的记录
    const KEEP: Duration = Duration::from_millis(300);

    pub fn new() -> Self {
        Self { samples: std::collections::VecDeque::new() }
    }

    /// 每帧调用：视角和上一次记录的不同时，记下这个新值
    pub fn push(&mut self, now: Instant, pitch: f32, yaw: f32) {
        if self.samples.back().is_some_and(|&(_, p, y)| p == pitch && y == yaw) { return; }
        self.samples.push_back((now, pitch, yaw));
        while self.samples.len() > 2 && now.duration_since(self.samples[1].0) > Self::KEEP {
            self.samples.pop_front();
        }
    }

    /// 取 `t` 时刻的视角：落在两次记录之间就插值，比最早的还早就用最早的，比最新的还晚就用最新的
    pub fn sample(&self, t: Instant) -> Option<(f32, f32)> {
        let &(_, last_p, last_y) = self.samples.back()?;
        for i in 0..self.samples.len().saturating_sub(1) {
            let ((t0, p0, y0), (t1, p1, y1)) = (self.samples[i], self.samples[i + 1]);
            if t < t0 { return Some((p0, y0)); }
            if t < t1 {
                let a = t.duration_since(t0).as_secs_f32() / t1.duration_since(t0).as_secs_f32();
                return Some((p0 + (p1 - p0) * a, lerp_angle(y0, y1, a)));
            }
        }
        Some((last_p, last_y))
    }
}
