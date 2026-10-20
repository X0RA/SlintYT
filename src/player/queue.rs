//! A playlist cursor refers to an entry, never to the first matching video ID.
#[derive(Clone)]
pub(super) struct Entry {
    pub id: String,
    pub title: String,
}

#[derive(Default)]
pub(super) struct Queue {
    pub entries: Vec<Entry>,
    pub current: Option<usize>,
}

impl Queue {
    pub fn next(&self, shuffle: bool, random: usize) -> Option<usize> {
        let current = self.current?;
        let count = self.entries.len();
        if current >= count {
            return None;
        }
        if shuffle && count > 1 {
            Some((current + 1 + random % (count - 1)) % count)
        } else {
            (current + 1 < count).then_some(current + 1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_videos_advance_through_each_entry_and_stop_at_the_end() {
        let mut queue = Queue {
            entries: ["a", "b", "a", "c"]
                .into_iter()
                .map(|id| Entry {
                    id: id.into(),
                    title: id.into(),
                })
                .collect(),
            current: Some(0),
        };
        let mut played = vec![queue.entries[0].id.clone()];
        while let Some(next) = queue.next(false, 0) {
            played.push(queue.entries[next].id.clone());
            queue.current = Some(next);
        }
        assert_eq!(played, ["a", "b", "a", "c"]);
        queue.current = Some(2);
        assert_eq!(queue.next(false, 0), Some(3));
        queue.current = None;
        assert_eq!(queue.next(false, 0), None);
    }

    #[test]
    fn shuffle_selects_other_entries_and_handles_empty_or_single_entry_queues() {
        let mut queue = Queue::default();
        assert_eq!(queue.next(true, usize::MAX), None);
        queue.entries.push(Entry {
            id: "a".into(),
            title: "a".into(),
        });
        queue.current = Some(0);
        assert_eq!(queue.next(true, 0), None);
        queue.entries.push(Entry {
            id: "a".into(),
            title: "a".into(),
        });
        assert_eq!(queue.next(true, usize::MAX), Some(1));
        queue.current = Some(1);
        assert_eq!(queue.next(true, usize::MAX), Some(0));
    }
}
