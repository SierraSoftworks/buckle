//! What the worker is doing and what waits behind it, in words: a local
//! caller's "behind …" line and `buckle status` read it.

// Only the local socket reads the board, and nothing serves it on Windows yet.
#![cfg_attr(not(unix), allow(dead_code))]

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::watch;

pub(super) struct Board {
    next: AtomicU64,
    inner: Mutex<Inner>,
    /// Bumped whenever the line moves.
    changed: watch::Sender<u64>,
}

#[derive(Default)]
struct Inner {
    running: Option<String>,
    /// Queued operations, next first, by their place number.
    waiting: VecDeque<(u64, String)>,
}

impl Default for Board {
    fn default() -> Self {
        Self {
            next: AtomicU64::new(0),
            inner: Mutex::default(),
            changed: watch::channel(0).0,
        }
    }
}

impl Board {
    fn update<T>(&self, f: impl FnOnce(&mut Inner) -> T) -> T {
        let out = f(&mut self.inner.lock().unwrap_or_else(|p| p.into_inner()));
        self.changed.send_modify(|n| *n += 1);
        out
    }

    fn read<T>(&self, f: impl FnOnce(&Inner) -> T) -> T {
        f(&self.inner.lock().unwrap_or_else(|p| p.into_inner()))
    }

    /// Joins the back of the line; returns the operation's place number.
    pub fn queue(&self, what: String) -> u64 {
        let place = self.next.fetch_add(1, Ordering::SeqCst);
        self.update(|i| i.waiting.push_back((place, what)));
        place
    }

    /// Leaves the line (the worker took the operation). `None` when it was
    /// not in it.
    pub fn take(&self, place: u64) -> Option<String> {
        self.update(|i| {
            let at = i.waiting.iter().position(|(p, _)| *p == place)?;
            i.waiting.remove(at).map(|(_, what)| what)
        })
    }

    /// Back to the end of the line, keeping its place number (an upgrade
    /// waiting for the agent to be idle).
    pub fn requeue(&self, place: u64, what: String) {
        self.update(|i| i.waiting.push_back((place, what)));
    }

    /// The worker started `what`.
    pub fn start(&self, what: String) {
        self.update(|i| i.running = Some(what));
    }

    /// The worker finished what it was running.
    pub fn finish(&self) {
        self.update(|i| i.running = None);
    }

    /// What runs before the operation at `place`: the running one, then
    /// those queued ahead of it. `None` once it has left the line.
    pub fn ahead_of(&self, place: u64) -> Option<Vec<String>> {
        self.read(|i| {
            let at = i.waiting.iter().position(|(p, _)| *p == place)?;
            Some(
                i.running
                    .iter()
                    .chain(i.waiting.iter().take(at).map(|(_, what)| what))
                    .cloned()
                    .collect(),
            )
        })
    }

    /// The running operation and those queued behind it.
    pub fn snapshot(&self) -> (Option<String>, Vec<String>) {
        self.read(|i| {
            (
                i.running.clone(),
                i.waiting.iter().map(|(_, what)| what.clone()).collect(),
            )
        })
    }

    /// Changes whenever the line moves.
    pub fn watch(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_board_names_what_runs_ahead_of_an_operation() {
        let board = Board::default();
        let apply = board.queue("apply r1".into());
        let plan = board.queue("local plan".into());
        assert_eq!(board.ahead_of(plan), Some(vec!["apply r1".to_owned()]));

        let what = board.take(apply).expect("queued");
        board.start(what);
        assert_eq!(
            board.ahead_of(plan),
            Some(vec!["apply r1".to_owned()]),
            "the running operation is still ahead"
        );
        assert_eq!(
            board.snapshot(),
            (Some("apply r1".into()), vec!["local plan".to_owned()])
        );

        board.finish();
        assert_eq!(board.ahead_of(plan), Some(Vec::new()), "next up");
        board.take(plan);
        assert_eq!(board.ahead_of(plan), None, "it has left the line");
    }

    #[test]
    fn a_requeued_operation_goes_to_the_back_and_watchers_hear_every_move() {
        let board = Board::default();
        let mut watch = board.watch();
        let upgrade = board.queue("upgrade".into());
        let plan = board.queue("local plan".into());
        assert!(watch.has_changed().expect("open"));
        watch.borrow_and_update();

        let what = board.take(upgrade).expect("queued");
        board.requeue(upgrade, what);
        assert!(watch.has_changed().expect("open"));
        assert_eq!(board.ahead_of(plan), Some(Vec::new()));
        assert_eq!(board.ahead_of(upgrade), Some(vec!["local plan".to_owned()]));
    }
}
