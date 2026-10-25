//! NEX calls answered without recordings: per-player profiles and challenge leaderboards (the server's players,
//! services/leaderboards.rs). Formats from the replies of the real server.

use super::{HandlerDeps, HandlerFn, Result};
use crate::nex::dispatch::Ctx;
use crate::nex::rmc::{parse_guid, Reader, Variant, Writer};
use crate::services::leaderboards::{self, Board};
use uuid::Uuid;

pub const HANDLERS: &[(&str, HandlerFn)] = &[
    ("ClansProtocol::GetPlayerProfile_V1", clans_player_profile),
    ("PlayerStatusProtocol::GetProfile_V1", player_status_profile),
    ("SteepTournamentProtocol::FetchPlayerProfileInfo_V1", tournament_player_info),
    ("PlayerStats2Protocol::GetLeaderboard_V1", leaderboard),
    ("PlayerStats2Protocol::GetLeaderboardForPopulations_V1", leaderboard_for_populations),
];

/// ClansProtocol::GetPlayerProfile_V1: the player is in no clan (32 zero bytes, as the real server).
pub fn clans_player_profile(_: &HandlerDeps, _: &mut Ctx, _: &mut Reader) -> Result {
    Ok(vec![0; 32])
}

/// PlayerStatusProtocol::GetProfile_V1: GUID of the player (profile id) + 8 zero bytes.
pub fn player_status_profile(_: &HandlerDeps, ctx: &mut Ctx, _: &mut Reader) -> Result {
    let pid = match &ctx.conn.profile_id {
        Some(p) => parse_guid(p)?,
        None => Uuid::nil(),
    };
    Ok(Writer::new().guid(&pid).u64(0).take())
}

/// SteepTournamentProtocol::FetchPlayerProfileInfo_V1: no tournament progress (22 zero bytes).
pub fn tournament_player_info(_: &HandlerDeps, _: &mut Ctx, _: &mut Reader) -> Result {
    Ok(vec![0; 22])
}

/// u32 total players, u32 n rows, then n x (String profile id, u32 rank, f64 score, u32 m, m x (String key,
/// Variant)); the one value recorded is associated_replay (a replay id or "none").
pub fn encode_board(b: &Board) -> Vec<u8> {
    let mut w = Writer::new();
    w.u32(b.total).u32(b.rows.len() as u32);
    for row in &b.rows {
        w.string(&row.profile_id).u32(row.rank).f64(row.score).u32(1).string("associated_replay")
            .variant(&Variant::String(row.replay.clone()));
    }
    w.take()
}

/// GetLeaderboardForPopulations_V1: type map, String board, List<String> profile ids (the player and friends) ->
/// their rows, with their rank among all the server's players.
pub fn leaderboard_for_populations(deps: &HandlerDeps, _: &mut Ctx, r: &mut Reader) -> Result {
    r.type_map()?;
    let name = r.string()?;
    let pids = r.strings()?;
    Ok(encode_board(&leaderboards::for_players(&deps.repos, &name, &pids)?))
}

/// GetLeaderboard_V1: type map, String board, u32 first rank, u32 count (1, 1: the record).
pub fn leaderboard(deps: &HandlerDeps, _: &mut Ctx, r: &mut Reader) -> Result {
    r.type_map()?;
    let name = r.string()?;
    let (first, count) = (r.u32()?, r.u32()?);
    Ok(encode_board(&leaderboards::ranks(&deps.repos, &name, first, count)?))
}

#[cfg(test)]
mod tests {
    use super::super::testing;
    use super::*;
    use crate::config::Config;
    use crate::nex::connection::{Connection, SendFn};
    use crate::nex::rmc::{self, V_STRING};
    use std::sync::Arc;

    const A: &str = "11111111-1111-1111-1111-111111111111";
    const B: &str = "22222222-2222-2222-2222-222222222222";
    const C: &str = "33333333-3333-3333-3333-333333333333";

    fn call(deps: &Arc<HandlerDeps>, profile_id: Option<&str>, method: &str, params: &[u8]) -> Vec<u8> {
        let d = testing::dispatcher(deps);
        let send_fn: SendFn = Arc::new(|_, _| {});
        let mut conn = Connection::new("192.0.2.9:1".parse().unwrap(), send_fn, 1, 0x3f, 0x31, 0.0);
        conn.profile_id = profile_id.map(String::from);
        let mut ctx = Ctx { conn: &mut conn, station_url: String::new(), now: 0.0 };
        let protocol = method.split("::").next().unwrap();
        let out = rmc::parse(&d.dispatch(&mut ctx, &rmc::parse(&rmc::build_request(protocol, 3, method, params))
            .unwrap())).unwrap();
        assert_eq!(out.error, None, "{method}");
        out.body
    }

    #[test]
    fn population_request_and_reply_as_recorded() {
        let deps = testing::deps(Config::default());
        deps.repos.set_score(A, "Steep_A03", "descending", 1708.0, Some("none")).unwrap();
        deps.repos.set_score(B, "Steep_A03", "descending", 2500.0, Some("none")).unwrap();
        let params = Writer::new().u32(2).string("Leaderboard").u16(1).string("LeaderboardRow").u16(1)
            .string("public_challenge_descending_ldb:Steep_A03:infinite").u32(2).string(A).string(C).take();
        let body = call(&deps, None, "PlayerStats2Protocol::GetLeaderboardForPopulations_V1", &params);
        let mut out = Reader::new(&body);
        assert_eq!((out.u32().unwrap(), out.u32().unwrap()), (2, 1)); // 2 on the board, A's row asked (C has none)
        assert_eq!((out.string().unwrap(), out.u32().unwrap(), out.f64().unwrap()), (A.into(), 2, 1708.0));
        assert_eq!((out.u32().unwrap(), out.string().unwrap().as_str(), out.u8().unwrap()),
                   (1, "associated_replay", V_STRING));
        assert_eq!((out.string().unwrap().as_str(), out.remaining()), ("none", 0));
    }

    #[test]
    fn record_holder_request() {
        let deps = testing::deps(Config::default());
        deps.repos.set_score(A, "Steep_A09", "ascending", 61.5, None).unwrap();
        deps.repos.set_score(B, "Steep_A09", "ascending", 58.25, Some("best_ghost_gone")).unwrap();
        let params = Writer::new().u32(2).string("Leaderboard").u16(1).string("LeaderboardRow").u16(1)
            .string("public_challenge_ascending_ranked_ldb:Steep_A09:infinite").u32(1).u32(1).take();
        let body = call(&deps, None, "PlayerStats2Protocol::GetLeaderboard_V1", &params);
        let mut expected = Writer::new();
        expected.u32(2).u32(1).string(B).u32(1).f64(58.25).u32(1).string("associated_replay").u8(V_STRING)
            .string("none");
        assert_eq!(body, expected.take());
    }

    #[test]
    fn fixed_profiles() {
        let deps = testing::deps(Config::default());
        assert_eq!(call(&deps, None, "ClansProtocol::GetPlayerProfile_V1", b""), vec![0; 32]);
        assert_eq!(call(&deps, None, "SteepTournamentProtocol::FetchPlayerProfileInfo_V1", b""), vec![0; 22]);
        assert_eq!(call(&deps, None, "PlayerStatusProtocol::GetProfile_V1", b""), vec![0; 24]);
        let body = call(&deps, Some("00112233-4455-6677-8899-aabbccddeeff"), "PlayerStatusProtocol::GetProfile_V1", b"");
        assert_eq!(hex::encode(body), "33221100554477669988bbaaddccffee0000000000000000");
    }
}
