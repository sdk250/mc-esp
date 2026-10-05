/// 用屏幕左边缘标定出来的焦距（3120x1440，游戏内 FOV 60）
pub const K_CALIBRATED: f32 = 1519.0;


/// 第一人称相机：位置 + 朝向 + 屏幕参数
pub struct Camera {
    pub pos: [f32; 3], // 眼睛坐标，就是 [LP+0x208]
    pub pitch: f32,    // 度，抬头为负
    pub yaw: f32,      // 度，0 = 面朝 +Z，-90 = 面朝 +X
    pub w: f32,        // 屏幕宽（像素）
    pub h: f32,        // 屏幕高（像素）
    pub k: f32,        // 焦距（像素），由 FOV 算出
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

impl Camera {
    /// 世界坐标 → 屏幕像素；在身后时返回 None
    pub fn world_to_screen(&self, p: [f32; 3]) -> Option<(f32, f32)> {
        let (sp, cp) = self.pitch.to_radians().sin_cos();
        let (sy, cy) = self.yaw.to_radians().sin_cos();

        // 相机的前、右、上三个方向
        let f = [-sy * cp, -sp, cy * cp];
        let r = [-cy, 0.0, -sy];
        let u = [-sy * sp, cp, cy * sp];

        // 眼睛 → 目标
        let d = [p[0] - self.pos[0], p[1] - self.pos[1], p[2] - self.pos[2]];

        // 拆成 右/上/前 三个分量
        let xc = dot(d, r);
        let yc = dot(d, u);
        let zc = dot(d, f);
        if zc < 0.1 {
            return None; // 在身后或贴脸，不画
        }

        // 透视除法 + 换算成像素；屏幕 y 向下增大，所以用减号
        Some((self.w / 2.0 + xc * self.k / zc, self.h / 2.0 - yc * self.k / zc))
    }

    /// 用脚底中心 + 宽高构造碰撞箱，投影 8 个角，返回屏幕上的 [left, top, right, bottom]
    ///
    /// 任何一个角在身后就整体放弃（简单处理，避免框被拉到无穷远）
    pub fn project_box(&self, feet: [f32; 3], width: f32, height: f32) -> Option<[f32; 4]> {
        let hw = width / 2.0;
        let mut rect = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];

        for dx in [-hw, hw] {
            for dy in [0.0, height] {
                for dz in [-hw, hw] {
                    let corner = [feet[0] + dx, feet[1] + dy, feet[2] + dz];
                    let (sx, sy) = self.world_to_screen(corner)?;
                    rect[0] = rect[0].min(sx); // left
                    rect[1] = rect[1].min(sy); // top
                    rect[2] = rect[2].max(sx); // right
                    rect[3] = rect[3].max(sy); // bottom
                }
            }
        }
        Some(rect)
    }
}
