//! GameStorageProtocol and the season info the game asks for at start (no game-data pack needed).

use super::{HandlerDeps, HandlerFn, Result};
use crate::nex::dispatch::Ctx;
use crate::nex::rmc::{quazal_datetime, Reader, Writer};
use chrono::Utc;

pub const HANDLERS: &[(&str, HandlerFn)] = &[
    ("GameStorageProtocol::GetGameStorageTime_V1", get_game_storage_time),
    ("GameStorageProtocol::GetGameParamaters_V1", get_game_parameters),
    ("SteepTournamentProtocol::FetchSeasonInfo_V1", fetch_season_info),
];

/// outputs (reader FUN_7ff661946de0): 8-byte Quazal DateTime = the server's current time.
pub fn get_game_storage_time(_: &HandlerDeps, _: &mut Ctx, _: &mut Reader) -> Result {
    Ok(Writer::new().u64(quazal_datetime(Utc::now())).take())
}

/// GetGameParamaters (sic): the real server answered 8 zero bytes for every request (no parameters).
pub fn get_game_parameters(_: &HandlerDeps, _: &mut Ctx, _: &mut Reader) -> Result {
    Ok(vec![0; 8])
}

/// SteepTournamentProtocol::FetchSeasonInfo_V1: no running season (32 zero bytes) and the server's current time
/// (u32 unix seconds), as the real server answered.
pub fn fetch_season_info(_: &HandlerDeps, _: &mut Ctx, _: &mut Reader) -> Result {
    Ok(Writer::new().raw(&[0; 32]).u32(Utc::now().timestamp() as u32).take())
}

#[cfg(test)]
mod tests {
    use super::super::testing;
    use crate::config::Config;
    use crate::nex::connection::{Connection, SendFn};
    use crate::nex::dispatch::Ctx;
    use crate::nex::rmc::{self, Reader};
    use chrono::{Datelike, Utc};
    use std::sync::Arc;

    #[test]
    fn storage_time_parameters_and_season() {
        let d = testing::dispatcher(&testing::deps(Config::default()));
        let send_fn: SendFn = Arc::new(|_, _| {});
        let mut conn = Connection::new("192.0.2.9:1".parse().unwrap(), send_fn, 1, 0x3f, 0x31, 0.0);
        let mut ctx = Ctx { conn: &mut conn, station_url: String::new(), now: 0.0 };
        let mut call = |proto: &str, method: &str| {
            let out = d.dispatch(&mut ctx, &rmc::parse(&rmc::build_request(proto, 3, method, b"")).unwrap());
            rmc::parse(&out).unwrap().body
        };
        let time = Reader::new(&call("GameStorageProtocol", "GameStorageProtocol::GetGameStorageTime_V1")).u64().unwrap();
        assert_eq!(time >> 26, Utc::now().year() as u64);
        assert_eq!(call("GameStorageProtocol", "GameStorageProtocol::GetGameParamaters_V1"), vec![0; 8]);
        let season = call("SteepTournamentProtocol", "SteepTournamentProtocol::FetchSeasonInfo_V1");
        assert_eq!((season.len(), &season[..32]), (36, &[0u8; 32][..]));
        let t = u32::from_le_bytes(season[32..].try_into().unwrap()) as i64;
        assert!((t - Utc::now().timestamp()).abs() < 5);
    }
}
