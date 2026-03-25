use chrono::Utc;
use saw_core::{compute_io_rate, AgentEvent, ProcessMetrics};
use std::time::Duration;
use sysinfo::{DiskUsage, Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tokio::time::{interval_at, Instant, MissedTickBehavior};

pub const DEFAULT_PROCESS_POLL_INTERVAL: Duration = Duration::from_secs(2);

pub struct ProcessMonitor {
    pid: Pid,
    sys: System,
    poll_interval: Duration,
    previous_sample: Option<ProcessSample>,
}

#[derive(Debug, Clone, Copy)]
struct ProcessSample {
    captured_at: Instant,
    io_read_bytes: u64,
    io_write_bytes: u64,
}

impl ProcessMonitor {
    pub fn new(pid: u32) -> Self {
        Self::with_interval(pid, DEFAULT_PROCESS_POLL_INTERVAL)
    }

    pub fn with_interval(pid: u32, poll_interval: Duration) -> Self {
        let pid = Pid::from_u32(pid);
        let mut sys = System::new();
        refresh_process(&mut sys, pid);

        Self {
            pid,
            sys,
            poll_interval,
            previous_sample: None,
        }
    }

    pub async fn run<F>(&mut self, mut emit: F)
    where
        F: FnMut(AgentEvent),
    {
        let start = Instant::now() + self.poll_interval;
        let mut ticker = interval_at(start, self.poll_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            ticker.tick().await;
            refresh_process(&mut self.sys, self.pid);

            let now = Instant::now();
            let timestamp = Utc::now();
            let Some(process) = self.sys.process(self.pid) else {
                emit(dead_event(timestamp));
                break;
            };

            if !process.exists() {
                emit(dead_event(timestamp));
                break;
            }

            let disk_usage = process.disk_usage();
            let metrics = self.build_metrics(
                timestamp,
                now,
                process.cpu_usage(),
                process.memory(),
                process.virtual_memory(),
                &disk_usage,
            );
            emit(AgentEvent::ProcessMetrics(metrics));
        }
    }

    fn build_metrics(
        &mut self,
        timestamp: chrono::DateTime<Utc>,
        captured_at: Instant,
        cpu_percent: f32,
        rss_bytes: u64,
        virtual_bytes: u64,
        disk_usage: &DiskUsage,
    ) -> ProcessMetrics {
        let io_read_bytes = disk_usage.total_read_bytes;
        let io_write_bytes = disk_usage.total_written_bytes;
        let (io_read_rate, io_write_rate) = self
            .previous_sample
            .map(|previous| {
                let elapsed = captured_at.saturating_duration_since(previous.captured_at);
                (
                    compute_io_rate(previous.io_read_bytes, io_read_bytes, elapsed),
                    compute_io_rate(previous.io_write_bytes, io_write_bytes, elapsed),
                )
            })
            .unwrap_or((0.0, 0.0));

        self.previous_sample = Some(ProcessSample {
            captured_at,
            io_read_bytes,
            io_write_bytes,
        });

        ProcessMetrics {
            timestamp,
            process_alive: true,
            cpu_percent,
            rss_bytes,
            virtual_bytes,
            io_read_bytes,
            io_write_bytes,
            io_read_rate,
            io_write_rate,
        }
    }
}

fn refresh_process(sys: &mut System, pid: Pid) {
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        false,
        ProcessRefreshKind::nothing()
            .with_cpu()
            .with_memory()
            .with_disk_usage(),
    );
}

fn dead_event(timestamp: chrono::DateTime<Utc>) -> AgentEvent {
    AgentEvent::ProcessMetrics(ProcessMetrics {
        timestamp,
        process_alive: false,
        cpu_percent: 0.0,
        rss_bytes: 0,
        virtual_bytes: 0,
        io_read_bytes: 0,
        io_write_bytes: 0,
        io_read_rate: 0.0,
        io_write_rate: 0.0,
    })
}

#[cfg(test)]
mod tests {
    use super::ProcessMonitor;
    use saw_core::{compute_io_rate, AgentEvent};
    use std::process::{Child, Command, Stdio};
    use std::time::Duration;
    use tokio::sync::mpsc;
    use tokio::time::{timeout, Instant};

    #[test]
    fn computes_io_rate_from_sample_delta() {
        assert_eq!(compute_io_rate(100, 350, Duration::from_secs(2)), 125.0);
    }

    #[tokio::test]
    async fn emits_metrics_roughly_every_poll_interval() {
        let mut child = spawn_sleep_process();
        let mut monitor = ProcessMonitor::with_interval(child.id(), Duration::from_millis(250));
        let (tx, mut rx) = mpsc::unbounded_channel();

        let handle = tokio::spawn(async move {
            monitor
                .run(|event| {
                    let _ = tx.send(event);
                })
                .await;
        });

        let first = recv_live_metrics(&mut rx).await;
        let second = recv_live_metrics(&mut rx).await;

        let delta = second
            .timestamp
            .signed_duration_since(first.timestamp)
            .to_std()
            .unwrap();
        assert!(
            delta >= Duration::from_millis(200),
            "delta too small: {delta:?}"
        );
        assert!(
            delta <= Duration::from_millis(600),
            "delta too large: {delta:?}"
        );
        assert_eq!(first.io_read_rate, 0.0);
        assert_eq!(first.io_write_rate, 0.0);

        terminate(&mut child);
        let dead = recv_dead_event(&mut rx).await;
        assert!(!dead.process_alive);

        handle.await.unwrap();
    }

    #[tokio::test]
    async fn emits_dead_event_when_process_disappears() {
        let mut child = spawn_sleep_process();
        let mut monitor = ProcessMonitor::with_interval(child.id(), Duration::from_millis(100));
        let (tx, mut rx) = mpsc::unbounded_channel();

        let handle = tokio::spawn(async move {
            monitor
                .run(|event| {
                    let _ = tx.send(event);
                })
                .await;
        });

        let _ = recv_live_metrics(&mut rx).await;
        terminate(&mut child);

        let dead = recv_dead_event(&mut rx).await;
        assert_eq!(dead.cpu_percent, 0.0);
        assert_eq!(dead.rss_bytes, 0);
        assert_eq!(dead.virtual_bytes, 0);
        assert_eq!(dead.io_read_rate, 0.0);
        assert_eq!(dead.io_write_rate, 0.0);

        handle.await.unwrap();
    }

    #[tokio::test]
    async fn samples_memory_from_proc_status_within_one_megabyte() {
        let mut child = spawn_sleep_process();
        let mut monitor = ProcessMonitor::with_interval(child.id(), Duration::from_millis(100));
        let (tx, mut rx) = mpsc::unbounded_channel();

        let handle = tokio::spawn(async move {
            monitor
                .run(|event| {
                    let _ = tx.send(event);
                })
                .await;
        });

        let metrics = recv_live_metrics(&mut rx).await;
        let proc_status_rss = read_proc_status_rss_bytes(child.id());
        let diff = metrics.rss_bytes.abs_diff(proc_status_rss);
        assert!(diff <= 1024 * 1024, "rss diff too large: {diff} bytes");

        terminate(&mut child);
        let _ = recv_dead_event(&mut rx).await;
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn reports_cpu_usage_for_busy_process() {
        let mut child = spawn_busy_process();
        let mut monitor = ProcessMonitor::with_interval(child.id(), Duration::from_millis(250));
        let (tx, mut rx) = mpsc::unbounded_channel();

        let handle = tokio::spawn(async move {
            monitor
                .run(|event| {
                    let _ = tx.send(event);
                })
                .await;
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saw_non_zero_cpu = false;
        while Instant::now() < deadline {
            let metrics = recv_live_metrics(&mut rx).await;
            if metrics.cpu_percent > 1.0 {
                saw_non_zero_cpu = true;
                break;
            }
        }

        assert!(
            saw_non_zero_cpu,
            "expected busy process to report non-zero cpu"
        );

        terminate(&mut child);
        let _ = recv_dead_event(&mut rx).await;
        handle.await.unwrap();
    }

    async fn recv_live_metrics(
        rx: &mut mpsc::UnboundedReceiver<AgentEvent>,
    ) -> saw_core::ProcessMetrics {
        loop {
            let event = timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("timed out waiting for event")
                .expect("channel closed unexpectedly");
            if let AgentEvent::ProcessMetrics(metrics) = event {
                if metrics.process_alive {
                    return metrics;
                }
            }
        }
    }

    async fn recv_dead_event(
        rx: &mut mpsc::UnboundedReceiver<AgentEvent>,
    ) -> saw_core::ProcessMetrics {
        loop {
            let event = timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("timed out waiting for event")
                .expect("channel closed unexpectedly");
            if let AgentEvent::ProcessMetrics(metrics) = event {
                if !metrics.process_alive {
                    return metrics;
                }
            }
        }
    }

    fn read_proc_status_rss_bytes(pid: u32) -> u64 {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
        let rss_kib = status
            .lines()
            .find_map(|line| line.strip_prefix("VmRSS:"))
            .and_then(|rest| rest.split_whitespace().next())
            .map(|value| value.parse::<u64>().unwrap())
            .expect("VmRSS missing in /proc status");
        rss_kib * 1024
    }

    fn spawn_sleep_process() -> Child {
        Command::new("python3")
            .arg("-c")
            .arg("import time; time.sleep(30)")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    fn spawn_busy_process() -> Child {
        Command::new("python3")
            .arg("-c")
            .arg("while True: pass")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    fn terminate(child: &mut Child) {
        let _ = child.kill();
        let _ = child.wait();
    }
}
