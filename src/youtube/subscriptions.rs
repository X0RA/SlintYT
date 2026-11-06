//! Subscription cards expose rounded ages, not precise publication timestamps.
use super::Video;

/// Sort by age bucket, preserving YouTube's order within each bucket.
/// Unknown dates stay explicit instead of being guessed to be old uploads.
pub(crate) fn group(video: &Video) -> ((u64, u8), String) {
    if video.duration.eq_ignore_ascii_case("live")
        || video
            .metadata
            .split('·')
            .any(|part| part.trim().ends_with(" watching"))
    {
        return ((0, 0), "Live now".into());
    }
    for part in video.metadata.split(['·', '•']) {
        let lower = part.trim().to_ascii_lowercase();
        let age = lower
            .strip_prefix("streamed ")
            .or_else(|| lower.strip_prefix("premiered "))
            .unwrap_or(&lower);
        match age {
            "today" | "just now" => return ((1, 0), "Today".into()),
            "yesterday" => return ((2, 2), "Yesterday".into()),
            _ => {}
        }
        let words: Vec<_> = age.split_whitespace().collect();
        if words.len() != 3 || words[2] != "ago" {
            continue;
        }
        let Ok(n) = words[0].parse::<u64>() else {
            continue;
        };
        let unit = words[1].trim_end_matches('s');
        let (rank, label) = match unit {
            "second" | "minute" | "hour" => (1, "Today".into()),
            "day" if n == 0 => (1, "Today".into()),
            "day" if n == 1 => (2, "Yesterday".into()),
            "day" => (2, format!("{n} days ago")),
            "week" | "month" | "year" => (
                match unit {
                    "week" => 3,
                    "month" => 4,
                    _ => 5,
                },
                format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" }),
            ),
            _ => continue,
        };
        let days = match rank {
            1 => 0,
            2 => n,
            3 => n.saturating_mul(7),
            4 => n.saturating_mul(30),
            _ => n.saturating_mul(365),
        };
        return (
            (days.saturating_add(1), if rank == 1 { 0 } else { rank }),
            label,
        );
    }
    ((u64::MAX, u8::MAX), "Date unavailable".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounded_ages_and_streams_have_distinct_headings() {
        for (metadata, expected) in [
            ("12K views · 9 hours ago", "Today"),
            ("1 view · 1 second ago", "Today"),
            ("3 views · Streamed 1 day ago", "Yesterday"),
            ("Premiered 2 days ago", "2 days ago"),
            ("16 days ago", "16 days ago"),
            ("1 week ago", "1 week ago"),
            ("2 months ago", "2 months ago"),
            ("1 year ago", "1 year ago"),
            ("1.3K watching", "Live now"),
            ("12K views", "Date unavailable"),
            ("Starts in 2 hours", "Date unavailable"),
            ("16 days of Rust · 5 views", "Date unavailable"),
        ] {
            assert_eq!(
                group(&Video {
                    metadata: metadata.into(),
                    ..Default::default()
                })
                .1,
                expected
            );
        }
    }

    #[test]
    fn interleaved_pages_sort_into_contiguous_chronological_buckets() {
        let mut videos: Vec<_> = [
            "2 days ago",
            "1.3K watching",
            "1 hour ago",
            "16 days ago",
            "1 day ago",
            "2 hours ago",
            "1 week ago",
            "",
        ]
        .into_iter()
        .map(|metadata| Video {
            metadata: metadata.into(),
            ..Default::default()
        })
        .collect();
        videos.sort_by_cached_key(|video| group(video).0);
        assert_eq!(
            videos
                .iter()
                .map(|video| group(video).1)
                .collect::<Vec<_>>(),
            [
                "Live now",
                "Today",
                "Today",
                "Yesterday",
                "2 days ago",
                "1 week ago",
                "16 days ago",
                "Date unavailable"
            ]
        );
    }
}
