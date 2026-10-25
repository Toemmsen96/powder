//! Challenge leaderboards: the server's players ranked by their best score on a challenge.
//!
//! The game names a board "public_challenge_<ascending|descending>[_ranked]_ldb:<challenge id>:<period>" (period
//! infinite, weekly or daily) and asks either for some players (its own player and friends,
//! GetLeaderboardForPopulations) or for a range of ranks (GetLeaderboard, e.g. rank 1 = the record). A
//! player-made challenge's board has no "public_" ("challenge_ascending_ldb:<creator>_<savename>:infinite").
//! Ascending: lower is better (times); descending: higher is better (points). Each player's best is kept, with
//! when it was set, so a weekly or daily board shows the bests set in that period. A row's associated_replay is
//! the best's ghost on ranked boards: the game races the record holder's ghost and, finishing a ranked run that
//! beats a record, uploads its own (OnlineTrailService).

use crate::repos::{Repos, ScoreRow};
use anyhow::Result;
use chrono::{DateTime, Datelike, Duration, NaiveTime, Utc};
use regex::Regex;
use std::collections::HashSet;
use std::sync::LazyLock;

static BOARD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:public_)?challenge_(ascending|descending)(?:_ranked)?_ldb:(.+):(infinite|weekly|daily)$").unwrap()
});

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub profile_id: String,
    pub rank: u32,
    pub score: f64,
    pub replay: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Board {
    /// Players on the board.
    pub total: u32,
    /// Ranked, best first.
    pub rows: Vec<Row>,
}

/// Board name -> (direction, challenge id, period), None if it is not a challenge board.
pub fn parse(name: &str) -> Option<(String, String, String)> {
    let m = BOARD.captures(name)?;
    Some((m[1].to_string(), m[2].to_string(), m[3].to_string()))
}

fn since(period: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let midnight = |t: DateTime<Utc>| t.with_time(NaiveTime::MIN).unwrap();
    match period {
        "daily" => Some(midnight(now)),
        "weekly" => Some(midnight(now - Duration::days(now.weekday().num_days_from_monday() as i64))),
        _ => None,
    }
}

/// The board from these bests (one challenge's); `has_ghost(replay)`: does that ghost entity still exist.
pub fn rank(name: &str, scores: Vec<ScoreRow>, now: DateTime<Utc>, has_ghost: impl Fn(&str) -> bool) -> Board {
    let Some((direction, _, period)) = parse(name) else { return Board::default() };
    let since = since(&period, now);
    let mut scores: Vec<ScoreRow> = scores.into_iter().filter(|s| match since {
        None => true,
        Some(since) => DateTime::parse_from_rfc3339(&s.updated.replace('Z', "+00:00"))
            .is_ok_and(|when| when >= since),
    }).collect();
    let descending = direction == "descending";
    // stable, like Python's sort (reverse=True keeps equal scores in their order too)
    scores.sort_by(|a, b| {
        let o = a.best_score.partial_cmp(&b.best_score).unwrap_or(std::cmp::Ordering::Equal);
        if descending { o.reverse() } else { o }
    });
    let (mut rows, mut rank, mut previous) = (vec![], 0u32, None);
    for (i, s) in scores.into_iter().enumerate() {
        if previous != Some(s.best_score) {
            // equal scores share a rank
            (rank, previous) = (i as u32 + 1, Some(s.best_score));
        }
        // a best's ghost (BEST_GHOST_* entity id), "none" once the game has deleted that entity: it replaces its
        // ghost entities and would otherwise try to download a missing one
        let replay = match s.replay.as_deref() {
            Some(r) if r != "none" && !r.is_empty() && has_ghost(r) => r.to_string(),
            _ => "none".to_string(),
        };
        rows.push(Row { profile_id: s.profile_id, rank, score: s.best_score, replay });
    }
    Board { total: rows.len() as u32, rows }
}

pub fn board(repos: &Repos, name: &str, now: DateTime<Utc>) -> Result<Board> {
    let Some((_, challenge, _)) = parse(name) else { return Ok(Board::default()) };
    let scores = repos.scores_for_challenge(&challenge)?;
    Ok(rank(name, scores, now, |r| repos.entity_get(r).ok().flatten().is_some()))
}

/// GetLeaderboardForPopulations: the rows of these players (with their global rank).
pub fn for_players(repos: &Repos, name: &str, profile_ids: &[String]) -> Result<Board> {
    Ok(only(board(repos, name, Utc::now())?, profile_ids))
}

/// GetLeaderboard: `count` rows from rank `first` (1-based).
pub fn ranks(repos: &Repos, name: &str, first: u32, count: u32) -> Result<Board> {
    Ok(slice(board(repos, name, Utc::now())?, first, count))
}

fn only(b: Board, profile_ids: &[String]) -> Board {
    let wanted: HashSet<String> = profile_ids.iter().map(|p| p.to_lowercase()).collect();
    Board { total: b.total, rows: b.rows.into_iter().filter(|r| wanted.contains(&r.profile_id)).collect() }
}

fn slice(b: Board, first: u32, count: u32) -> Board {
    let start = (first.max(1) - 1) as usize;
    Board { total: b.total, rows: b.rows.into_iter().skip(start).take(count as usize).collect() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const A: &str = "11111111-1111-1111-1111-111111111111";
    const B: &str = "22222222-2222-2222-2222-222222222222";
    const C: &str = "33333333-3333-3333-3333-333333333333";

    fn s(p: &str, score: f64, updated: &str) -> ScoreRow {
        ScoreRow { profile_id: p.into(), challenge_id: "Steep_A03".into(), direction: "x".into(), best_score: score,
                   replay: Some("none".into()), updated: updated.into() }
    }

    #[test]
    fn ranks_by_direction_with_ties() {
        let now = Utc.with_ymd_and_hms(2026, 10, 10, 12, 0, 0).unwrap();
        let scores = || vec![s(A, 1708.0, "2026-10-10T10:00:00Z"), s(B, 2500.0, "2026-10-01T10:00:00Z"),
                             s(C, 1708.0, "2026-10-10T11:00:00.123Z")];
        let r = |name: &str| rank(name, scores(), now, |_| true);
        let points = r("public_challenge_descending_ldb:Steep_A03:infinite");
        let order: Vec<(&str, u32)> = points.rows.iter().map(|x| (x.profile_id.as_str(), x.rank)).collect();
        assert_eq!((points.total, order), (3, vec![(B, 1), (A, 2), (C, 2)]));
        let times = r("public_challenge_ascending_ranked_ldb:Steep_A03:infinite");
        assert_eq!(times.rows[2], Row { profile_id: B.into(), rank: 3, score: 2500.0, replay: "none".into() });
        let week = r("public_challenge_descending_ldb:Steep_A03:weekly"); // B's best is from last week
        assert_eq!(week.rows.iter().map(|x| x.profile_id.as_str()).collect::<Vec<_>>(), vec![A, C]);
        assert_eq!(r("public_challenge_descending_ldb:Steep_A03:daily").total, 2);
        assert_eq!(r("something_else").total, 0);
        assert_eq!(r("challenge_descending_ldb:Steep_A03:infinite").total, 3); // a player-made one's
        assert_eq!(r("tournament_challenge_descending_ldb:Steep_A03:infinite").total, 0);
        let b = rank("public_challenge_descending_ldb:Steep_A03:infinite", scores(), now, |_| true);
        assert_eq!(only(b.clone(), &[C.to_uppercase()]).rows[0].profile_id, C);
        assert_eq!(slice(b.clone(), 0, 1).rows[0].profile_id, B);
        assert_eq!(slice(b, 3, 5).rows.len(), 1);
    }

    #[test]
    fn ghosts_that_are_gone_are_none() {
        let mut row = s(A, 1.0, "2026-10-10T10:00:00Z");
        row.replay = Some("ghost-1".into());
        let name = "public_challenge_ascending_ranked_ldb:Steep_A03:infinite";
        assert_eq!(rank(name, vec![row.clone()], Utc::now(), |_| true).rows[0].replay, "ghost-1");
        assert_eq!(rank(name, vec![row], Utc::now(), |_| false).rows[0].replay, "none");
    }
}
