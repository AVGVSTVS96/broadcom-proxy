//! The pool's decisions as pure functions over what is known about each account:
//! which account takes new work, when a session should leave its account, and when to
//! spend a held weekly reset.
//!
//! The pool spends every account's weekly limit before it resets without letting any one
//! hit its five-hour limit. Agent SDK and `claude -p` sessions drain the five-hour limit
//! faster than interactive ones, so they weigh more there.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

pub const SECOND: i64 = 1000;
pub const MINUTE: i64 = 60 * SECOND;
pub const HOUR: i64 = 60 * MINUTE;
pub const FIVE_HOURS: i64 = 5 * HOUR;
pub const WEEK: i64 = 7 * 24 * HOUR;

/// How far below the five-hour cutoff a session starts looking for a calmer account.
const MARGIN: f64 = 10.0;
/// A held reset is spent when every account has used its week and none comes back for this long.
const DRY_SPELL: i64 = 12 * HOUR;
const LAST_WEEK: i64 = WEEK;
const LAST_HOUR: i64 = HOUR;

/// One limit: percent used and when the window resets (unix ms, 0 unknown).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Window {
    pub used: f64,
    pub resets_at: i64,
}

/// An account's five-hour and weekly limits as last read. A missing window hasn't started,
/// or the plan has no such limit.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Reading {
    pub five_hour: Option<Window>,
    pub week: Option<Window>,
    pub at: i64,
}

impl Reading {
    /// The reading with windows whose reset time has passed counted as empty.
    #[must_use]
    pub fn live(self, now: i64) -> Self {
        let live =
            |w: Option<Window>| w.map(|w| if w.resets_at > 0 && w.resets_at <= now { Window::default() } else { w });
        Self { five_hour: live(self.five_hour), week: live(self.week), at: self.at }
    }
}

/// A limit reset an account holds.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Reset {
    pub id: String,
    pub kind: String,
    pub left: i64,
    pub expires_at: Option<i64>,
    pub paused: bool,
    pub usable_now: bool,
    /// Anthropic lets this reset be used only at the limit it refills.
    pub needs_limit: bool,
    pub weekly: bool,
}

impl Reset {
    pub fn live(&self, now: i64) -> bool {
        self.left > 0 && !self.paused && self.weekly && self.expires_at.is_none_or(|at| at > now)
    }

    fn time_left(&self, now: i64) -> i64 {
        self.expires_at.map_or(i64::MAX, |at| at - now)
    }
}

/// The cutoffs the pool keeps accounts under.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Policy {
    pub session_cutoff: f64,
    pub weekly_cutoff: f64,
    pub agents_per_account: u32,
}

impl Default for Policy {
    fn default() -> Self {
        Self { session_cutoff: 90.0, weekly_cutoff: 98.0, agents_per_account: 100 }
    }
}

impl Policy {
    pub fn validate(&self) -> Result<(), &'static str> {
        let cutoff = |v: f64| v.is_finite() && v > 0.0 && v <= 100.0;
        if !cutoff(self.session_cutoff) || !cutoff(self.weekly_cutoff) {
            return Err("Cutoffs must be above 0 and at most 100.");
        }
        if !(1..=500).contains(&self.agents_per_account) {
            return Err("Agents per account must be a whole number from 1 to 500.");
        }
        Ok(())
    }
}

/// What ranking needs to know about one account.
#[derive(Clone, Debug, Default)]
pub struct View<'a> {
    pub label: &'a str,
    pub reading: Reading,
    pub resets: &'a [Reset],
    pub needs_reauth: bool,
    /// Sessions seen on the account in the last two minutes.
    pub working: usize,
    /// Those sessions' five-hour weight, Agent SDK ones counting more.
    pub load: f64,
}

/// One account's standing for new work. `index` points into the views it was ranked from.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Row {
    pub index: usize,
    pub five: f64,
    pub week: f64,
    pub week_cutoff: f64,
    pub usable: bool,
    pub open: bool,
    pub known: bool,
    pub comfortable: bool,
    pub holds_reset: bool,
    pub needs_reauth: bool,
    pub working: usize,
    pub room: f64,
    pub urgency: f64,
    pub week_resets_at: i64,
    pub free_at: i64,
}

/// Why a session left its account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    /// Its account failed or was unavailable.
    Limit,
    /// Its account was past a cutoff.
    Over,
    /// Its account was nearing a cutoff and a calmer one was free.
    Early,
}

impl Cause {
    pub fn as_str(self) -> &'static str {
        match self {
            Cause::Limit => "limit",
            Cause::Over => "over",
            Cause::Early => "early",
        }
    }
}

/// The next time a periodic window resets after `now`, from one known reset time.
fn next_reset(mut at: i64, now: i64, period: i64) -> i64 {
    if at <= 0 {
        return 0;
    }
    while at <= now {
        at += period;
    }
    at
}

/// Accounts under both cutoffs first; among them one holding a reset, so its week runs out
/// first and the reset clears a whole one; then those with their share of five-hour room per
/// working session; then sampled ones; then the account whose remaining week runs out
/// soonest, then the most five-hour room per working session. With nothing usable, the most
/// real room wins. `weight` is the incoming session's five-hour weight.
pub fn rank(views: &[View], policy: &Policy, now: i64, weight: f64) -> Vec<Row> {
    let mut rows: Vec<Row> = views.iter().enumerate().map(|(index, v)| row(index, v, policy, now, weight)).collect();
    rows.sort_by(order);
    rows
}

fn row(index: usize, v: &View, policy: &Policy, now: i64, weight: f64) -> Row {
    let r = v.reading;
    let (mut load, mut incoming) = (v.load, weight);
    if r.five_hour.is_none() && r.at > 0 {
        // A plan without a five-hour limit counts every session alike.
        (load, incoming) = (v.working as f64, 1.0);
    }
    let five = r.five_hour.map_or(0.0, |w| w.used);
    let week = r.week.map_or(0.0, |w| w.used);
    let holds_reset = v.resets.iter().any(|g| g.left > 0 && !g.paused && g.expires_at.is_none_or(|at| at > now));
    let week_cutoff = if v.resets.iter().any(|g| g.needs_limit && g.live(now)) { 100.0 } else { policy.weekly_cutoff };
    let five_room = policy.session_cutoff - five;
    let week_room = week_cutoff - week;
    let room = five_room / (load + incoming);
    let week_resets_at = r.week.map(|w| next_reset(w.resets_at, now, WEEK)).filter(|&at| at > 0).unwrap_or(now + WEEK);
    let mut free_at = 0;
    if five_room <= 0.0
        && let Some(w) = r.five_hour
    {
        free_at = next_reset(w.resets_at, now, FIVE_HOURS);
    }
    if week_room <= 0.0 && week_resets_at > free_at {
        free_at = week_resets_at;
    }
    Row {
        index,
        five,
        week,
        week_cutoff,
        usable: five_room > 0.0 && week_room > 0.0 && !v.needs_reauth,
        open: five < 100.0 && week < 100.0 && !v.needs_reauth,
        known: r.at > 0,
        comfortable: room >= policy.session_cutoff / f64::from(policy.agents_per_account) && week_room >= 5.0,
        holds_reset,
        needs_reauth: v.needs_reauth,
        working: v.working,
        room,
        urgency: week_room / ((week_resets_at - now) as f64 / HOUR as f64).max(1.0),
        week_resets_at,
        free_at,
    }
}

fn order(a: &Row, b: &Row) -> Ordering {
    let first = |x: bool| if x { Ordering::Less } else { Ordering::Greater };
    let higher = |x: f64, y: f64| y.partial_cmp(&x).unwrap_or(Ordering::Equal);
    if a.usable != b.usable {
        return first(a.usable);
    }
    if !a.usable {
        if a.open != b.open {
            return first(a.open);
        }
        if a.open {
            return a.five.partial_cmp(&b.five).unwrap_or(Ordering::Equal);
        }
        return a.free_at.cmp(&b.free_at);
    }
    if a.holds_reset != b.holds_reset {
        return first(a.holds_reset);
    }
    if a.comfortable != b.comfortable {
        return first(a.comfortable);
    }
    if a.known != b.known {
        return first(a.known);
    }
    let urgency = higher(a.urgency, b.urgency);
    if a.comfortable && urgency != Ordering::Equal {
        return urgency;
    }
    higher(a.room, b.room).then(urgency)
}

/// Whether a session on the account at `index` should leave it: `Over` past a cutoff when
/// any other account is usable, `Early` nearing one when a calm account is free. An Agent
/// SDK session nears the five-hour cutoff faster, so its margin is wider by its weight.
pub fn move_away(rows: &[Row], index: usize, policy: &Policy, weight: f64) -> Option<Cause> {
    let mine = rows.iter().find(|r| r.index == index)?;
    let urgent = !mine.usable;
    let soon = mine.five >= policy.session_cutoff - MARGIN * weight || mine.week_cutoff - mine.week < 5.0;
    if !urgent && !soon {
        return None;
    }
    let calm =
        |r: &Row| r.usable && r.comfortable && r.five < policy.session_cutoff - MARGIN && r.week_cutoff - r.week >= 5.0;
    let better = rows.iter().any(|r| r.index != index && (calm(r) || (urgent && r.usable)));
    better.then_some(if urgent { Cause::Over } else { Cause::Early })
}

/// A held weekly reset and what the pool does with it: spend it now (`why` set) or keep it,
/// with `plan` saying which in words.
#[derive(Clone, Debug, PartialEq)]
pub struct ResetPlan {
    pub index: usize,
    pub grant: Reset,
    pub why: Option<String>,
    pub plan: String,
}

/// A held weekly reset zeroes an account's week. The pool spends one the moment every
/// account has used its week: before that, the room it gives would only stand in for room
/// other accounts already have. It waits out a dry spell shorter than 12 hours, when a week
/// comes back on its own soon, unless the reset expires within a week. One reset is spent
/// per dry spell, the one whose account's week ends last. A reset in its last hour is spent
/// whatever the pool holds. A reset that only works at the weekly limit makes its holder
/// take work past the weekly cutoff until it gets there.
pub fn plan_resets(views: &[View], rows: &[Row], now: i64, when: impl Fn(i64) -> String) -> Vec<ResetPlan> {
    let mut busy = Vec::new();
    let mut dry_until = i64::MAX;
    let mut holders: Vec<(&Row, ResetPlan)> = Vec::new();
    for r in rows {
        if r.needs_reauth {
            continue;
        }
        if !r.known || r.week < r.week_cutoff {
            busy.push(views[r.index].label);
        }
        dry_until = dry_until.min(r.week_resets_at);
        let grant = views[r.index].resets.iter().filter(|g| g.live(now)).min_by_key(|g| g.time_left(now));
        if let Some(grant) = grant {
            holders.push((r, ResetPlan { index: r.index, grant: grant.clone(), why: None, plan: String::new() }));
        }
    }
    holders.sort_by_key(|(row, _)| std::cmp::Reverse(row.week_resets_at));
    let spend = |h: &mut ResetPlan, why: String| {
        h.plan = format!("Spending it now: {why}.");
        h.why = Some(why);
    };

    let mut spent = false;
    for (row, h) in &mut holders {
        if h.grant.time_left(now) >= LAST_HOUR || row.week <= 0.0 {
            continue;
        }
        let expires = when(h.grant.expires_at.unwrap_or_default());
        if h.grant.needs_limit && row.week < 100.0 {
            h.plan = format!(
                "Expires {expires} and only works at the weekly limit, so {} takes new sessions first to reach it.",
                views[row.index].label
            );
            continue;
        }
        spend(h, format!("the reset expires {expires}"));
        spent = true;
    }

    let dry = dry_until.saturating_sub(now);
    for (_, h) in &mut holders {
        if !h.plan.is_empty() {
            continue;
        }
        if !busy.is_empty() {
            let one = busy.len() == 1;
            h.plan = format!(
                "Spends itself once {} {} used {} week.",
                busy.join(" and "),
                if one { "has" } else { "have" },
                if one { "its" } else { "their" }
            );
        } else if spent {
            h.plan = "Every account has used its week, but another reset is being spent, so this one waits for the next time they all have.".into();
        } else if dry >= DRY_SPELL {
            spend(h, format!("every account had used its week and none came back before {}", when(dry_until)));
            spent = true;
        } else if h.grant.time_left(now) < LAST_WEEK {
            let expires = when(h.grant.expires_at.unwrap_or_default());
            spend(h, format!("every account had used its week and the reset expires {expires}"));
            spent = true;
        } else {
            h.plan = format!(
                "Every account has used its week, but one comes back {}, so the reset waits for a longer gap.",
                when(dry_until)
            );
        }
    }
    holders.into_iter().map(|(_, h)| h).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000_000;

    /// An account read just now at `five`% and `week`%, its five-hour window resetting in an
    /// hour and its week in `week_reset_in` ms.
    fn reading(five: f64, week: f64, week_reset_in: i64) -> Reading {
        Reading {
            five_hour: Some(Window { used: five, resets_at: NOW + HOUR }),
            week: Some(Window { used: week, resets_at: NOW + week_reset_in }),
            at: NOW,
        }
    }

    fn view(label: &str, five: f64, week: f64, week_reset_in: i64) -> View<'_> {
        View { label, reading: reading(five, week, week_reset_in), ..View::default() }
    }

    fn first<'a>(views: &'a [View], weight: f64) -> &'a str {
        let rows = rank(views, &Policy::default(), NOW, weight);
        views[rows[0].index].label
    }

    fn weekly(expires_in: i64) -> Reset {
        Reset {
            id: "free_reset".into(),
            kind: "weekly".into(),
            left: 1,
            expires_at: Some(NOW + expires_in),
            weekly: true,
            usable_now: true,
            ..Reset::default()
        }
    }

    #[test]
    fn skips_accounts_past_a_cutoff() {
        let views =
            [view("a", 95.0, 10.0, 48 * HOUR), view("b", 20.0, 99.0, 48 * HOUR), view("c", 50.0, 50.0, 96 * HOUR)];
        assert_eq!(first(&views, 1.0), "c");
    }

    #[test]
    fn spends_the_week_that_runs_out_soonest() {
        let views = [view("later", 10.0, 40.0, 120 * HOUR), view("soon", 10.0, 40.0, 10 * HOUR)];
        assert_eq!(first(&views, 1.0), "soon");
    }

    #[test]
    fn prefers_an_account_holding_a_reset() {
        let held = [Reset { kind: "weekly".into(), left: 1, expires_at: Some(NOW + 24 * HOUR), ..Reset::default() }];
        let holder = View { resets: &held, ..view("holder", 10.0, 40.0, 120 * HOUR) };
        assert_eq!(first(&[view("soon", 10.0, 40.0, 10 * HOUR), holder], 1.0), "holder");
    }

    #[test]
    fn agent_sdk_sessions_load_the_five_hour_room_by_their_factor() {
        let x = View { working: 1, load: 1.7, ..view("x", 50.0, 30.0, 48 * HOUR) };
        let y = View { working: 1, load: 1.0, ..view("y", 50.0, 30.0, 48 * HOUR) };
        assert_eq!(first(&[x, y], 1.0), "y");
    }

    #[test]
    fn an_account_without_a_five_hour_limit_counts_sessions_alike() {
        let mut x = View { working: 1, load: 1.7, ..view("x", 0.0, 30.0, 48 * HOUR) };
        let mut y = View { working: 1, load: 1.0, ..view("y", 0.0, 30.0, 48 * HOUR) };
        x.reading.five_hour = None;
        y.reading.five_hour = None;
        let rows = rank(&[x, y], &Policy::default(), NOW, 1.7);
        assert!((rows[0].room - rows[1].room).abs() < f64::EPSILON);
    }

    #[test]
    fn an_expired_window_counts_as_empty() {
        let mut stale = view("stale", 95.0, 99.0, 48 * HOUR);
        stale.reading.five_hour = Some(Window { used: 95.0, resets_at: NOW - MINUTE });
        stale.reading.week = Some(Window { used: 99.0, resets_at: NOW - MINUTE });
        stale.reading = stale.reading.live(NOW);
        assert!(rank(&[stale], &Policy::default(), NOW, 1.0)[0].usable);
    }

    #[test]
    fn move_away_leaves_an_account_near_its_cutoff_only_for_a_clear_one() {
        let policy = Policy::default();
        let views = [view("hot", 85.0, 30.0, 48 * HOUR), view("calm", 10.0, 30.0, 48 * HOUR)];
        let rows = rank(&views, &policy, NOW, 1.0);
        assert_eq!(move_away(&rows, 0, &policy, 1.0), Some(Cause::Early));
        assert_eq!(move_away(&rows, 1, &policy, 1.0), None, "moved a session off a calm account");

        let views = [view("hot", 85.0, 30.0, 48 * HOUR), view("warm", 82.0, 30.0, 48 * HOUR)];
        let rows = rank(&views, &policy, NOW, 1.0);
        assert_eq!(move_away(&rows, 0, &policy, 1.0), None, "moved to an account that is no clearer");
    }

    #[test]
    fn move_away_gives_an_agent_sdk_session_a_wider_margin() {
        let policy = Policy::default();
        let views = [view("hot", 76.0, 30.0, 48 * HOUR), view("calm", 10.0, 30.0, 48 * HOUR)];
        assert_eq!(move_away(&rank(&views, &policy, NOW, 1.0), 0, &policy, 1.0), None);
        assert_eq!(move_away(&rank(&views, &policy, NOW, 1.7), 0, &policy, 1.7), Some(Cause::Early));
    }

    #[test]
    fn move_away_leaves_an_account_past_its_cutoff_for_any_usable_one() {
        let policy = Policy::default();
        let views = [view("over", 92.0, 30.0, 48 * HOUR), view("warm", 82.0, 30.0, 48 * HOUR)];
        assert_eq!(move_away(&rank(&views, &policy, NOW, 1.0), 0, &policy, 1.0), Some(Cause::Over));
    }

    fn plans(views: &[View]) -> Vec<(String, ResetPlan)> {
        let rows = rank(views, &Policy::default(), NOW, 1.0);
        plan_resets(views, &rows, NOW, |ms| format!("@{}h", (ms - NOW) / HOUR))
            .into_iter()
            .map(|p| (views[p.index].label.to_string(), p))
            .collect()
    }

    fn why(all: &[(String, ResetPlan)], label: &str) -> Option<String> {
        all.iter().find(|(l, _)| l == label).and_then(|(_, p)| p.why.clone())
    }

    fn plan_for(views: &[View]) -> ResetPlan {
        plans(views).into_iter().find(|(label, _)| label == "holder").expect("holder has a plan").1
    }

    #[test]
    fn reset_waits_while_another_account_has_week_left() {
        let held = [weekly(40 * 24 * HOUR)];
        let holder = View { resets: &held, ..view("holder", 0.0, 100.0, 110 * HOUR) };
        let plan = plan_for(&[holder, view("b", 0.0, 42.0, 112 * HOUR), view("c", 0.0, 36.0, 124 * HOUR)]);
        assert_eq!(plan.why, None);
        assert!(plan.plan.contains("b and c have used their week"), "{}", plan.plan);
    }

    #[test]
    fn reset_spends_the_moment_the_pool_runs_dry() {
        let held = [weekly(40 * 24 * HOUR)];
        let holder = View { resets: &held, ..view("holder", 0.0, 100.0, 60 * HOUR) };
        let plan = plan_for(&[holder, view("b", 0.0, 98.0, 62 * HOUR), view("c", 0.0, 99.0, 74 * HOUR)]);
        assert!(plan.why.is_some(), "{plan:?}");
    }

    #[test]
    fn reset_waits_out_a_short_dry_spell() {
        let held = [weekly(40 * 24 * HOUR)];
        let holder = View { resets: &held, ..view("holder", 0.0, 100.0, 60 * HOUR) };
        assert_eq!(plan_for(&[holder, view("b", 0.0, 99.0, 3 * HOUR)]).why, None);
    }

    #[test]
    fn reset_takes_a_short_dry_spell_in_its_last_week() {
        let held = [weekly(5 * 24 * HOUR)];
        let holder = View { resets: &held, ..view("holder", 0.0, 100.0, 60 * HOUR) };
        assert!(plan_for(&[holder, view("b", 0.0, 99.0, 3 * HOUR)]).why.is_some());
    }

    #[test]
    fn reset_is_spent_in_its_last_hour() {
        let held = [weekly(30 * MINUTE)];
        let holder = View { resets: &held, ..view("holder", 0.0, 40.0, 60 * HOUR) };
        assert!(plan_for(&[holder, view("b", 0.0, 10.0, 60 * HOUR)]).why.is_some());
    }

    #[test]
    fn every_held_reset_gets_its_last_hour() {
        let (late, expiring) = ([weekly(40 * 24 * HOUR)], [weekly(30 * MINUTE)]);
        let all = plans(&[
            View { resets: &late, ..view("late", 0.0, 100.0, 120 * HOUR) },
            View { resets: &expiring, ..view("expiring", 0.0, 40.0, 20 * HOUR) },
            view("b", 0.0, 10.0, 60 * HOUR),
        ]);
        assert!(why(&all, "expiring").is_some());
        assert!(why(&all, "late").is_none());
    }

    #[test]
    fn one_reset_per_dry_spell_on_the_week_that_ends_last() {
        let (soon, late) = ([weekly(40 * 24 * HOUR)], [weekly(40 * 24 * HOUR)]);
        let all = plans(&[
            View { resets: &soon, ..view("soon", 0.0, 100.0, 20 * HOUR) },
            View { resets: &late, ..view("late", 0.0, 100.0, 120 * HOUR) },
            view("b", 0.0, 99.0, 30 * HOUR),
        ]);
        assert!(why(&all, "late").is_some());
        assert!(why(&all, "soon").is_none());
    }

    #[test]
    fn a_reset_that_needs_the_limit_runs_its_holder_to_the_limit() {
        let held = [Reset { needs_limit: true, ..weekly(30 * MINUTE) }];
        let holder = View { resets: &held, ..view("holder", 0.0, 99.0, 60 * HOUR) };
        let b = view("b", 0.0, 10.0, 60 * HOUR);
        let rows = rank(&[holder.clone(), b.clone()], &Policy::default(), NOW, 1.0);
        assert_eq!(rows[0].index, 0);
        assert!(rows[0].usable, "the holder still takes work at 99%");

        let plan = plan_for(&[holder, b.clone()]);
        assert_eq!(plan.why, None);
        assert!(plan.plan.contains("weekly limit"), "{}", plan.plan);

        let at_limit = View { resets: &held, ..view("holder", 0.0, 100.0, 60 * HOUR) };
        assert!(plan_for(&[at_limit, b]).why.is_some());
    }

    #[test]
    fn policy_rejects_out_of_range_values() {
        assert!(Policy::default().validate().is_ok());
        assert!(Policy { session_cutoff: 0.0, ..Policy::default() }.validate().is_err());
        assert!(Policy { weekly_cutoff: 101.0, ..Policy::default() }.validate().is_err());
        assert!(Policy { agents_per_account: 0, ..Policy::default() }.validate().is_err());
    }
}
