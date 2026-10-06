//! 后台线程：定期刷新实体列表，通过 watch 通道把最新结果交给渲染循环
//!
//! 用 watch 而不是 mpsc：渲染循环只关心“最新的列表”，不需要处理中间每一次的更新。
//! watch 只保存最新值，发送方覆盖、接收方随时取，不会积压；mpsc 则要每帧把队列读空才能拿到最新的。

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use tokio::sync::watch;
use tracing::warn;

use crate::game::{ self, Actor, RttiCache };
use crate::process::Process;


/// 实体列表的刷新间隔。新生成的生物最多晚这么久出现框
const REFRESH_INTERVAL: Duration = Duration::from_millis(500);


/// 启动后台刷新线程，返回接收端。线程启动后立即刷新一次，之后每隔 REFRESH_INTERVAL 刷新
///
/// 通道的初始值是空列表；需要第一份真实数据时，用 `wait_first` 等待。
/// 渲染循环退出（接收端被丢弃）后，线程在下一次发送时发现没人接收，自行结束。
pub fn spawn(process: Arc<Process>, entry_addr: u64) -> watch::Receiver<Arc<Vec<Actor>>> {
    let (tx, rx) = watch::channel(Arc::new(Vec::new()));
    thread::Builder::new()
        .name("actor-tracker".into())
        .spawn(move || {
            let mut names = RttiCache::new();
            loop {
                match game::list_actors(&process, entry_addr, &mut names) {
                    Ok(actors) => {
                        if tx.send(Arc::new(actors)).is_err() { break; }
                    }
                    // 偶发的读取失败（比如存储池正在扩容）不致命，下一轮再试
                    Err(e) => warn!("refresh actors failed: {e}"),
                }
                thread::sleep(REFRESH_INTERVAL);
            }
        })
        .expect("spawn actor-tracker thread");
    rx
}

/// 阻塞等待后台线程发出第一份列表，超时返回 None
///
/// watch 的 `changed()` 是 async 函数，这里没有异步运行时，所以用短间隔轮询 `has_changed()`
pub fn wait_first(rx: &mut watch::Receiver<Arc<Vec<Actor>>>, timeout: Duration) -> Option<Arc<Vec<Actor>>> {
    const POLL: Duration = Duration::from_millis(5);
    let mut waited = Duration::ZERO;
    while waited < timeout {
        match rx.has_changed() {
            Ok(true) => return Some(rx.borrow_and_update().clone()),
            Ok(false) => {}
            Err(_) => return None,   // 线程已经退出
        }
        thread::sleep(POLL);
        waited += POLL;
    }
    None
}
