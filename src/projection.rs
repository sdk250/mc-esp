//! 游戏实时的投影矩阵：GameRenderer 对象 + 0x458
//!
//! 透视投影矩阵开头的 6 个 float 是 `a, 0, 0, 0, 0, b`，a、b 是水平、垂直方向的
//! 1 / tan(FOV/2)，b / a = 屏幕宽高比。FOV 会随状态变化（静止约 66°、奔跑约 77°，指垂直视野），
//! 所以焦距每帧都要从这里读，不能写死。

use crate::error::{ ESPError, Result };
use crate::game::scan_actors;
use crate::overlay;
use crate::process::Process;


const MODULE: &str = "Projection";

/// GameRenderer（RTTI 12GameRenderer）的 vtable 相对基址的偏移。只对当前游戏版本有效
const GAME_RENDERER_VPTR_OFFSET: u64 = 0x13A58F20;
/// 投影矩阵在 GameRenderer 对象里的偏移
const GAME_RENDERER_PROJ_OFFSET: u64 = 0x458;
/// b / a 和屏幕宽高比之间允许的相对误差
const RATIO_TOLERANCE: f32 = 0.03;


/// 矩阵开头需要读的字节数（6 个 float）
pub const HEAD_LEN: usize = 24;

/// 读矩阵开头的 6 个 float；形状是投影矩阵时返回 a（水平方向的 1 / tan(FOV/2)）
pub fn read_focal(proc: &Process, addr: u64) -> Option<f32> {
    let mut buf = [0u8; HEAD_LEN];
    if proc.read_into(addr, &mut buf) != HEAD_LEN { return None; }
    focal_from_bytes(&buf)
}

/// 从已经读出来的矩阵开头 24 字节里解析 a；形状不对时返回 None
pub fn focal_from_bytes(buf: &[u8]) -> Option<f32> {
    let f: [f32; 6] = std::array::from_fn(|i| f32::from_le_bytes(buf[i * 4..i * 4 + 4].try_into().unwrap()));
    let (a, b) = (f[0], f[5]);
    let aspect = overlay::SCREEN_W as f32 / overlay::SCREEN_H as f32;
    let shaped = (0.3..=1.5).contains(&a)
        && f[1..5].iter().all(|&z| z == 0.0)
        && ((b / a) / aspect - 1.0).abs() <= RATIO_TOLERANCE;
    shaped.then_some(a)
}

/// 偏移失效时，在 GameRenderer 对象里搜索的范围（字节）
const SEARCH_LEN: usize = 0x2000;

/// 启动时定位投影矩阵：按 vptr 扫堆找 GameRenderer（和找牛的方法一样），取 +0x458 处确实是矩阵形状的那个
///
/// 游戏更新后 +0x458 可能失效：这时在对象开头 SEARCH_LEN 字节里按 4 字节步长搜“矩阵形状”，
/// 打印所有候选偏移（方便更新常量），并使用第一个
pub fn locate(proc: &Process) -> Result<u64> {
    let renderers = scan_actors(proc, proc.base + GAME_RENDERER_VPTR_OFFSET)?;
    if let Some(addr) = renderers.iter()
        .map(|&obj| obj + GAME_RENDERER_PROJ_OFFSET)
        .find(|&addr| read_focal(proc, addr).is_some())
    {
        return Ok(addr);
    }

    let mut buf = vec![0u8; SEARCH_LEN];
    for &obj in &renderers {
        let n = proc.read_into(obj, &mut buf);
        let hits: Vec<usize> = (0..n.saturating_sub(HEAD_LEN))
            .step_by(4)
            .filter(|&off| focal_from_bytes(&buf[off..off + HEAD_LEN]).is_some())
            .collect();
        if let Some(&first) = hits.first() {
            let list: Vec<String> = hits.iter()
                .map(|&off| format!("+0x{off:X} (a={:.4})", focal_from_bytes(&buf[off..off + HEAD_LEN]).unwrap()))
                .collect();
            tracing::warn!(
                "projection matrix not at +0x{:X}; GameRenderer 0x{obj:X} has candidates: {}; using +0x{first:X}",
                GAME_RENDERER_PROJ_OFFSET, list.join(", ")
            );
            return Ok(obj + first as u64);
        }
    }
    Err(ESPError::Custom(
        MODULE,
        format!(
            "found {} GameRenderer, but no projection matrix in the first 0x{SEARCH_LEN:X} bytes (game updated?)",
            renderers.len()
        ).into(),
    ))
}
