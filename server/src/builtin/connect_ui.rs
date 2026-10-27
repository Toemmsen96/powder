//! Ubisoft Connect's window and in-game overlay settings (configcache/api/postauth), without a pack.
//!
//! Connect's main window asks for the same settings: it gets "not modified" (304) while it holds a cached copy
//! it can revalidate, and these otherwise, so without a pack it shows the Powder page too (no Ubisoft settings
//! needed, and no dolphin-032 when Connect has no cache).
//!
//! The overlay (Shift+F2) is a frame (a "shell" from Ubisoft's CDN) that loads its parts ("micro apps":
//! profile, friends, store...) from the addresses these settings list. Unlike Connect's main window, the
//! overlay does not cache them: without them it fails to open (dolphin-040), and with no part listed too
//! (dolphin-029). These are written for Powder: a single part of our own, served by this server
//! (/_powder/overlay/powder.js), and no Ubisoft parts. Achievement pop-ups are drawn by the frame itself
//! (features.achievementNotification). Logs and metrics are off.
//!
//! The part's contract (from the shell's code): a SystemJS module whose default export is a class with a
//! static apiVersion, constructed as new App(shellApi, appConfig) with appName equal to the settings entry's,
//! and lifecycle methods mount(el) -> true, beforeActivate() -> true, beforeDeactivate(), beforeUnmount().

use crate::constants::{APP_ID, SPACE_ID};
use serde_json::{json, Map, Value};

pub const REMOTELOG: &str = "https://public-ubiservices.ubi.com/v1/profiles/me/remotelog";
const METRICS: &str = "https://public-ubiservices.ubi.com/v1/spaces/all/global/sre/opentelemetry/api/metrics";
const LOG_CATEGORIES: &str = "api focus input shell streaming webSocket gameLaunch navigation unexpected performance
shellPlugin kinderGarden apiValidation configuration serviceWorker uncategorized authentication initialization
hostCommunication microAppLifecycle navigationHistory playerNotification";

pub const POWDER_APP: &str = "powder";
pub const POWDER_SPACE: &str = "b0a5c0de-0000-4000-8000-000000000001"; // made up: the part's own space
pub const POWDER_PART_PATH: &str = "/_powder/overlay/powder.js";

/// The Powder part: plain JavaScript, no dependencies (served with CORS, as the CDN does).
pub const POWDER_PART_JS: &str = r#"System.register([], function (_export) {
  return {
    execute: function () {
      class Powder {
        constructor(shellApi, appConfig) {
          this.shellApi = shellApi; this.appConfig = appConfig; this.appName = "powder";
        }
        async mount(el) {
          el.innerHTML =
            '<div style="font-family:sans-serif;color:#fff;padding:32px;max-width:560px">' +
            '<h1 style="font-size:28px;margin:0 0 12px">Powder</h1>' +
            '<p style="font-size:16px;line-height:1.5;opacity:.85;margin:0">' +
            'You are playing on a Powder server. Your progress and achievements are stored there; ' +
            'Ubisoft\'s overlay pages (profile, friends, store) are not available.</p></div>';
          return true;
        }
        async beforeActivate() { return true; }
        async beforeDeactivate() { return { confirmationRequired: false, applicationData: null }; }
        async beforeUnmount() {}
        async apiUpdated(shellApi) { this.shellApi = shellApi; }
        static appIsAvailable() { return Promise.resolve(true); }
      }
      Powder.apiVersion = "1.1.0";
      _export("default", Powder);
    }
  };
});
"#;

pub fn powder_part(host: &str) -> Value {
    json!({"appName": POWDER_APP, "spaceId": POWDER_SPACE, "default": true,
           "bundleUrl": format!("https://{host}{POWDER_PART_PATH}"), "manifestUrl": "", "label": "Powder",
           "iconUrl": "",
           "parents": [{"parentAppName": null, "order": 0, "defaultPlacement": "main"}],
           "remoteLogsConfiguration": {"url": REMOTELOG, "maxTextLength": 32768, "uncategorized": "None"}})
}

/// The settings of one application (the overlay's parts live on `host`).
pub fn application_settings(host: &str) -> Value {
    let mut remote_logs = Map::new();
    remote_logs.insert("url".into(), json!(REMOTELOG));
    remote_logs.insert("maxTextLength".into(), json!(32768));
    for c in LOG_CATEGORIES.split_whitespace() {
        remote_logs.insert(c.into(), json!("None"));
    }
    json!({
        "microApplications": [powder_part(host)],
        "accessMenu": [],
        "lastUpdate": {"lastUpdateDate": "2016-12-01T00:00:00.000Z", "isMandatory": false},
        "manifests": [],
        "notifications": {"default": "topRight", "overrides": []},
        "serviceUrls": {"remoteLogs": REMOTELOG,
                        "metricsCollector": METRICS,
                        "websocketNotifications": "wss://public-ws-ubiservices.ubi.com/v2/websocket"},
        "feedback": {"feedbackButtonUrl": "", "ubisoftHelpButtonUrl": "", "reportBugButtonUrl": "",
                     "discordServerButtonUrl": ""},
        "remoteLogs": remote_logs,
        "cacheParameters": {"useConfigService": true},
        "features": {"metrics": false, "replaceRouterReducer": true, "replaceSwitchMicroApp": true,
                     "disableMicroAppPreload": true, "rainbow6ThemingEnabled": false,
                     "achievementNotification": true, "anonymousPreAuthenticatedLogging": 0.0},
        "metricCategoryFilters": {"p0": false, "p1": false, "p2": false, "p3": false, "p4": false,
                                  "performance": false},
    })
}

/// Where the overlay frame lives ({shellBaseUrl} is Connect's CDN address it knows itself).
pub const OVERLAY_URL: &str = concat!(
    "{shellBaseUrl}/overlay/default/?microApp={microApp}&microAppParams={microAppParams}&spaceId={spaceId}",
    "&applicationId={applicationId}&profileId={profileId}&platform={platform}&locale={locale}",
    "&ubiTicket={ubiTicket}&env={environment}&deviceType=desktop&playerSessionId={playerSessionId}",
    "&country={country}"
);

/// GET /v1/applications/{Connect's app id}/parameters: Connect's client settings. Only the overlay's address
/// (without it the overlay fails with dolphin-029); account-security settings are left out.
pub fn client_parameters() -> Value {
    json!({"parameters": {"uplayPc-clientConfigs": {"fields": {"ubisoftConnectOverlayUrlV2": OVERLAY_URL},
                                                    "relatedPopulation": null}}})
}

const UBI: &str = "https://public-ubiservices.ubi.com";
const CONNECT_SPACE: &str = "e17be87d-2996-4f3b-97c4-19bb2dae2933";
const CONNECT_APP: &str = "f68a4bb5-608a-4ff2-8123-be8ef797e0a6";
const SHELL_SPACE: &str = "0a706b37-4b88-4437-b8f4-4ed2458c9518";
const SHELL_APP: &str = "20adeb9c-6dad-404e-af1e-b12b4594e86e";

fn shell_url() -> String {
    format!(concat!(
        "https://{{env}}connect.cdn.ubisoft.com/overlay/default/bootstrap.html?env={{environment}}",
        "&isStandalone=true&platform={{platform}}&deviceType=desktop&locale={{locale}}",
        "&spaceId={}&applicationId={}&ticket={{ubiTicket}}",
        "&sessionId={{playerSessionId}}&profileId={{profileId}}&country={{country}}&region={{region}}",
        "&ownershipGroup={{ownershipGroup}}&deviceId={{deviceId}}&deviceName={{deviceName}}",
        "&isOffline={{isOffline}}"), SHELL_SPACE, SHELL_APP)
}

/// GET .../ubiconnect/configcache/api/connecturls from Connect itself (not its browser): where its services
/// and its window (shell) live. Hostnames and URL templates only. Without it, Connect waits 30 s at every
/// start before falling back to its cached copy.
pub fn connect_urls() -> Value {
    let anon = format!("{UBI}/v1/applications/{CONNECT_APP}/global/sre/opentelemetry/anon");
    json!({
        "demuxUrl": "dmx.upc.ubisoft.com",
        "shellUrl": shell_url(),
        "cdnBaseUrl": "https://static3.cdn.ubi.com",
        "shellBaseUrl": "https://{env}connect.cdn.ubisoft.com",
        "serviceLayerUrl": "https://servicelayer{env}.ecom.ubi.com",
        "storeSessionsUrl": "https://store-sessions{env}.ecom.ubi.com",
        "ubiServicesBaseUrl": "https://{env}public-ubiservices.ubi.com",
        "ubiServicesWSSBaseUrl": "wss://{env}public-ws-ubiservices.ubi.com",
        "ubiServicesNotificationBaseUrl": "https://{env}public-im-ubiservices.ubi.com",
        "eventsUrl": format!("{UBI}/v3/profiles/me/events"),
        "overlayUrl": OVERLAY_URL,
        "profilesUrl": format!("{UBI}/{{version}}/profiles"),
        "usersCreateUrl": format!("{UBI}/{{version}}/users"),
        "configsEventsUrl": format!("{UBI}/{{version}}/spaces/{{spaceId}}/configs/events"),
        "visitorEventsUrl": format!("{UBI}/v5/spaces/{CONNECT_SPACE}/serviceEvents"),
        "metricsCollectorUrl": METRICS,
        "profilesEntitiesUrl": format!("{UBI}/{{version}}/profiles/{{profileId}}/entities"),
        "crashReportUploadUrl": "",
        "allProfilesEntitiesUrl": format!("{UBI}/{{version}}/profiles/entities"),
        "remoteLogsCollectorUrl": format!("{UBI}/v1/spaces/all/global/sre/opentelemetry/api/logs"),
        "gameActivationSupportUrl": "",
        "usersValidateCreationUrl": format!("{UBI}/{{version}}/users/validatecreation"),
        "visitorMetricsCollectorUrl": format!("{anon}/metrics"),
        "steamGameActivationSupportUrl": "",
        "visitorRemoteLogsCollectorUrl": format!("{anon}/logs"),
    })
}

/// GET /v1/spaces/{id}/parameters?parameterGroups=... from Connect itself (no cached copy; with an error it
/// waits 30 s at start). Empty groups, except the address of Connect's window.
pub fn native_parameters(groups: &str) -> Value {
    let mut out = Map::new();
    if groups.split(',').any(|g| g == "connect-clientConfigs") {
        out.insert("connect-clientConfigs".into(),
                   json!({"fields": {"shellUrl": shell_url()}, "relatedPopulation": null}));
    }
    json!({"parameters": out})
}

/// Connect's card for Steep (GET .../ubiconnect/games/api): ids and name only, no texts or images. The
/// overlay waits for it (about 4 s without one) before the game logs in.
pub fn game_card(space_id: &str) -> Option<Value> {
    if space_id != SPACE_ID {
        return None;
    }
    let nulls = |keys: &[&str]| -> Map<String, Value> { keys.iter().map(|k| (k.to_string(), Value::Null)).collect() };
    Some(json!({
        "spaceId": SPACE_ID, "installmentSpaceId": SPACE_ID, "displayName": "Steep", "displayDescription": "",
        "platforms": [{"type": "PC", "displayName": "PC", "applicationId": APP_ID}], "siblingGames": [],
        "installmentDefault": true, "ageRating": null, "releaseAt": null, "releaseStatusDisplayName": null,
        "imageUrls": nulls(&["lowThumbnail", "highThumbnail", "background", "lowBoxArt", "highBoxArt",
                             "gameLogoFlat", "gameLogoColor", "gameAvatar"]),
        "linkUrls": nulls(&["facebook", "twitter", "instagram", "reddit", "website"]),
    }))
}

/// The cards of the spaces asked for (`spaceIds=` a comma list); None when none is Steep's.
pub fn game_cards(space_ids: &str) -> Option<Value> {
    let cards: Vec<Value> = space_ids.split(',').filter_map(game_card).collect();
    (!cards.is_empty()).then(|| json!({"games": cards}))
}

/// {application id: settings} for the applications asked for (`applications=` a comma list).
pub fn postauth(applications: &str) -> Option<Value> {
    let apps: Map<String, Value> = applications
        .split(',')
        .filter(|a| !a.is_empty())
        .map(|a| (a.to_string(), application_settings("public-ubiservices.ubi.com")))
        .collect();
    (!apps.is_empty()).then(|| Value::Object(apps))
}
