//! 按屏幕的垂直同步节奏驱动渲染循环（AChoreographer）
//!
//! MAILBOX 模式下 acquire 不会等垂直同步，渲染节奏要自己控制。用 sleep 按固定帧率睡，
//! 和屏幕的刷新时刻对不齐：有的刷新周期里交了两帧（前一帧被丢掉），有的一帧也没交（画面停一帧），
//! 看起来就是框在“掉帧”。AChoreographer 在每次垂直同步时回调，用它来触发每一帧，
//! 每个刷新周期正好画一帧，而且时机固定。
//!
//! AChoreographer 依赖当前线程的 ALooper：先 ALooper_prepare，再用 ALooper_pollOnce 等回调。

use std::ffi::{ c_int, c_void };
use std::sync::atomic::{ AtomicI64, Ordering };


unsafe extern "C" {
    fn ALooper_prepare(opts: c_int) -> *mut c_void;
    fn ALooper_pollOnce(timeout_ms: c_int, out_fd: *mut c_int, out_events: *mut c_int, out_data: *mut *mut c_void) -> c_int;
    fn AChoreographer_getInstance() -> *mut c_void;
    fn AChoreographer_postFrameCallback64(
        choreographer: *mut c_void,
        callback: extern "C" fn(frame_time_nanos: i64, data: *mut c_void),
        data: *mut c_void,
    );
}

/// 回调里把这次垂直同步的时间戳写进 data 指向的 AtomicI64
extern "C" fn on_frame(frame_time_nanos: i64, data: *mut c_void) {
    let slot = unsafe { &*(data as *const AtomicI64) };
    slot.store(frame_time_nanos, Ordering::Release);
}

/// 只能在创建它的线程上使用（ALooper 和 AChoreographer 都是线程相关的）
pub struct Vsync {
    choreographer: *mut c_void,
    /// 回调写入的时间戳；0 表示还没触发。放在 Box 里保证地址固定
    fired: Box<AtomicI64>,
}

impl Vsync {
    /// 当前线程拿不到 AChoreographer 时返回 None，调用方应退回按固定帧率 sleep
    pub fn new() -> Option<Self> {
        unsafe {
            ALooper_prepare(0);
            let choreographer = AChoreographer_getInstance();
            (!choreographer.is_null()).then(|| Self { choreographer, fired: Box::new(AtomicI64::new(0)) })
        }
    }

    /// 阻塞到下一次垂直同步，返回它的时间戳（CLOCK_MONOTONIC，纳秒）
    pub fn wait(&mut self) -> i64 {
        self.fired.store(0, Ordering::Relaxed);
        let data = &*self.fired as *const AtomicI64 as *mut c_void;
        unsafe {
            AChoreographer_postFrameCallback64(self.choreographer, on_frame, data);
            loop {
                // 回调在 pollOnce 内部、当前线程上执行
                ALooper_pollOnce(-1, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
                let t = self.fired.load(Ordering::Acquire);
                if t != 0 { return t; }
            }
        }
    }
}
