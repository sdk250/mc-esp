/// 第一人称相机：位置 + 朝向 + 屏幕参数
pub struct Camera {
    pub pos: [f32; 3], // 眼睛坐标，就是 [LP+0x228]
    pub pitch: f32,    // 度，抬头为负
    pub yaw: f32,      // 度，0 = 面朝 +Z，-90 = 面朝 +X
    pub w: f32,        // 屏幕宽（像素）
    pub h: f32,        // 屏幕高（像素）
    pub k: f32,        // 焦距（像素）= 投影矩阵的 a × 半屏宽
}

/// 近平面：相机前方小于这个距离（格）的点不投影
const NEAR: f32 = 0.1;

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

impl Camera {
    /// 世界坐标 → 相机坐标（右、上、前）
    fn to_view(&self, p: [f32; 3]) -> [f32; 3] {
        let (sp, cp) = self.pitch.to_radians().sin_cos();
        let (sy, cy) = self.yaw.to_radians().sin_cos();

        // 相机的前、右、上三个方向
        let f = [-sy * cp, -sp, cy * cp];
        let r = [-cy, 0.0, -sy];
        let u = [-sy * sp, cp, cy * sp];

        // 眼睛 → 目标
        let d = [p[0] - self.pos[0], p[1] - self.pos[1], p[2] - self.pos[2]];
        [dot(d, r), dot(d, u), dot(d, f)]
    }

    /// 相机坐标 → 屏幕像素：透视除法 + 换算成像素；屏幕 y 向下增大，所以用减号
    fn view_to_screen(&self, [x, y, z]: [f32; 3]) -> (f32, f32) {
        (self.w / 2.0 + x * self.k / z, self.h / 2.0 - y * self.k / z)
    }

    /// 世界坐标 → 屏幕像素；在身后时返回 None
    pub fn world_to_screen(&self, p: [f32; 3]) -> Option<(f32, f32)> {
        let v = self.to_view(p);
        if v[2] < NEAR {
            return None; // 在身后或贴脸，不画
        }
        Some(self.view_to_screen(v))
    }

    /// 投影一条线段。一端在身后时裁到近平面上，这样站在框里或贴着框时，框的边不会整条消失；
    /// 两端都在身后才返回 None
    pub fn project_segment(&self, a: [f32; 3], b: [f32; 3]) -> Option<([f32; 2], [f32; 2])> {
        let (mut va, mut vb) = (self.to_view(a), self.to_view(b));
        if va[2] < NEAR && vb[2] < NEAR { return None; }
        if va[2] < NEAR || vb[2] < NEAR {
            let t = (NEAR - va[2]) / (vb[2] - va[2]);
            let hit: [f32; 3] = std::array::from_fn(|i| va[i] + (vb[i] - va[i]) * t);
            if va[2] < NEAR { va = hit; } else { vb = hit; }
        }
        let (p, q) = (self.view_to_screen(va), self.view_to_screen(vb));
        Some(([p.0, p.1], [q.0, q.1]))
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
