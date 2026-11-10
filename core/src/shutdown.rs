// 優雅關閉：SIGTERM／SIGINT 只設旗標。輸入端看到旗標後停止讀取，
// channel 依序關閉，各階段處理完佇列內的資料後結束，router 送出剩餘 buffer，
// 最後印出統計。送不出去的資料寫入 error.txt。

use std::sync::atomic::{AtomicBool, Ordering};

static REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    // signal handler 內只做 async-signal-safe 的操作
    REQUESTED.store(true, Ordering::SeqCst);
}

pub fn install_handlers() {
    let handler: extern "C" fn(libc::c_int) = on_signal;
    // SAFETY: handler 只寫一個 atomic；sigaction 結構以 zeroed 初始化後設定必要欄位
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handler as usize;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut());
        libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
    }
}

pub fn requested() -> bool {
    REQUESTED.load(Ordering::SeqCst)
}
