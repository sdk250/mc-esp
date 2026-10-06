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
/// `w`、`h` 是屏幕“自然方向”的尺寸：竖屏手机就传竖屏的宽高，之后的绘制坐标也都是这个方向。
/// 图层一直存活到进程结束，没有对应的销毁函数。
pub fn create_window(w: u32, h: u32) -> Result<NonNull<c_void>> {
    let window = unsafe { overlay_create(w, h) };
    NonNull::new(window).ok_or(ESPError::Custom(MODULE, "overlay_create failed, see stderr.".into()))
}
