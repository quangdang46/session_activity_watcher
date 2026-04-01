pub mod alerter;
pub mod event_bus;
pub mod jsonl_tail;
pub mod process_monitor;
pub mod runtime;
pub mod state_machine;
pub mod store;
pub mod watcher;

pub use alerter::{
    alert_log_path, AlertActionExecutor, AlertContext, AlertNotification, Alerter, AlerterConfig,
    ScopeLeakAction, StuckAction, DEFAULT_ALERT_RATE_LIMIT,
};
pub use event_bus::{EventBus, EventBusMetrics, Receiver, EVENT_BUS_CAPACITY};
pub use jsonl_tail::{
    JsonlTailMode, JsonlTailer, JsonlTailerOptions, JSONL_TAIL_FALLBACK_POLL_INTERVAL,
    JSONL_TAIL_NOTIFY_IDLE_TIMEOUT, JSONL_TAIL_POLL_INTERVAL,
};
pub use process_monitor::{ProcessMonitor, DEFAULT_PROCESS_POLL_INTERVAL};
pub use runtime::{
    list_alive_session_selections, resolve_watch_target, sessions_for_cwd,
    NoopRuntimeStateRefresher, RuntimeStateRefresher, RuntimeUpdate, SessionFile, SessionSelection,
    WatcherRuntime, WatcherRuntimeOptions, WatcherRuntimeTarget,
};
pub use state_machine::StateMachine;
pub use store::{SessionSummary, Store};
pub use watcher::{FileWatcher, WATCHER_POLL_INTERVAL};
