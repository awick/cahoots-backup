// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

//! Which destinations a view lists, and in what order.

use crate::config::Friend;
use serde_json::Value;

/// What the Friends page can be put in order by.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Sort {
    /// As they were added.
    Added,
    Name,
    Theirs,
    StoredHere,
    StoredThere,
    Sending,
    Receiving,
}

/// Every order, with what a view calls it.
pub const SORTS: [(Sort, &str); 7] = [
    (Sort::Added, "order added"),
    (Sort::Name, "name"),
    (Sort::Theirs, "their own name"),
    (Sort::StoredHere, "stored here"),
    (Sort::StoredThere, "stored there"),
    (Sort::Sending, "sending now"),
    (Sort::Receiving, "receiving now"),
];

impl Sort {
    pub fn label(self) -> &'static str {
        SORTS.iter().find(|(s, _)| *s == self).map_or("", |(_, l)| l)
    }

    /// The one after this, round and round.
    pub fn next(self) -> Sort {
        let at = SORTS.iter().position(|(s, _)| *s == self).unwrap_or(0);
        SORTS[(at + 1) % SORTS.len()].0
    }

    /// The number a destination is ordered by, from what the daemon says of it.
    fn number(self, o: Option<&Value>) -> u64 {
        let key = match self {
            Sort::StoredHere => "used_here",
            Sort::StoredThere => "used_there",
            Sort::Sending => "rate_out",
            Sort::Receiving => "rate_in",
            _ => return 0,
        };
        o.and_then(|o| o[key].as_u64()).unwrap_or(0)
    }
}

/// What a friend calls their own computer: what they said, or the name they have always had here.
pub fn theirs(f: &Friend) -> &str {
    f.theirs.as_deref().unwrap_or(&f.name)
}

/// The destinations to list: those whose name, or own name, holds `filter` (in any case),
/// in the order asked for. Names run A to Z and amounts largest first, unless `backward`.
/// Friends still come before drives, and drives before cloud storage. `overview` is the
/// daemon's picture, for the amounts.
pub fn arrange(friends: &[Friend], overview: &Value, sort: Sort, backward: bool, filter: &str) -> Vec<Friend> {
    let needle = filter.trim().to_lowercase();
    let wanted = |f: &&Friend| {
        needle.is_empty() || f.name.to_lowercase().contains(&needle) || theirs(f).to_lowercase().contains(&needle)
    };
    let said = |f: &Friend| overview["dests"].as_array().and_then(|a| a.iter().find(|d| d["name"] == f.name.as_str()));
    let mut all: Vec<&Friend> = friends.iter().filter(wanted).collect();
    match sort {
        Sort::Added => {}
        Sort::Name => all.sort_by_key(|f| f.name.to_lowercase()),
        Sort::Theirs => all.sort_by_key(|f| theirs(f).to_lowercase()),
        _ => all.sort_by_key(|f| std::cmp::Reverse(sort.number(said(f)))),
    }
    if backward {
        all.reverse();
    }
    // Stable, so each kind keeps the order just given.
    all.sort_by_key(|f| match f.kind() {
        "friend" => 0,
        "drive" => 1,
        _ => 2,
    });
    all.into_iter().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn friend(name: &str, theirs: Option<&str>, drive: bool) -> Friend {
        Friend {
            name: name.into(),
            id: name.into(),
            quota_gb: 1,
            backup_to: true,
            relay: None,
            addrs: vec![],
            every_minutes: 0,
            max_in_kbps: 0,
            path: drive.then(|| "/drive".into()),
            drive_key: None,
            cloud: None,
            retention: None,
            theirs: theirs.map(String::from),
        }
    }

    fn names(v: Vec<Friend>) -> Vec<String> {
        v.into_iter().map(|f| f.name).collect()
    }

    #[test]
    fn destinations_are_listed_in_the_order_asked_for() {
        let all = [
            friend("usb", None, true),
            friend("Mum", Some("qnap-box"), false),
            friend("bob", None, false),
            friend("alice", Some("Zed"), false),
        ];
        let ov = json!({"dests": [
            {"name": "Mum", "used_here": 50, "rate_out": 0},
            {"name": "bob", "used_here": 900, "rate_out": 7},
            {"name": "alice", "used_here": 10, "rate_out": 3},
        ]});
        let list = |sort, backward, filter: &str| names(arrange(&all, &ov, sort, backward, filter));
        // Friends first whatever the order, and as added when no order is asked for.
        assert_eq!(list(Sort::Added, false, ""), ["Mum", "bob", "alice", "usb"]);
        assert_eq!(list(Sort::Name, false, ""), ["alice", "bob", "Mum", "usb"]);
        assert_eq!(list(Sort::Name, true, ""), ["Mum", "bob", "alice", "usb"]);
        // By what they call themselves: bob has only the one name.
        assert_eq!(list(Sort::Theirs, false, ""), ["bob", "Mum", "alice", "usb"]);
        // Amounts run largest first.
        assert_eq!(list(Sort::StoredHere, false, ""), ["bob", "Mum", "alice", "usb"]);
        assert_eq!(list(Sort::Sending, true, ""), ["Mum", "alice", "bob", "usb"]);
        // The filter looks at both names, in any case.
        assert_eq!(list(Sort::Name, false, "QNAP"), ["Mum"]);
        assert_eq!(list(Sort::Name, false, "B"), ["bob", "Mum", "usb"]);
        assert_eq!(list(Sort::Name, false, " us "), ["usb"]);
        assert!(list(Sort::Name, false, "nobody").is_empty());
        assert_eq!(Sort::Receiving.next(), Sort::Added);
    }
}
