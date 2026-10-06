//! 覆盖图层：调用 cpp/overlay.cpp，在 SurfaceFlinger 最顶层创建一个透明、触摸穿透的图层

use std::ffi::c_void;
use std::ptr::NonNull;

use crate::error::{ ESPError, Result };


const MODULE: &str = "Overlay";


unsafe extern "C" {
    /// 定义在 cpp/overlay.cpp，由 build.rs 编译并静态链接进来。失败时返回空指针，原因打印在 stderr
    fn overlay_create(w: u32, h: u32) -> *mut c_void;
}

/// 创建图层，返回它的 ANativeWindow*（不透明指针，交给 Vulkan 用）
///
/// `w`、`h` 是图层在逻辑显示坐标里的尺寸（见 SCREEN_W / SCREEN_H），之后的绘制坐标也是这个坐标系。
/// 图层一直存活到进程结束，没有对应的销毁函数。
pub fn create_window(w: u32, h: u32) -> Result<NonNull<c_void>> {
    let window = unsafe { overlay_create(w, h) };
    NonNull::new(window).ok_or(ESPError::Custom(MODULE, "overlay_create failed, see stderr.".into()))
}


/// 游戏画面（横屏）的逻辑尺寸，也是图层和绘制坐标系的尺寸
///
/// 根图层的位置和大小用的是“逻辑显示坐标”：它跟着屏幕旋转，横屏时就是 3120x1440。
/// 所以图层直接按横屏尺寸创建，ESP 算出的框不需要做任何旋转变换。
/// （代价：如果把手机转回竖屏，图层会有一部分落到屏幕外。游戏固定横屏，暂不处理）
pub const SCREEN_W: u32 = 3120;
pub const SCREEN_H: u32 = 1440;
