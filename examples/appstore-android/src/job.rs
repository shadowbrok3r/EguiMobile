//! Background jobs wake the UI both when they start and when their result is ready.

use egui_mobile::egui;
use std::sync::mpsc::{Receiver, channel};

pub(crate) struct Job<T> {
    rx: Option<Receiver<T>>,
}

impl<T: Send + 'static> Job<T> {
    pub(crate) fn idle() -> Self {
        Self { rx: None }
    }
    pub(crate) fn busy(&self) -> bool {
        self.rx.is_some()
    }
    pub(crate) fn start(
        &mut self,
        ctx: egui::Context,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> bool {
        if self.rx.is_some() {
            return false;
        }
        let (tx, rx) = channel();
        self.rx = Some(rx);
        // The UI may start this job after its polling pass. Schedule the next
        // frame now so progress begins even when the user stops touching the screen.
        ctx.request_repaint();
        std::thread::spawn(move || {
            let _ = tx.send(f());
            ctx.request_repaint();
        });
        true
    }
    pub(crate) fn poll(&mut self) -> Option<T> {
        match self.rx.as_ref()?.try_recv() {
            Ok(v) => {
                self.rx = None;
                Some(v)
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.rx = None;
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{RecvTimeoutError, TryRecvError};
    use std::time::Duration;

    const TIMEOUT: Duration = Duration::from_secs(5);

    fn settle(ctx: &egui::Context, wakes: &Receiver<Duration>) {
        // Drain egui's follow-up frames so the next callback must come from the job.
        for _ in 0..4 {
            ctx.run_ui(Default::default(), |_| {})
                .drop_without_applying_deltas();
        }
        while wakes.try_recv().is_ok() {}
        assert!(!ctx.has_requested_repaint());
    }

    fn idle_context() -> (egui::Context, Receiver<Duration>) {
        let ctx = egui::Context::default();
        let (wake_tx, wakes) = channel();
        ctx.set_request_repaint_callback(move |info| {
            let _ = wake_tx.send(info.delay);
        });
        settle(&ctx, &wakes);
        (ctx, wakes)
    }

    #[test]
    fn start_and_completion_each_wake_an_idle_ui() {
        let (ctx, wakes) = idle_context();
        let (release, wait) = channel();
        let mut job = Job::idle();
        assert!(job.start(ctx.clone(), move || {
            wait.recv_timeout(TIMEOUT).expect("release worker");
            42
        }));
        assert_eq!(wakes.recv_timeout(TIMEOUT), Ok(Duration::ZERO));
        assert!(job.busy());
        assert_eq!(job.poll(), None);

        // Let the UI go idle while the worker remains blocked. No input or UI
        // frame occurs between releasing the worker and receiving its wakeup.
        settle(&ctx, &wakes);
        release.send(()).unwrap();
        assert_eq!(wakes.recv_timeout(TIMEOUT), Ok(Duration::ZERO));
        assert_eq!(job.poll(), Some(42), "result must precede its wakeup");
        assert!(!job.busy());

        settle(&ctx, &wakes);
        assert_eq!(job.poll(), None);
        assert_eq!(wakes.try_recv(), Err(TryRecvError::Empty));
        assert!(!ctx.has_requested_repaint());
    }

    #[test]
    fn busy_job_rejects_duplicate_work_then_can_start_again() {
        let (ctx, wakes) = idle_context();
        let (release, wait) = channel();
        let (duplicate_tx, duplicate_rx) = channel();
        let mut job = Job::idle();
        assert!(job.start(ctx.clone(), move || {
            wait.recv_timeout(TIMEOUT).expect("release worker");
            1
        }));
        settle(&ctx, &wakes);
        assert!(!job.start(ctx.clone(), move || {
            duplicate_tx.send(()).unwrap();
            2
        }));
        assert_eq!(
            duplicate_rx.recv_timeout(TIMEOUT),
            Err(RecvTimeoutError::Disconnected)
        );
        assert_eq!(wakes.try_recv(), Err(TryRecvError::Empty));
        release.send(()).unwrap();
        assert_eq!(wakes.recv_timeout(TIMEOUT), Ok(Duration::ZERO));
        assert_eq!(job.poll(), Some(1));

        settle(&ctx, &wakes);
        let (release, wait) = channel();
        assert!(job.start(ctx.clone(), move || {
            wait.recv_timeout(TIMEOUT).expect("release next worker");
            3
        }));
        assert_eq!(wakes.recv_timeout(TIMEOUT), Ok(Duration::ZERO));
        settle(&ctx, &wakes);
        release.send(()).unwrap();
        assert_eq!(wakes.recv_timeout(TIMEOUT), Ok(Duration::ZERO));
        assert_eq!(job.poll(), Some(3));
        assert!(!job.busy());
    }
}
