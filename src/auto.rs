//! Daily automation schedule maths and state. The actual work (find topics, make, upload) runs from main.rs.

use crate::settings::{write_atomic, AutoPlan};
use serde_json::{json, Value};
use std::fs;

/// (local calendar day number, local minutes since midnight) right now.
pub fn local_now(tz_offset_min: i64) -> (i64, i64) {
    let local = crate::youtube::now_secs() as i64 - tz_offset_min * 60;
    (local.div_euclid(86400), local.rem_euclid(86400) / 60)
}

/// "HH:MM" -> minutes since midnight.
pub fn hhmm(t: &str) -> Option<i64> {
    let (h, m) = t.trim().split_once(':')?;
    let (h, m): (i64, i64) = (h.parse().ok()?, m.parse().ok()?);
    ((0..24).contains(&h) && (0..60).contains(&m)).then_some(h * 60 + m)
}

/// Publish times for `n` shorts: the user's times in order; any extras go 2 hours after the last one.
pub fn slot_minutes(plan: &AutoPlan, n: usize) -> Vec<i64> {
    let mut t: Vec<i64> = plan.times.iter().filter_map(|x| hhmm(x)).collect();
    t.sort();
    if t.is_empty() {
        t.push(12 * 60);
    }
    while t.len() < n {
        let last = *t.last().unwrap();
        t.push((last + 120).min(23 * 60 + 59));
    }
    t.truncate(n);
    t
}

/// Local day + minute -> UTC RFC 3339 for YouTube's publishAt.
pub fn slot_rfc3339(day: i64, minute: i64, tz_offset_min: i64) -> String {
    crate::youtube::to_rfc3339((day * 86400 + minute * 60 + tz_offset_min * 60).max(0) as u64)
}

fn state() -> Value {
    fs::read_to_string(crate::settings::channel_dir().join("auto_state.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(json!({"last_day": -1, "history": []}))
}

/// Is today's batch due? (Plan on, within its days, not done today, past the "make at" time.)
pub fn due(plan: &AutoPlan) -> Option<i64> {
    if !plan.enabled || plan.per_day == 0 {
        return None;
    }
    let (day, minute) = local_now(plan.tz_offset_min);
    let in_range = day >= plan.start_day && day < plan.start_day + plan.days as i64;
    let done = state()["last_day"].as_i64() == Some(day);
    (in_range && !done && minute >= hhmm(&plan.make_at).unwrap_or(0)).then_some(day)
}

/// Mark a day as started before working on it, so a crash or restart never makes the same day twice.
pub fn mark_started(day: i64) {
    let mut st = state();
    st["last_day"] = day.into();
    let _ = write_atomic(&crate::settings::channel_dir().join("auto_state.json"), &st.to_string());
}

pub fn record(day: i64, made: usize, uploaded: usize, note: &str) {
    let mut st = state();
    let mut h = st["history"].as_array().cloned().unwrap_or_default();
    h.insert(0, json!({"day": day, "made": made, "uploaded": uploaded, "note": note, "at": crate::youtube::now_secs()}));
    h.truncate(60);
    st["history"] = h.into();
    let _ = write_atomic(&crate::settings::channel_dir().join("auto_state.json"), &st.to_string());
}

/// For the UI: where the plan stands.
pub fn status(plan: &AutoPlan) -> Value {
    let (day, _) = local_now(plan.tz_offset_min);
    let st = state();
    json!({
        "today": day, "day_of_plan": day - plan.start_day + 1, "days_left": (plan.start_day + plan.days as i64 - day).max(0),
        "done_today": st["last_day"].as_i64() == Some(day), "history": st["history"]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_maths() {
        assert_eq!(hhmm("06:30"), Some(390));
        assert_eq!(hhmm("24:00"), None);
        let plan = AutoPlan { times: vec!["18:00".into(), "09:00".into()], ..Default::default() };
        assert_eq!(slot_minutes(&plan, 3), vec![540, 1080, 1200]); // sorted, extra 2 h after the last
        assert_eq!(slot_minutes(&plan, 1), vec![540]);
        // SAST (UTC+2, offset -120): local 2026-11-27 12:00 = 10:00 UTC
        let day = 1_795_795_200 / 86400; // 2026-11-27
        assert_eq!(slot_rfc3339(day, 12 * 60, -120), "2026-11-27T10:00:00Z");
    }
}
