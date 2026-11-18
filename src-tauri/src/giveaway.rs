// The giveaway code. Every accepted upload gets a receipt from the server (8 characters, Crockford base32); the
// app keeps the latest one per kind (config::Receipts) and shows them as one code the player can paste into the
// giveaway: EVL-<auctions receipt>-<collector receipt>, with 0 for a part it doesn't have yet.
//
// Shipped dark. To switch it on: set ENABLED to true (and ENDS to the day after the giveaway's last day), build
// and release. The spot then shows under Uploads until ENDS. EVERLOOKDB_GIVEAWAY=1 shows it in any build without
// changing the code (for testing the look before it is announced); EVERLOOKDB_GIVEAWAY=0 hides it.
// Receipts are kept whether it is on or not, so a player who uploaded before it is switched on already has a code.

use crate::config::{Receipt, Receipts};
use serde::Serialize;

/// The switch. false: no giveaway spot anywhere in the window.
pub const ENABLED: bool = false;
/// The first day it is no longer shown (local date, YYYY-MM-DD).
pub const ENDS: &str = "2027-01-01";

/// Crockford base32: digits and letters without I, L, O and U.
const CROCKFORD: &str = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Whether the window shows the giveaway on `today` (YYYY-MM-DD).
pub fn shown(today: &str) -> bool {
    match std::env::var("EVERLOOKDB_GIVEAWAY").ok().as_deref().map(str::trim) {
        Some("1") | Some("true") | Some("on") => return true,
        Some("0") | Some("false") | Some("off") => return false,
        _ => {}
    }
    on(ENABLED, ENDS, today)
}

/// The rule without the environment: switched on, and before the end date. A date that can't be read ends it.
pub fn on(enabled: bool, ends: &str, today: &str) -> bool {
    let ends = chrono::NaiveDate::parse_from_str(ends, "%Y-%m-%d");
    let today = chrono::NaiveDate::parse_from_str(today, "%Y-%m-%d");
    enabled && matches!((ends, today), (Ok(e), Ok(t)) if t < e)
}

/// A receipt from the server's answer, in capitals; None unless it is 8 Crockford base32 characters.
pub fn receipt(s: &str) -> Option<String> {
    let r = s.trim().to_ascii_uppercase();
    (r.len() == 8 && r.chars().all(|c| CROCKFORD.contains(c))).then_some(r)
}

/// "EVL-<auctions>-<collector>", 0 for a missing part.
pub fn code(auctions: Option<&str>, collector: Option<&str>) -> String {
    format!("EVL-{}-{}", auctions.unwrap_or("0"), collector.unwrap_or("0"))
}

/// Keep a new auctions receipt. The latest one wins, except that one without a full scan never replaces one with:
/// the code should stay the best the player has.
pub fn keep_auctions(r: &mut Receipts, new: Receipt) {
    if r.auctions.as_ref().is_some_and(|old| old.full_scan && !new.full_scan) {
        return;
    }
    r.auctions = Some(new);
}

/// What the window shows.
#[derive(Serialize, Clone, Debug, PartialEq, Default)]
pub struct View {
    pub code: String,
    /// a full auction house scan was uploaded (and has a receipt)
    pub auctions: bool,
    /// collector data (EverlookDB.lua) was uploaded (and has a receipt)
    pub collector: bool,
    /// all three steps are done (the app is installed: it is showing this)
    pub complete: bool,
}

pub fn view(r: &Receipts) -> View {
    let a = r.auctions.as_ref().map(|x| x.code.as_str());
    let c = r.collector.as_ref().map(|x| x.code.as_str());
    let auctions = r.auctions.as_ref().is_some_and(|x| x.full_scan);
    let collector = c.is_some();
    View { code: code(a, c), auctions, collector, complete: auctions && collector }
}

/// The view, or None when the giveaway isn't shown.
pub fn view_if_shown(r: &Receipts) -> Option<View> {
    shown(&chrono::Local::now().format("%Y-%m-%d").to_string()).then(|| view(r))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rc(code: &str, full: bool) -> Receipt {
        Receipt { code: code.into(), at: 1, full_scan: full }
    }

    #[test]
    fn receipts_are_checked() {
        assert_eq!(receipt("7K3M9QXZ").as_deref(), Some("7K3M9QXZ"));
        assert_eq!(receipt(" 7k3m9qxz ").as_deref(), Some("7K3M9QXZ"));
        assert_eq!(receipt("00000000").as_deref(), Some("00000000"));
        assert_eq!(receipt("7K3M9QX"), None); // too short
        assert_eq!(receipt("7K3M9QXZA"), None); // too long
        assert_eq!(receipt("7K3M9QXI"), None); // I, L, O, U aren't Crockford
        assert_eq!(receipt("ULOI0000"), None);
        assert_eq!(receipt("7K3M-QXZ"), None);
        assert_eq!(receipt(""), None);
        assert_eq!(receipt("ÄK3M9QXZ"), None);
    }

    #[test]
    fn code_format() {
        assert_eq!(code(Some("AAAAAAAA"), Some("BBBBBBBB")), "EVL-AAAAAAAA-BBBBBBBB");
        assert_eq!(code(None, Some("BBBBBBBB")), "EVL-0-BBBBBBBB");
        assert_eq!(code(Some("AAAAAAAA"), None), "EVL-AAAAAAAA-0");
        assert_eq!(code(None, None), "EVL-0-0");
    }

    #[test]
    fn view_and_checklist() {
        let none = view(&Receipts::default());
        assert_eq!(none, View { code: "EVL-0-0".into(), auctions: false, collector: false, complete: false });
        let both = view(&Receipts { auctions: Some(rc("AAAAAAAA", true)), collector: Some(rc("BBBBBBBB", true)) });
        assert_eq!(both, View { code: "EVL-AAAAAAAA-BBBBBBBB".into(), auctions: true, collector: true, complete: true });
        // an auctions receipt without a full scan is in the code, but the full scan isn't ticked
        let part = view(&Receipts { auctions: Some(rc("AAAAAAAA", false)), collector: Some(rc("BBBBBBBB", true)) });
        assert_eq!((part.code.as_str(), part.auctions, part.collector, part.complete), ("EVL-AAAAAAAA-BBBBBBBB", false, true, false));
    }

    #[test]
    fn latest_auctions_receipt_wins_unless_it_loses_the_full_scan() {
        let mut r = Receipts::default();
        keep_auctions(&mut r, rc("AAAAAAAA", false));
        keep_auctions(&mut r, rc("BBBBBBBB", true));
        assert_eq!(r.auctions.as_ref().unwrap().code, "BBBBBBBB");
        keep_auctions(&mut r, rc("CCCCCCCC", true));
        assert_eq!(r.auctions.as_ref().unwrap().code, "CCCCCCCC");
        keep_auctions(&mut r, rc("DDDDDDDD", false));
        assert_eq!(r.auctions.as_ref().unwrap().code, "CCCCCCCC");
    }

    #[test]
    fn switch_and_end_date() {
        assert!(!on(false, "2027-01-01", "2026-12-01"));
        assert!(on(true, "2027-01-01", "2026-12-31"));
        assert!(!on(true, "2027-01-01", "2027-01-01"));
        assert!(!on(true, "2027-01-01", "2027-03-01"));
        assert!(!on(true, "soon", "2026-12-01"));
        // it ships dark
        assert!(!ENABLED);
        assert!(chrono::NaiveDate::parse_from_str(ENDS, "%Y-%m-%d").is_ok());
    }
}
