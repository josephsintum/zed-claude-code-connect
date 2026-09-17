//! What the user has selected, and how that fact travels through the process.
//!
//! One typed value, [`Selection`], replaces three copies of the same state that
//! used to live as JSON: a cache in the WebSocket module, a per-connection cache
//! in the MCP server, and the debouncer's last-sent string. The editor side
//! feeds a [`SelectionTracker`]; the CLI side subscribes to the [`EventBus`].
//! Nothing here knows about JSON -- the wire shape is built at the socket edge.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use lsp_types::{Range, Url};
use tokio::sync::{broadcast, watch};

/// The live selection in some buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// Absolute filesystem path, percent-decoded.
    pub path: PathBuf,
    /// The document URI as Zed sent it.
    pub uri: Url,
    /// 0-based, UTF-16 columns, as LSP counts.
    pub range: Range,
    /// The selected text, from the buffer mirror or from disk.
    pub text: String,
}

impl Selection {
    /// A bare cursor: nothing selected.
    pub fn is_empty(&self) -> bool {
        self.range.start == self.range.end
    }
}

/// An `@`-mention of a file, optionally narrowed to a line range (0-based).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mention {
    pub path: PathBuf,
    /// `None` mentions the whole file. The CLI reads a missing range that way and
    /// rejects an explicit null, so this must never serialize as null.
    pub lines: Option<(u32, u32)>,
}

impl Mention {
    pub fn of(selection: &Selection) -> Self {
        Self {
            path: selection.path.clone(),
            lines: if selection.is_empty() {
                None
            } else {
                Some((selection.range.start.line, selection.range.end.line))
            },
        }
    }
}

/// Everything the CLI side is told about.
#[derive(Debug, Clone)]
pub enum Event {
    SelectionChanged(Arc<Selection>),
    AtMentioned(Mention),
}

/// Fan-out from the editor side to every connected CLI, plus the one selection a
/// late client is told about on connect.
///
/// `latest` is written only here and only when a selection is actually
/// published, so it is exactly what every subscriber has been sent.
#[derive(Debug, Clone)]
pub struct EventBus {
    events: broadcast::Sender<Event>,
    latest: watch::Sender<Option<Arc<Selection>>>,
}

impl EventBus {
    /// `capacity` is how far a slow subscriber may fall behind before it starts
    /// missing events; only the newest selection matters, so that is harmless.
    pub fn new(capacity: usize) -> Self {
        let (events, _) = broadcast::channel(capacity);
        let (latest, _) = watch::channel(None);
        Self { events, latest }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// The most recent published selection, if any.
    pub fn latest(&self) -> Option<Arc<Selection>> {
        // Clone out immediately: a `watch` borrow is a read lock the publisher
        // blocks on, and must never be held across an await.
        self.latest.borrow().clone()
    }

    /// Record `selection` as the latest and tell every subscriber. Returns the
    /// shared value that was published.
    pub fn publish_selection(&self, selection: impl Into<Arc<Selection>>) -> Arc<Selection> {
        let selection = selection.into();
        self.latest.send_replace(Some(Arc::clone(&selection)));
        // No subscribers is fine: `latest` still carries it to the next connect.
        let _ = self
            .events
            .send(Event::SelectionChanged(Arc::clone(&selection)));
        selection
    }

    pub fn publish_mention(&self, mention: Mention) {
        let _ = self.events.send(Event::AtMentioned(mention));
    }

    pub fn subscriber_count(&self) -> usize {
        self.events.receiver_count()
    }
}

/// Coalesces the stream of selections the editor reports into the ones worth
/// sending: the settled value after a burst, and only if it differs from the
/// last one sent.
///
/// Zed asks for code actions on every cursor move, so holding shift-arrow
/// through a block produces a burst, each carrying the full selected text. The
/// VS Code extension debounces the same way.
#[derive(Debug, Clone)]
pub struct SelectionTracker {
    input: watch::Sender<Option<Arc<Selection>>>,
}

impl SelectionTracker {
    /// Returns the tracker and the task that drives it; the caller spawns the
    /// task. It ends when every clone of the tracker has been dropped.
    pub fn new(
        bus: EventBus,
        debounce: Duration,
    ) -> (Self, impl std::future::Future<Output = ()> + Send) {
        let (input, rx) = watch::channel(None);
        (Self { input }, run(rx, bus, debounce))
    }

    /// The editor reported a selection. Cheap; call it on every cursor move.
    pub fn update(&self, selection: Selection) {
        // `send_replace` overwrites regardless of receivers and bumps the version
        // even for an equal value, which restarts the timer -- identity is judged
        // at emission, not on input.
        self.input.send_replace(Some(Arc::new(selection)));
    }
}

async fn run(mut rx: watch::Receiver<Option<Arc<Selection>>>, bus: EventBus, debounce: Duration) {
    let mut last_sent: Option<Arc<Selection>> = None;
    loop {
        // Wait for the first update since the last emission.
        if rx.changed().await.is_err() {
            return; // every tracker dropped
        }
        // Then let it settle: each further update restarts the timer. `watch`
        // keeps only the newest value, which is exactly the coalescing wanted.
        loop {
            tokio::select! {
                _ = tokio::time::sleep(debounce) => break,
                changed = rx.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
            }
        }
        // `borrow_and_update`, not `borrow`: otherwise the next `changed()` fires
        // at once for the value just read and it is emitted twice.
        let Some(selection) = rx.borrow_and_update().clone() else {
            continue;
        };
        if last_sent.as_ref() == Some(&selection) {
            continue; // identical to what every client already has
        }
        last_sent = Some(Arc::clone(&selection));
        bus.publish_selection(selection);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::Position;
    use tokio::sync::broadcast::error::TryRecvError;
    use tokio::task::yield_now;
    use tokio::time::advance;

    const DEBOUNCE: Duration = Duration::from_millis(300);

    fn sel(text: &str) -> Selection {
        Selection {
            path: PathBuf::from("/tmp/a.rs"),
            uri: Url::parse("file:///tmp/a.rs").unwrap(),
            range: Range {
                start: Position {
                    line: 1,
                    character: 0,
                },
                end: Position {
                    line: 1,
                    character: text.len() as u32,
                },
            },
            text: text.to_string(),
        }
    }

    fn rig() -> (EventBus, broadcast::Receiver<Event>, SelectionTracker) {
        let bus = EventBus::new(8);
        let rx = bus.subscribe();
        let (tracker, task) = SelectionTracker::new(bus.clone(), DEBOUNCE);
        tokio::spawn(task);
        (bus, rx, tracker)
    }

    /// Let the tracker task run, then move the clock. Paused time never advances
    /// on its own while a task is runnable, so this is deterministic.
    async fn elapse(d: Duration) {
        yield_now().await;
        advance(d).await;
        yield_now().await;
    }

    fn text_of(event: Result<Event, TryRecvError>) -> String {
        match event {
            Ok(Event::SelectionChanged(s)) => s.text.clone(),
            other => panic!("expected a selection, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_is_emitted_before_the_debounce_elapses() {
        let (bus, mut rx, tracker) = rig();
        tracker.update(sel("a"));

        elapse(DEBOUNCE - Duration::from_millis(1)).await;
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
        assert!(bus.latest().is_none(), "latest changes only on emission");

        elapse(Duration::from_millis(2)).await;
        assert_eq!(text_of(rx.try_recv()), "a");
        assert_eq!(bus.latest().unwrap().text, "a");
    }

    #[tokio::test(start_paused = true)]
    async fn a_burst_settles_to_its_last_value() {
        let (_bus, mut rx, tracker) = rig();
        for text in ["a", "ab", "abc"] {
            tracker.update(sel(text));
            elapse(Duration::from_millis(100)).await;
        }
        // 100ms after the last update: still inside the window.
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));

        elapse(DEBOUNCE).await;
        assert_eq!(text_of(rx.try_recv()), "abc");
        assert!(
            matches!(rx.try_recv(), Err(TryRecvError::Empty)),
            "exactly one emission for the whole burst"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_identical_settled_selection_is_not_resent() {
        let (_bus, mut rx, tracker) = rig();
        tracker.update(sel("same"));
        elapse(DEBOUNCE + Duration::from_millis(1)).await;
        assert_eq!(text_of(rx.try_recv()), "same");

        tracker.update(sel("same"));
        elapse(DEBOUNCE + Duration::from_millis(1)).await;
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));

        tracker.update(sel("different"));
        elapse(DEBOUNCE + Duration::from_millis(1)).await;
        assert_eq!(text_of(rx.try_recv()), "different");
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_every_tracker_ends_the_task() {
        let bus = EventBus::new(8);
        let (tracker, task) = SelectionTracker::new(bus, DEBOUNCE);
        let handle = tokio::spawn(task);
        drop(tracker);
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("task should end once no tracker can feed it")
            .unwrap();
    }

    #[test]
    fn a_mention_of_an_empty_selection_covers_the_whole_file() {
        let mut s = sel("");
        s.range.end = s.range.start;
        assert_eq!(Mention::of(&s).lines, None);
        let s = sel("x");
        assert_eq!(Mention::of(&s).lines, Some((1, 1)));
    }
}
