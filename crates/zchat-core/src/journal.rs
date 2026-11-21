use alloc::{
    collections::{BTreeMap, VecDeque},
    string::String,
    vec::Vec,
};

use crate::{ChatError, Result, Value, arr, obj};

const MAX_CAPACITY: usize = 65_536;
const DEFAULT_BYTE_CAPACITY: usize = 8 * 1024 * 1024;

/// An update retains its provider payload and server event identifier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub cursor: u64,
    pub account: String,
    pub kind: String,
    pub event_id: Option<String>,
    pub payload: Value,
}

impl Event {
    pub fn to_value(&self) -> Value {
        obj([
            ("cursor", Value::from(self.cursor)),
            ("account", Value::from(self.account.as_str())),
            ("kind", Value::from(self.kind.as_str())),
            (
                "event_id",
                self.event_id.as_deref().map_or(Value::Null, Value::from),
            ),
            ("payload", self.payload.clone()),
        ])
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppendOutcome {
    pub cursor: u64,
    /// False means the same account/kind/event_id/payload was already retained.
    pub inserted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventPage {
    pub events: Vec<Event>,
    pub next_cursor: u64,
    pub has_more: bool,
    pub oldest_cursor: u64,
    pub latest_cursor: u64,
}

impl EventPage {
    pub fn to_value(&self) -> Value {
        obj([
            ("events", arr(self.events.iter().map(Event::to_value))),
            ("next_cursor", Value::from(self.next_cursor)),
            ("has_more", Value::from(self.has_more)),
            ("oldest_cursor", Value::from(self.oldest_cursor)),
            ("latest_cursor", Value::from(self.latest_cursor)),
        ])
    }
}

type DedupKey = (String, String, String);
#[derive(Debug)]
struct Entry {
    event: Event,
    encoded_bytes: usize,
}

/// In-memory, bounded event replay. Cursors survive polling but are scoped to
/// this journal instance. A host must persist the journal or publish a new stream
/// identity after restart; these cursors are not durable backend sync tokens.
/// Deduplication is bounded to the retained replay window.
#[derive(Debug)]
pub struct EventJournal {
    capacity: usize,
    byte_capacity: usize,
    retained_bytes: usize,
    entries: VecDeque<Entry>,
    dedup: BTreeMap<DedupKey, u64>,
    next_cursor: u64,
    evicted_through: u64,
}

impl EventJournal {
    pub fn new(capacity: usize) -> Result<Self> {
        Self::with_limits(capacity, DEFAULT_BYTE_CAPACITY)
    }

    pub fn with_limits(capacity: usize, byte_capacity: usize) -> Result<Self> {
        if capacity == 0
            || capacity > MAX_CAPACITY
            || byte_capacity == 0
            || byte_capacity > 256 * 1024 * 1024
        {
            return Err(ChatError::new(
                "invalid_limits",
                "journal capacity must be 1..65536 events and 1..256 MiB",
            ));
        }
        Ok(Self {
            capacity,
            byte_capacity,
            retained_bytes: 0,
            entries: VecDeque::new(),
            dedup: BTreeMap::new(),
            next_cursor: 1,
            evicted_through: 0,
        })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }
    pub fn latest_cursor(&self) -> u64 {
        self.next_cursor - 1
    }
    pub fn oldest_cursor(&self) -> u64 {
        self.entries
            .front()
            .map_or(self.next_cursor, |entry| entry.event.cursor)
    }

    pub fn append(
        &mut self,
        account: &str,
        kind: &str,
        event_id: Option<&str>,
        payload: Value,
    ) -> Result<AppendOutcome> {
        for value in [account, kind] {
            if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
                return Err(ChatError::new(
                    "invalid_event",
                    "account and kind must be bounded nonempty strings",
                ));
            }
        }
        if let Some(id) = event_id
            && (id.is_empty() || id.len() > 4096 || id.chars().any(char::is_control))
        {
            return Err(ChatError::new(
                "invalid_event",
                "event_id must contain 1..4096 bytes without control characters",
            ));
        }
        let key = event_id.map(|id| (String::from(account), String::from(kind), String::from(id)));
        if let Some(cursor) = key.as_ref().and_then(|key| self.dedup.get(key)).copied() {
            let index = (cursor - self.oldest_cursor()) as usize;
            let entry = self.entries.get(index).expect("retained dedup entry");
            if entry.event.payload != payload {
                return Err(ChatError::new(
                    "event_conflict",
                    "a retained event identifier was reused with different data",
                ));
            }
            return Ok(AppendOutcome {
                cursor,
                inserted: false,
            });
        }
        let following = self
            .next_cursor
            .checked_add(1)
            .ok_or_else(|| ChatError::new("cursor_exhausted", "event cursor space exhausted"))?;
        let event = Event {
            cursor: self.next_cursor,
            account: account.into(),
            kind: kind.into(),
            event_id: event_id.map(String::from),
            payload,
        };
        let encoded_bytes = event.to_value().try_stringify()?.len();
        if encoded_bytes > self.byte_capacity {
            return Err(ChatError::new(
                "event_too_large",
                "event exceeds the journal byte capacity",
            ));
        }

        // Validate the complete event before evicting anything, so rejected input
        // leaves the replay window unchanged.
        while self.entries.len() >= self.capacity
            || self.retained_bytes + encoded_bytes > self.byte_capacity
        {
            let removed = self
                .entries
                .pop_front()
                .expect("capacity eviction requires retained entries");
            self.retained_bytes -= removed.encoded_bytes;
            self.evicted_through = removed.event.cursor;
            if let Some(id) = removed.event.event_id {
                self.dedup
                    .remove(&(removed.event.account, removed.event.kind, id));
            }
        }
        let cursor = event.cursor;
        if let Some(key) = key {
            self.dedup.insert(key, cursor);
        }
        self.entries.push_back(Entry {
            event,
            encoded_bytes,
        });
        self.retained_bytes += encoded_bytes;
        self.next_cursor = following;
        Ok(AppendOutcome {
            cursor,
            inserted: true,
        })
    }

    /// `None` starts at the oldest retained update. An explicit cursor never
    /// silently skips an evicted update: resynchronization is required.
    pub fn poll(&self, after: Option<u64>, limit: usize) -> Result<EventPage> {
        if limit == 0 || limit > crate::MAX_PAGE_SIZE {
            return Err(ChatError::new(
                "invalid_params",
                "event page limit must be 1..200",
            ));
        }
        let after = after.unwrap_or(self.evicted_through);
        if after < self.evicted_through {
            return Err(ChatError::new(
                "cursor_expired",
                "event replay cursor expired; resynchronize the account",
            ));
        }
        if after >= self.next_cursor {
            return Err(ChatError::new(
                "cursor_ahead",
                "event cursor is newer than this journal",
            ));
        }
        let events: Vec<Event> = self
            .entries
            .iter()
            .filter(|entry| entry.event.cursor > after)
            .take(limit)
            .map(|entry| entry.event.clone())
            .collect();
        let next_cursor = events.last().map_or(after, |event| event.cursor);
        Ok(EventPage {
            has_more: next_cursor < self.latest_cursor(),
            events,
            next_cursor,
            oldest_cursor: self.oldest_cursor(),
            latest_cursor: self.latest_cursor(),
        })
    }

    pub fn read(&self, after: u64, limit: usize) -> Result<EventPage> {
        self.poll(Some(after), limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn push(journal: &mut EventJournal, id: &str) -> AppendOutcome {
        journal
            .append("matrix", "message.received", Some(id), Value::from(id))
            .unwrap()
    }

    #[test]
    fn preserves_order_and_paginates_without_dropping_updates() {
        let mut journal = EventJournal::new(8).unwrap();
        push(&mut journal, "z");
        push(&mut journal, "a");
        push(&mut journal, "m");
        let first = journal.read(0, 2).unwrap();
        assert_eq!(
            first
                .events
                .iter()
                .map(|event| event.event_id.as_deref().unwrap())
                .collect::<Vec<_>>(),
            vec!["z", "a"]
        );
        assert!(first.has_more);
        let second = journal.read(first.next_cursor, 2).unwrap();
        assert_eq!(second.events.len(), 1);
        assert!(!second.has_more);
        assert_eq!(second.next_cursor, 3);
    }

    #[test]
    fn evicted_cursor_is_explicit_and_latest_cursor_can_wait() {
        let mut journal = EventJournal::new(2).unwrap();
        push(&mut journal, "a");
        push(&mut journal, "b");
        push(&mut journal, "c");
        assert_eq!(journal.read(0, 10).unwrap_err().code, "cursor_expired");
        assert_eq!(journal.poll(None, 10).unwrap().events.len(), 2);
        assert!(journal.read(3, 10).unwrap().events.is_empty());
        assert_eq!(journal.read(4, 10).unwrap_err().code, "cursor_ahead");
    }

    #[test]
    fn dedup_is_account_and_kind_scoped_and_conflicts_fail() {
        let mut journal = EventJournal::new(8).unwrap();
        assert!(push(&mut journal, "a").inserted);
        assert!(!push(&mut journal, "a").inserted);
        assert_eq!(
            journal
                .append(
                    "matrix",
                    "message.received",
                    Some("a"),
                    Value::from("changed")
                )
                .unwrap_err()
                .code,
            "event_conflict"
        );
        assert!(
            journal
                .append("telegram", "message.received", Some("a"), Value::from("a"))
                .unwrap()
                .inserted
        );
        assert!(
            journal
                .append("matrix", "message.edited", Some("a"), Value::from("a"))
                .unwrap()
                .inserted
        );
        assert_eq!(journal.len(), 3);
    }

    #[test]
    fn byte_limit_evicts_and_rejected_event_leaves_state_unchanged() {
        let mut journal = EventJournal::with_limits(10, 240).unwrap();
        push(&mut journal, "a");
        push(&mut journal, "b");
        push(&mut journal, "c");
        assert!(journal.retained_bytes() <= 240);
        let before = journal.poll(None, 10).unwrap();
        let error = journal
            .append(
                "matrix",
                "message.received",
                None,
                Value::from("x".repeat(300)),
            )
            .unwrap_err();
        assert_eq!(error.code, "event_too_large");
        assert_eq!(journal.poll(None, 10).unwrap(), before);
    }

    #[test]
    fn dedup_window_is_bounded_and_empty_poll_is_valid() {
        let mut journal = EventJournal::new(1).unwrap();
        assert_eq!(journal.poll(None, 1).unwrap().next_cursor, 0);
        push(&mut journal, "a");
        push(&mut journal, "b");
        assert!(push(&mut journal, "a").inserted);
        assert_eq!(journal.latest_cursor(), 3);
    }
}
