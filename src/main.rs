mod utils;
use rustls::ClientConfig;
use utils::{
    Tracking, UniversalAdId,
    base_url, build_forward_url, calculate_expected_program_date_time_list, copy_headers,
    find_program_datetime_tag, get_all_raw_creatives_from_vast,
    get_all_transcoded_creatives_from_vast, get_duration_and_media_urls_and_tracking_events_from_linear,
    get_header_value, get_universal_ad_ids_from_creative, get_query_param, is_media_segment, is_hls_playlist,
    is_fragmented_mp4_vod_media_playlist, make_program_date_time_tag, rustls_config,
};

use actix_web::{error, middleware, web, App, Error, HttpRequest, HttpResponse, HttpServer};
use awc::{http::header, Client, Connector};
use clap::{Parser, ValueEnum};
use dashmap::{DashMap, DashSet};
use hls_m3u8::tags::{ExtXDateRange, VariantStream};
use hls_m3u8::types::Value;
use hls_m3u8::{MasterPlaylist, MediaPlaylist, MediaSegment};
use json::object;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;
use url::Url;
use uuid::Uuid;

const STATUS_PREFIX: &str = "/status";
const COMMAND_PREFIX: &str = "/command";
const INTERSTITIAL_PLAYLIST: &str = "interstitials.m3u8";

const SESSION_ID_TEMPLATE: &str = "[template.sessionId]";
const DURATION_TEMPLATE: &str = "[template.duration]";
const POD_NUM_TEMPLATE: &str = "[template.pod]";

const HLS_PLAYLIST_CONTENT_TYPE: &str = "application/vnd.apple.mpegurl";
const HLS_INTERSTITIAL_ID: &str = "_HLS_interstitial_id";
const HLS_PRIMARY_ID: &str = "_HLS_primary_id";
const AD_ID: &str = "_ad_id";

const APPLICATION_XML: &str = "application/xml";

// Get the start time of the program as a static DateTime
lazy_static::lazy_static! {
    static ref START_TIME: chrono::DateTime<chrono::Local> = chrono::offset::Local::now();
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RequestType {
    MasterPlayList,
    MediaPlayList,
    Playlist, // Unknown playlist type (origin host mode)
    Segment,
    Other,
}

#[derive(Clone, Debug)]
struct TestAsset {
    url: Url,
    duration: u64,
}

impl TestAsset {
    fn new(url: Url, duration: u64) -> Self {
        Self { url, duration }
    }
    
    fn to_json(&self) -> json::JsonValue {
        object! {
            "url": self.url.as_str(),
            "duration": self.duration,
        }
    }
}

#[derive(Clone, Default)]
struct Ad {
    ad_id: Uuid,
    universal_ad_ids: Vec<UniversalAdId>,
    duration: u64,
    url: String,
    requested_at: chrono::DateTime<chrono::Local>,
    tracking: Vec<Tracking>,
}

#[derive(Clone, Default)]
struct AvailableAds {
    linears: Arc<DashMap<Uuid, Ad>>,
}

impl AvailableAds {
    fn to_json(&self) -> json::JsonValue {
        let linears = self
            .linears
            .iter()
            .map(|entry| {
                let (id, ad) = entry.pair();
                object! {
                    "id": id.to_string(),
                    "duration": ad.duration,
                    "url": ad.url.clone(),
                    "requested_at": ad.requested_at.to_rfc3339(),
                }
            })
            .collect::<Vec<_>>();

        object! {
            "count": linears.len(),
            "linears": linears,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct AdSlot {
    id: Uuid,
    index: u64,
    start_time: chrono::DateTime<chrono::Local>,
    duration: u64,
    pod_num: u64,
}

impl AdSlot {
    fn name(&self) -> String {
        format!("ad_slot{}", self.index)
    }

    /// End of the break window: the slot stays live for the whole `duration`
    /// after its `start_time` so that every concurrent viewer polling within
    /// the break gets the ad, not just the first one.
    fn expires_at(&self) -> chrono::DateTime<chrono::Local> {
        self.start_time + chrono::Duration::seconds(self.duration as i64)
    }
}

#[derive(Clone, Default)]
struct AvailableAdSlots(Arc<DashSet<AdSlot>>);

impl AvailableAdSlots {
    fn to_json(&self) -> json::JsonValue {
        let slots = self
            .0
            .iter()
            .map(|slot| {
                object! {
                    "id": slot.id.to_string(),
                    "index": slot.index,
                    "start_time": slot.start_time.to_rfc3339(),
                    "duration": slot.duration,
                    "pod_num": slot.pod_num,
                }
            })
            .collect::<Vec<_>>();

        object! {
            "count": slots.len(),
            "slots": slots,
        }
    }
}

#[derive(Clone, Default)]
struct UserDefinedQueryParams(Arc<DashMap<Uuid, String>>);

impl UserDefinedQueryParams {
    fn to_json(&self) -> json::JsonValue {
        let params = self
            .0
            .iter()
            .map(|entry| {
                let (id, query) = entry.pair();
                object! {
                    "id": id.to_string(),
                    "query": query.clone(),
                }
            })
            .collect::<Vec<_>>();

        object! {
            "params": params,
        }
    }
}

#[derive(clap::Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct CliArguments {
    /// Proxy address (ip)
    listen_addr: String,
    /// Proxy port
    listen_port: u16,

    /// Ad server endpoint (protocol://ip:port/path)
    /// It should be a VAST4.0/4.1 XML compatible endpoint
    /// Not required when --test-asset-url is set
    #[clap(required_unless_present = "test_asset_url", verbatim_doc_comment)]
    ad_server_endpoint: Option<String>,

    /// HLS stream address (protocol://ip:port/path)
    /// (e.g., http://localhost/test/master.m3u8)
    /// Required unless --origin-host is provided
    #[clap(required_unless_present = "origin_host", verbatim_doc_comment)]
    master_playlist_url: Option<String>,

    /// Origin host URL (protocol://host:port) to proxy any stream from
    /// Use this instead of master_playlist_url to proxy multiple streams
    #[clap(long, verbatim_doc_comment)]
    origin_host: Option<String>,

    /// Ad insertion mode to use:
    /// 1) static  - add interstitial every 30 seconds (1000 in total).
    /// 2) dynamic - add interstitial when requested (Live Content only).
    #[clap(short, long, value_enum, verbatim_doc_comment, default_value_t = InsertionMode::Static)]
    ad_insertion_mode: InsertionMode,

    /// Base URL for interstitials (protocol://ip:port)
    /// If not provided, the server will use 'localhost' and the 'listen port' as the base URL
    /// e.g., http://localhost:${LISTEN_PORT}
    #[clap(short, long, verbatim_doc_comment, default_value_t = String::from(""))]
    interstitials_address: String,

    /// Default ad break duration in seconds
    #[clap(long, env, verbatim_doc_comment, default_value_t = String::from(""))]
    default_ad_duration: String,

    /// Repeat the ad break every 'n' seconds
    #[clap(long, env, verbatim_doc_comment, default_value_t = String::from(""))]
    default_repeating_cycle: String,

    /// Default number of ad slots to generate
    #[clap(long, env, verbatim_doc_comment, default_value_t = String::from(""))]
    default_ad_number: String,

    /// Replace raw MP4 assets with this test assets (it has to be a fragmented MP4 VoD **MEDIA** playlist)
    /// e.g., https://eyevinnlab-adtracking.minio-minio.auto.prod.osaas.io/tutorial/index.m3u8
    #[clap(long, env, verbatim_doc_comment, default_value_t = String::from(""))]
    test_asset_url: String,

    /// Seconds after the interstitial start before the skip button appears (0 = immediate).
    /// Must be less than the interstitial duration or no button is shown.
    /// When set, X-RESTRICT switches from "SKIP,JUMP" to "JUMP" and skip-control attributes are emitted.
    #[clap(long, env, verbatim_doc_comment, default_value_t = String::from(""))]
    skip_control_offset: String,

    /// How long (seconds) the skip button remains visible; absent means the whole interstitial.
    /// Must be >= 1 if provided.
    #[clap(long, env, verbatim_doc_comment, default_value_t = String::from(""))]
    skip_control_duration: String,

    /// Localisation key for the skip-button label (ASCII letters, hyphens, underscores only).
    /// e.g., "skip_ad"
    #[clap(long, env, verbatim_doc_comment, default_value_t = String::from(""))]
    skip_control_label_id: String,
}

#[derive(ValueEnum, Clone, Debug, PartialEq)]
pub enum InsertionMode {
    Static,
    Dynamic,
}

impl InsertionMode {
    pub fn to_str(&self) -> &str {
        match self {
            InsertionMode::Static => "static",
            InsertionMode::Dynamic => "dynamic",
        }
    }
}

#[derive(Debug, Clone)]
struct ServerConfig {
    forward_url: Url,
    interstitials_address: Url,
    master_playlist_path: Option<String>,
    insertion_mode: InsertionMode,
    target_ad_duration: u64,
    target_repeating_cycle: u64,
    target_ad_number: u64,
    test_asset: Option<TestAsset>,
    skip_control_offset: Option<u64>,
    skip_control_duration: Option<u64>,
    skip_control_label_id: Option<String>,
}

impl ServerConfig {
    fn new(
        forward_url: Url,
        interstitials_address: Url,
        master_playlist_path: Option<String>,
        insertion_mode: InsertionMode,
        target_ad_duration: u64,
        target_repeating_cycle: u64,
        target_ad_number: u64,
        test_asset: Option<TestAsset>,
        skip_control_offset: Option<u64>,
        skip_control_duration: Option<u64>,
        skip_control_label_id: Option<String>,
    ) -> Self {
        Self {
            forward_url,
            interstitials_address,
            master_playlist_path,
            insertion_mode,
            target_ad_duration,
            target_repeating_cycle,
            target_ad_number,
            test_asset,
            skip_control_offset,
            skip_control_duration,
            skip_control_label_id,
        }
    }

    fn to_json(&self) -> json::JsonValue {
        object! {
            "forward_url": self.forward_url.as_str(),
            "interstitials_address": self.interstitials_address.as_str(),
            "master_playlist_path": self.master_playlist_path.clone().unwrap_or_default(),
            "insertion_mode": self.insertion_mode.to_str(),
            "target_ad_duration": self.target_ad_duration,
            "target_repeating_cycle": self.target_repeating_cycle,
            "target_ad_number": self.target_ad_number,
            "test_asset": self.test_asset.as_ref().map(|asset| asset.to_json()).unwrap_or_else(|| object! {}),
            "skip_control_offset": self.skip_control_offset.map(|v| json::JsonValue::from(v)).unwrap_or(json::JsonValue::Null),
            "skip_control_duration": self.skip_control_duration.map(|v| json::JsonValue::from(v)).unwrap_or(json::JsonValue::Null),
            "skip_control_label_id": self.skip_control_label_id.as_deref().map(json::JsonValue::from).unwrap_or(json::JsonValue::Null),
        }
    }
}

#[derive(Debug, Clone)]
struct InsertionCommand {
    in_sec: u64,
    duration: u64,
    pod_num: u64,
}

impl InsertionCommand {
    fn from_query(query: &str) -> Result<Self, String> {
        let mut in_sec = None;
        let mut duration = None;
        let mut pod_num = None;

        for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
            match key.as_ref() {
                "in" => in_sec = value.parse().ok(),
                "dur" => duration = value.parse().ok(),
                "pod" => pod_num = value.parse().ok(),
                _ => {}
            }
        }

        match (in_sec, duration, pod_num) {
            (Some(in_sec), Some(duration), Some(pod_num)) => Ok(Self {
                in_sec,
                duration,
                pod_num,
            }),
            _ => Err("Missing required query parameters".to_string()),
        }
    }
}

fn get_request_type(req: &HttpRequest, config: &web::Data<ServerConfig>) -> RequestType {
    let path = req.uri().path();
    let semi_idx = path.find(';').or_else(|| path.find("%3B")).or_else(|| path.find("%3b"));
    let path_base = semi_idx.map(|i| &path[..i]).unwrap_or(path);

    // In specific playlist mode, check for master playlist path
    if let Some(ref master_path) = config.master_playlist_path {
        if path_base.contains(master_path.as_str()) {
            return RequestType::MasterPlayList;
        }
    }

    if is_media_segment(path_base) {
        return RequestType::Segment;
    } else if path_base.ends_with(".m3u8") {
        // In origin host mode (master_playlist_path is None), return generic Playlist
        if config.master_playlist_path.is_none() {
            return RequestType::Playlist;
        }
        return RequestType::MediaPlayList;
    }
    RequestType::Other
}

async fn build_ad_server_url(
    ad_server_url: &Url,
    interstitial_id: &str,
    user_id: &str,
    available_slots: &web::Data<AvailableAdSlots>,
    user_defined_query_params: &web::Data<UserDefinedQueryParams>,
) -> Result<Url, Error> {
    let slot = available_slots
        .0
        .iter()
        .find(|slot| slot.name() == interstitial_id)
        .ok_or_else(|| error::ErrorNotFound("Ad slot missing".to_string()))?;

    // Create a map of query templates to replace in the ad_server_url
    let duration_str = slot.duration.to_string();
    let pod_num_str = slot.pod_num.to_string();
    let query_templates: HashMap<&str, &str> = [
        (SESSION_ID_TEMPLATE, user_id),
        (DURATION_TEMPLATE, &duration_str),
        (POD_NUM_TEMPLATE, &pod_num_str),
    ]
    .iter()
    .cloned()
    .collect();

    if query_templates.is_empty() {
        log::warn!("No query templates found for ad server URL. Missing [duration] ...");
    }

    // Extract and transform query parameters from the ad_server_url
    let transformed_queries: String = ad_server_url
        .query_pairs()
        .map(|(key, value)| {
            // Check if the value matches any template in query_templates
            let new_value = if let Some(&matched_value) = query_templates.get(value.as_ref()) {
                // Use the matched value if a template is found
                matched_value.to_string()
            } else {
                // Otherwise, use the original value
                value.into_owned()
            };

            format!("{}={}", key, new_value)
        })
        .collect::<Vec<_>>()
        .join("&");

    // AVPlayer and Safari support setting the 'X-PLAYBACK-SESSION-ID' request
    // header with a common, globally-unique value on every HTTP request
    // associated with a particular playback session, which matches the
    // _HLS_primary_id query parameter of interstitial requests.
    let user_defined_queries = Uuid::parse_str(user_id)
        .ok()
        .and_then(|uuid| user_defined_query_params.0.get(&uuid));

    let full_queries = if let Some(user_defined_queries) = user_defined_queries {
        format!("{}&{}", transformed_queries, user_defined_queries.as_str())
    } else {
        transformed_queries
    };

    // Clone the original URL and set the new query string
    let mut updated_ad_server_url = ad_server_url.clone();
    updated_ad_server_url.set_query(Some(&full_queries));

    Ok(updated_ad_server_url)
}

fn make_new_ad_from_creative(creative: &vast4_rs::Creative) -> Ad {
    let universal_ad_ids = get_universal_ad_ids_from_creative(creative);
    let linear = creative.linear.as_ref().unwrap();
    let (duration, urls, trackings) = get_duration_and_media_urls_and_tracking_events_from_linear(linear);
    let url = urls.first().unwrap().clone();
    let ad_id = Uuid::new_v4();

    Ad {
        ad_id,
        universal_ad_ids,
        duration: duration as u64,
        url,
        requested_at: chrono::Local::now(),
        tracking: trackings,
    }
}

fn make_test_ad_from_creative(creative: &vast4_rs::Creative, test_asset: &TestAsset) -> Ad {
    let mut ad = make_new_ad_from_creative(creative);
    ad.url = test_asset.url.as_str().to_string();
    ad.duration = test_asset.duration;

    // Replace the http with https in urls
    ad.tracking.iter_mut().for_each(|tracking| {
        tracking.urls.iter_mut().for_each(|url| {
            if url.starts_with("http://") {
                *url = url.replace("http://", "https://");
            }
        });
    });

    ad
}

fn to_tracking_json(tracking: &Tracking) -> json::JsonValue {
    if tracking.offset.is_none() {
        object! {
            "type": tracking.event.clone(),
            "urls": tracking.urls.clone(),
        }
    } else {
        object! {
            "type": tracking.event.clone(),
            "offset": tracking.offset.as_ref().unwrap().as_str(),
            "urls": tracking.urls.clone(),
        }
    }

}

fn to_ad_asset_json(url: &str, ad: &Ad, start: u64) -> json::JsonValue {
    object! {
        "URI": url,
        "DURATION": ad.duration,
        "X-AD-CREATIVE-SIGNALING": object! {
            "version": 2,
            "type": "slot",
            "payload": object! {
                "type": "linear",
                "start": start,
                "duration": ad.duration,
                "identifiers": ad.universal_ad_ids.iter().map(|id| {
                    object! {
                        "scheme": id.scheme.as_str(),
                        "value": id.value.as_str(),
                    }
                }).collect::<Vec<_>>(),
                "tracking": ad.tracking.iter().map(to_tracking_json).collect::<Vec<_>>(),
            },
        },
    }
}

fn to_asset_list_json_string(assets: Vec<json::JsonValue>, duration: u64) -> String {
    object! {
        "ASSETS": assets,
        "X-AD-CREATIVE-SIGNALING": object! {
            "version": 2,
            "type": "pod",
            "payload": object! {
                "duration": duration,
            },
        },
    }
    .pretty(2)
}

fn wrap_into_assets(
    vast: vast4_rs::Vast,
    req_url: Url,
    interstitial_id: &str,
    user_id: &str,
    test_asset: &Option<TestAsset>,
    available_ads: web::Data<AvailableAds>,
) -> String {
    let mut start_offset: u64 = 0;
    // Get all linears (regular MP4s) from the VAST
    let raw_assets = get_all_raw_creatives_from_vast(&vast)
        .iter()
        .map(|creative| {
            let asset = if test_asset.is_some() {
                let ad = make_test_ad_from_creative(creative, &test_asset.as_ref().unwrap());
                
                start_offset += ad.duration;
                to_ad_asset_json(&ad.url, &ad, start_offset)
            } else {
                let ad = make_new_ad_from_creative(creative);
                let id = ad.ad_id;
                log::info!("Processing raw asset {id}, tracking: {:?}", ad.tracking);

                // Save the asset for follow-up requests (this applies to not-transcoded ads)
                available_ads.linears.insert(id, ad.clone());

                let mut url = req_url.clone();
                url.query_pairs_mut()
                    .clear()
                    .append_pair(HLS_INTERSTITIAL_ID, interstitial_id)
                    .append_pair(HLS_PRIMARY_ID, user_id)
                    .append_pair(AD_ID, &id.to_string());

                start_offset += ad.duration;
                to_ad_asset_json(&url.as_str(), &ad, start_offset)
            };

            asset
        })
        .collect::<Vec<_>>();

    let transcoded_assets = get_all_transcoded_creatives_from_vast(&vast)
        .iter()
        .map(|creative| {
            let ad = make_new_ad_from_creative(creative);
            let id = ad.ad_id;
            log::info!("Processing transcoded asset {id}, tracking: {:?}", ad.tracking);

            let asset = to_ad_asset_json(&ad.url, &ad, start_offset);
            start_offset += ad.duration;

            asset
        })
        .collect::<Vec<_>>();

    let assets = raw_assets
        .into_iter()
        .chain(transcoded_assets.into_iter())
        .collect::<Vec<_>>();

    to_asset_list_json_string(assets, start_offset)
}

fn replace_absolute_url_with_relative_url(m3u8: &mut MasterPlaylist) {
    m3u8.variant_streams.iter_mut().for_each(|variant| {
        // Skip iframe playlists

        if let VariantStream::ExtXStreamInf { uri, .. } = variant {
            if !uri.starts_with("http") {
                // Relative URIs
                return;
            }

            // Replace the absolute URI by their relative path
            let absolute_media_playlist_url = Url::parse(&uri).expect("Invalid media playlist URI");
            let mut relative_url = absolute_media_playlist_url.path().to_string();
            if let Some(query) = absolute_media_playlist_url.query() {
                relative_url.push('?');
                relative_url.push_str(query);
            }

            *uri = relative_url.into();
        }
    });
}

fn generate_static_ad_slots(ad_duration:u64, every:u64, number: u64, date_time: chrono::DateTime<chrono::Local>) -> Vec<AdSlot> {
    (1..number)
        .map(|i| {
            let seconds = i * every;
            let start_time = date_time + chrono::Duration::seconds(seconds as i64);
            AdSlot {
                id: Uuid::new_v4(),
                index: i as u64,
                start_time: start_time,
                duration: ad_duration,
                pod_num: 2,
            }
        })
        .collect()
}

/// Resolved skip-control attributes for one interstitial slot.
/// All fields correspond directly to HLS X-SKIP-CONTROL-* attributes.
#[derive(Debug, Clone, PartialEq)]
struct SkipControlAttrs {
    /// X-SKIP-CONTROL-OFFSET (unquoted decimal-integer, seconds)
    offset: u64,
    /// X-SKIP-CONTROL-DURATION (unquoted decimal-integer, seconds); None = omit attribute
    duration: Option<u64>,
    /// X-SKIP-CONTROL-LABEL-ID (quoted string); None = omit attribute
    label_id: Option<String>,
}

/// Validate the LABEL-ID value: ASCII letters, hyphens, underscores, non-empty.
fn is_valid_label_id(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphabetic() || c == '-' || c == '_')
}

/// Resolve whether skip-control should be emitted for a slot with the given duration.
/// Returns `Some(SkipControlAttrs)` when skip-control applies, `None` when it does not.
/// Validation warnings are emitted here so the call site stays clean.
fn resolve_skip_control(cfg: &ServerConfig, slot_duration: f32) -> Option<SkipControlAttrs> {
    let offset = cfg.skip_control_offset?;
    // OFFSET must be < slot_duration; if not, the button would never appear.
    if offset as f32 >= slot_duration {
        log::warn!(
            "skip-control offset ({offset}s) >= slot duration ({slot_duration}s); \
             button would never appear — skip-control suppressed for this slot"
        );
        return None;
    }
    let duration = cfg.skip_control_duration; // already validated (>=1) at startup
    let label_id = cfg.skip_control_label_id.as_deref().and_then(|id| {
        if is_valid_label_id(id) {
            Some(id.to_owned())
        } else {
            log::warn!("skip-control label-id {id:?} contains invalid characters; label suppressed");
            None
        }
    });
    Some(SkipControlAttrs { offset, duration, label_id })
}

fn insert_interstitials(
    m3u8: &mut MediaPlaylist,
    config: &web::Data<ServerConfig>,
    available_slots: web::Data<AvailableAdSlots>,
) {
    let interstitials_address = &config.interstitials_address;
    let ad_insert_mode = &config.insertion_mode;

    let mut first_program_date_time = find_program_datetime_tag(&m3u8);
    let segments = &mut m3u8.segments;

    let is_vod = m3u8
        .playlist_type
        .is_some_and(|t| t == hls_m3u8::types::PlaylistType::Vod);
    let is_static = *ad_insert_mode == InsertionMode::Static;
    if is_vod && !is_static {
        log::error!("Dynamic ad insertion is not supported for VOD streams.");
        return;
    }

    if first_program_date_time.is_none() {
        // Synthesize PDT so the live edge tracks wall clock time.
        // Use now() minus total window duration so the last segment's PDT ≈ now().
        let window_ms: i64 = segments
            .iter()
            .map(|(_, s)| s.duration.duration().as_millis() as i64)
            .sum();
        let synthetic_start = chrono::Local::now() - chrono::Duration::milliseconds(window_ms);
        log::warn!("No program_date_time found in the media playlist. Synthesizing from now() - window ({window_ms}ms).");

        segments.find_first_mut().and_then(|first_segment| {
            first_segment.program_date_time = Some(make_program_date_time_tag(&synthetic_start));
            first_program_date_time = Some(synthetic_start);
            Some(first_segment)
        });
    }

    // By this point, we should have a valid program_date_time
    let first_program_date_time = first_program_date_time.expect("Missing program_date_time Tag");
    // Find the available ad slots
    let ad_slots: Vec<AdSlot> = if is_static {
        // Find a reference date time for the ad slots
        let ad_slots_start_date_time = if is_vod {
            // Use the first program_date_time for VoD streams
            first_program_date_time
        } else {
            // Use the server start time for Live streams
            *START_TIME
        };

        // Generate ad slots
        let ad_duration = config.target_ad_duration;
        let ad_every = config.target_repeating_cycle;
        let ad_num = config.target_ad_number;
        let fixed_ad_slots: Vec<AdSlot> = generate_static_ad_slots(ad_duration, ad_every, ad_num, ad_slots_start_date_time);

        // Save fixed ad slots to available slots
        if available_slots.0.is_empty() {
            for slot in &fixed_ad_slots {
                available_slots.0.insert(slot.clone());
            }
            log::debug!("Saved fixed ad slots for VOD or static mode.");
        }

        fixed_ad_slots
    } else {
        // Retrieve the available ad slots for dynamic mode
        available_slots.0.iter().map(|slot| slot.clone()).collect()
    };
    log::trace!("Available slots: {:?}", ad_slots);

    // Find the date time tag for each segment
    // Or calculate the expected date time based on the previous segments
    let expected_program_date_time_list =
        calculate_expected_program_date_time_list(segments, first_program_date_time);

    // Evict slots only once their whole break window has scrolled past the
    // window start. Retaining until `expires_at` (start_time + duration) keeps
    // the break available to every concurrent viewer for the full `dur`,
    // instead of dropping it the moment the first request scrolls the slot's
    // start out of the DVR window.
    if let Some((window_start, _)) = expected_program_date_time_list.first() {
        available_slots
            .0
            .retain(|slot| slot.expires_at() >= *window_start);
    }
    for (index, (program_date_time, duration)) in expected_program_date_time_list.iter().enumerate()
    {
        log::trace!(
            "Segment {index} starts at {program_date_time} and lasts for {:?}",
            duration
        );

        // If a segment has a discontinuity tag but no program_date_time, insert one
        let seg = segments.get_mut(index).unwrap();
        if seg.has_discontinuity && seg.program_date_time.is_none() {
            let program_date_time_tag = make_program_date_time_tag(program_date_time);
            seg.program_date_time = Some(program_date_time_tag);
        }
    }

    // Match the ad slots with the segments.
    // Track which slot UUIDs have already been matched to prevent duplicate DATERANGEs
    // when channel-engine discontinuities cause multiple segments to share the same PDT range.
    let mut matched_slot_ids: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
    let interstitials: Vec<_> = expected_program_date_time_list
        .iter()
        .enumerate()
        .filter_map(|(index, (program_date_time, duration))| {
            // Match the segment with the first possible ad slot
            ad_slots.iter().find_map(|ad_slot| {
                if matched_slot_ids.contains(&ad_slot.id) {
                    return None;
                }
                let expected_date_time = ad_slot.start_time;
                let next_program_date_time = expected_date_time + *duration;
                // Place the DATERANGE on the first segment whose PDT >= slot start,
                // so it appears after the preceding segment in the manifest.
                if program_date_time >= &expected_date_time
                    && program_date_time < &next_program_date_time
                {
                    log::debug!("Insert interstitial at time: {expected_date_time}");

                    let ad_slot_name = ad_slot.name();
                    let url = format!(
                        "{interstitials_address}{INTERSTITIAL_PLAYLIST}?{HLS_INTERSTITIAL_ID}={ad_slot_name}"
                    );
                    let slot_duration = ad_slot.duration as f32;
                    
                    let skip_ctrl = resolve_skip_control(config, slot_duration);

                    let mut date_range = ExtXDateRange::builder();
                    date_range
                        .id(ad_slot_name)
                        .class("com.apple.hls.interstitial")
                        .start_date(
                            expected_date_time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                        )
                        .duration(Duration::from_secs_f32(slot_duration))
                        .insert_client_attribute("X-ASSET-LIST", Value::String(url.into()))
                        .insert_client_attribute("X-SNAP", Value::String(if is_vod { "IN,OUT" } else { "IN" }.into()));
                    // When skip-control is active, drop "SKIP" from X-RESTRICT (contradictory).
                    // Keep JUMP in both cases.
                    if skip_ctrl.is_some() {
                        date_range.insert_client_attribute(
                            "X-RESTRICT",
                            Value::String("JUMP".into()),
                        );
                    } else {
                        date_range.insert_client_attribute(
                            "X-RESTRICT",
                            Value::String("SKIP,JUMP".into()),
                        );
                    }
                    if is_vod {
                        date_range.insert_client_attribute(
                            "X-RESUME-OFFSET",
                            Value::Float(hls_m3u8::types::Float::new(0.0)),
                        );
                    } else {
                        // For live streams, advance the primary by the ad duration so the player
                        // resumes near the live edge instead of going into a seek loop.
                        date_range.insert_client_attribute(
                            "X-RESUME-OFFSET",
                            Value::Float(hls_m3u8::types::Float::new(slot_duration)),
                        );
                    }
                    // Emit X-SKIP-CONTROL-* attributes when skip-control is configured and valid.
                    if let Some(sc) = skip_ctrl {
                        date_range.insert_client_attribute(
                            "X-SKIP-CONTROL-OFFSET",
                            Value::Float(hls_m3u8::types::Float::new(sc.offset as f32)),
                        );
                        if let Some(dur) = sc.duration {
                            date_range.insert_client_attribute(
                                "X-SKIP-CONTROL-DURATION",
                                Value::Float(hls_m3u8::types::Float::new(dur as f32)),
                            );
                        }
                        if let Some(label) = sc.label_id {
                            date_range.insert_client_attribute(
                                "X-SKIP-CONTROL-LABEL-ID",
                                Value::String(label.into()),
                            );
                        }
                    }
                    let date_range = date_range
                        .build()
                        .unwrap();

                    matched_slot_ids.insert(ad_slot.id);
                    Some((index, Some(date_range)))
                } else {
                    None
                }
            })
        })
        .collect();

    // Insert the interstitials into the segments
    for (index, date_range) in interstitials {
        if let Some(date_range) = date_range {
            segments.get_mut(index).unwrap().date_range = Some(date_range);
        }
    }

}

/// Build the companion `com.apple.hls.preload` DATERANGE for an injected
/// interstitial DATERANGE, per Apple HLS Interstitials preload hinting.
///
/// The preload DATERANGE carries its own unique `ID` (target ID + "-preload"),
/// a `START-DATE` equal to the target's START-DATE, and the three preload
/// attributes `X-URI` (the resource to preload — the interstitial's
/// `X-ASSET-URI` or `X-ASSET-LIST`), `X-TARGET-ID` (the target's ID) and
/// `X-TARGET-CLASS` (the target's CLASS). Returns `None` if the target lacks
/// the fields a legal preload DATERANGE requires (ID, START-DATE, a target
/// CLASS, and an asset URI/list to preload).
fn build_preload_date_range<'a>(target: &ExtXDateRange<'a>) -> Option<ExtXDateRange<'static>> {
    // The resource to preload: prefer X-ASSET-URI, fall back to X-ASSET-LIST.
    let x_uri = target
        .client_attributes
        .get("X-ASSET-URI")
        .or_else(|| target.client_attributes.get("X-ASSET-LIST"))
        .and_then(|v| match v {
            Value::String(s) => Some(s.to_string()),
            _ => None,
        })?;

    // START-DATE is mandatory for a legal preload DATERANGE.
    let start_date = target.start_date().as_ref()?.to_string();
    // The target's CLASS is required so the player can resolve the target kind.
    let target_class = target.class().as_ref()?.to_string();
    let target_id = target.id().to_string();
    // A preload DATERANGE MUST carry a DURATION or END-DATE (draft-pantos
    // Appendix F). Injected interstitials always carry DURATION; mirror it onto
    // the preload. If the target somehow lacks one, refuse to emit an illegal tag.
    let duration = target.duration?;

    let preload_id = format!("{target_id}-preload");

    let mut builder = ExtXDateRange::builder();
    builder
        .id(preload_id)
        .class("com.apple.hls.preload")
        .start_date(start_date)
        .duration(duration)
        .insert_client_attribute("X-URI", Value::String(x_uri.into()))
        .insert_client_attribute("X-TARGET-ID", Value::String(target_id.into()))
        .insert_client_attribute("X-TARGET-CLASS", Value::String(target_class.into()));

    builder.build().ok().map(|dr| dr.into_owned())
}

/// Emit a `com.apple.hls.preload` companion DATERANGE line ahead of every
/// injected interstitial DATERANGE line in a serialized media playlist.
///
/// This works on the rendered manifest string because a single `MediaSegment`
/// can hold only one DATERANGE; the preload is a second, distinct DATERANGE
/// that must sit alongside the interstitial it points at.
fn inject_preload_date_ranges(playlist_str: &str) -> String {
    let mut out = String::with_capacity(playlist_str.len() + 128);
    for line in playlist_str.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("#EXT-X-DATERANGE:")
            && trimmed.contains("CLASS=\"com.apple.hls.interstitial\"")
        {
            if let Ok(target) = ExtXDateRange::try_from(trimmed) {
                if let Some(preload) = build_preload_date_range(&target) {
                    out.push_str(&preload.to_string());
                    out.push('\n');
                }
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

// Extract the live edge PDT from a media playlist and store it in the shared cache.
fn update_last_seen_pdt(playlist: &MediaPlaylist, last_seen_pdt: &AtomicI64) {
    if let Some(seed) = find_program_datetime_tag(playlist) {
        let pdts = calculate_expected_program_date_time_list(&playlist.segments, seed);
        if let Some((last_pdt, last_dur)) = pdts.last() {
            let live_edge = *last_pdt + chrono::Duration::from_std(*last_dur).unwrap_or_default();
            last_seen_pdt.store(live_edge.timestamp_millis(), Ordering::Relaxed);
        }
    }
}

// Returns the current live edge PDT for ad slot scheduling.
// Always fetches a fresh media playlist from origin; falls back to cached PDT if that fails.
async fn fetch_stream_now(config: &ServerConfig, client: &Client, last_seen_pdt: &AtomicI64) -> chrono::DateTime<chrono::Local> {
    // Always fetch a fresh media playlist from origin to get the current live edge PDT.
    // The cached value is stale if the player hasn't polled recently, causing slots to be
    // scheduled in the past relative to the live edge.
    if let Some(media_url) = resolve_media_playlist_url(config, client).await {
        log::debug!("Fetching live edge PDT from origin: {media_url}");
        if let Ok(mut res) = client.get(media_url.as_str()).send().await {
            if let Ok(payload) = res.body().await {
                if let Ok(text) = std::str::from_utf8(&payload) {
                    if let Ok(playlist) = MediaPlaylist::try_from(text) {
                        update_last_seen_pdt(&playlist, last_seen_pdt);
                        let ts = last_seen_pdt.load(Ordering::Relaxed);
                        if let Some(dt) = chrono::DateTime::from_timestamp_millis(ts) {
                            log::info!("Live edge PDT from origin: {}", dt.with_timezone(&chrono::Local));
                            return dt.with_timezone(&chrono::Local);
                        }
                    }
                }
            }
        }
    }

    // Fall back to cached PDT if origin fetch failed
    let ts = last_seen_pdt.load(Ordering::Relaxed);
    if ts != 0 {
        if let Some(dt) = chrono::DateTime::from_timestamp_millis(ts) {
            let local_dt = dt.with_timezone(&chrono::Local);
            log::warn!("Origin fetch failed; using cached stream PDT: {local_dt}");
            return local_dt;
        }
    }

    log::warn!("Could not determine stream PDT; falling back to wall clock");
    chrono::Local::now()
}

// Resolves a usable media playlist URL from the configured origin.
// For master-playlist mode: fetches the master, picks the first variant stream.
// For origin-host mode: returns None (no known playlist path).
async fn resolve_media_playlist_url(config: &ServerConfig, client: &Client) -> Option<url::Url> {
    let master_path = config.master_playlist_path.as_ref().filter(|p| !p.is_empty())?;
    let master_url = config.forward_url.join(master_path).ok()?;

    let mut res = client.get(master_url.as_str()).send().await.ok()?;
    let payload = res.body().await.ok()?;
    let text = std::str::from_utf8(&payload).ok()?;

    // Try to parse as a master playlist and pick the first variant
    if let Ok(master) = MasterPlaylist::try_from(text) {
        if let Some(variant) = master.variant_streams.iter().next() {
            if let VariantStream::ExtXStreamInf { uri, .. } = variant {
                return master_url.join(uri).ok();
            }
        }
    }

    // Already a media playlist (single-rendition stream) — use it directly
    if MediaPlaylist::try_from(text).is_ok() {
        return Some(master_url);
    }

    None
}

// Take http get requests and parse the query string into commands
async fn handle_commands(
    req: HttpRequest,
    config: web::Data<ServerConfig>,
    available_slots: web::Data<AvailableAdSlots>,
    client: web::Data<Client>,
    last_seen_pdt: web::Data<AtomicI64>,
    slot_counter: web::Data<AtomicU64>,
) -> Result<HttpResponse, Error> {
    if config.insertion_mode == InsertionMode::Static {
        return Ok(HttpResponse::BadRequest().body("Ad insertion is not supported in static mode."));
    }

    let query = req.uri().query().unwrap_or_default();
    match InsertionCommand::from_query(query) {
        Ok(command) => {
            let stream_now = fetch_stream_now(&config, &client, &last_seen_pdt).await;
            let start_time = stream_now + chrono::Duration::seconds(command.in_sec as i64);
            let index = slot_counter.fetch_add(1, Ordering::Relaxed);
            let ad_slot = AdSlot {
                id: Uuid::new_v4(),
                index,
                start_time: start_time,
                duration: command.duration,
                pod_num: command.pod_num,
            };
            log::debug!("Received ad slot: {:?}", ad_slot);
            available_slots.0.insert(ad_slot);

            let response = object! {
                status: "success",
                command: {
                    "index": index,
                    "in_sec": command.in_sec,
                    "duration": command.duration,
                    "pod_num": command.pod_num,
                }
            };
            Ok(HttpResponse::Ok()
                .content_type(mime::APPLICATION_JSON)
                .body(response.pretty(2)))
        }
        Err(err) => {
            let response = object! {
                status: "error",
                message: err
            };
            Ok(HttpResponse::BadRequest()
                .content_type(mime::APPLICATION_JSON)
                .body(response.pretty(2)))
        }
    }
}

async fn handle_interstitials(
    req: HttpRequest,
    ad_server_url: web::Data<Url>,
    available_ads: web::Data<AvailableAds>,
    available_slots: web::Data<AvailableAdSlots>,
    config: web::Data<ServerConfig>,
    client: web::Data<Client>,
    user_defined_query_params: web::Data<UserDefinedQueryParams>,
) -> Result<HttpResponse, Error> {
    let ad_server_url = ad_server_url.clone();
    let req_url = req.full_url();

    let interstitial_id =
        get_query_param(&req, HLS_INTERSTITIAL_ID).unwrap_or_else(|| "default_ad".to_string());
    let user_id =
        get_query_param(&req, HLS_PRIMARY_ID).unwrap_or_else(|| "default_user".to_string());
    
    // For non-transcoded ads
    if let Some(linear_id) = get_query_param(&req, AD_ID) {
        return handle_raw_asset_request(&interstitial_id, &linear_id, &user_id, available_ads)
            .await;
    }
    log::info!("Received interstitial request from user {user_id} for slot {interstitial_id}");

    // If a test asset is configured, skip VAST entirely and serve it directly.
    if let Some(test_asset) = &config.test_asset {
        let asset = to_ad_asset_json(&test_asset.url.as_str(), &Ad { duration: test_asset.duration, ..Default::default() }, 0);
        let response = to_asset_list_json_string(vec![asset], test_asset.duration);
        log::info!("Serving test asset directly (no VAST): {response}");
        return Ok(HttpResponse::Ok()
            .content_type(mime::APPLICATION_JSON)
            .body(response));
    }

    let ad_url = build_ad_server_url(
        &ad_server_url,
        &interstitial_id,
        &user_id,
        &available_slots,
        &user_defined_query_params,
    )
    .await?;
    log::info!("Request ad pod with url {ad_url}");
    let mut res = client
        .get(ad_url.as_str())
        // Specify the Accept header to request XML
        .insert_header((header::ACCEPT, APPLICATION_XML))
        .send()
        .await
        .map_err(error::ErrorInternalServerError)?;

    let payload = res.body().await.map_err(error::ErrorInternalServerError)?;
    let xml = std::str::from_utf8(&payload).unwrap();
    log::debug!("VAST response from ad server \n{:?}", xml);
    let vast: vast4_rs::Vast = vast4_rs::from_str(&xml)
        .inspect_err(|err| {
            log::error!("Error parsing VAST: {:?}", err);
        })
        // Return an empty VAST in case of parsing error
        .unwrap_or_default();
    // Wrap the VAST into JSON
    let response = wrap_into_assets(vast, req_url, &interstitial_id, &user_id, &config.test_asset, available_ads);
    log::info!("asset json reply \n{response}");

    Ok(HttpResponse::Ok()
        .content_type(mime::APPLICATION_JSON)
        .body(response))
}

async fn handle_raw_asset_request(
    ad_slot_id: &str,
    linear_id: &str,
    user_id: &str,
    available_ads: web::Data<AvailableAds>,
) -> Result<HttpResponse, Error> {
    log::info!(
        "Received follow-up interstitial request for slot {ad_slot_id} with id {linear_id} from user {user_id}"
    );

    // return http 404 error if the ad is not found
    let linear = available_ads
        .linears
        .get(&Uuid::parse_str(linear_id).unwrap_or_default())
        .ok_or_else(|| error::ErrorNotFound("Ad not found".to_string()))?;

    let segment = MediaSegment::builder()
        .duration(Duration::from_secs(linear.duration))
        .uri(linear.url.clone())
        .build()
        .unwrap();

    // Wrap the MP4 in a media playlist
    let m3u8 = MediaPlaylist::builder()
        .media_sequence(0)
        .target_duration(Duration::from_secs(linear.duration))
        .segments(vec![segment])
        .has_end_list(true)
        .build()
        .inspect(|m3u8| {
            log::debug!("creative playlist \n{m3u8}");
        })
        .unwrap();

    Ok(HttpResponse::Ok()
        .content_type(HLS_PLAYLIST_CONTENT_TYPE)
        .body(m3u8.to_string()))
}

async fn handle_media_stream(
    req: HttpRequest,
    available_slots: web::Data<AvailableAdSlots>,
    config: web::Data<ServerConfig>,
    client: web::Data<Client>,
    user_defined_query_params: web::Data<UserDefinedQueryParams>,
    last_seen_pdt: web::Data<AtomicI64>,
) -> Result<HttpResponse, Error> {
    log::trace!("Received request \n{:?}", req);
    let request_type = get_request_type(&req, &config);

    match request_type {
        RequestType::MasterPlayList => {
            handle_master_playlist(req, config, client, user_defined_query_params).await
        }
        RequestType::MediaPlayList => {
            handle_media_playlist(req, available_slots, config, client, last_seen_pdt).await
        }
        RequestType::Playlist => {
            handle_playlist(req, available_slots, config, client, user_defined_query_params, last_seen_pdt).await
        }
        RequestType::Segment => handle_segment(req, config, client).await,
        // In origin host mode, proxy unrecognised paths (e.g. VTT dummy segments) through
        RequestType::Other => {
            if config.master_playlist_path.is_none() {
                handle_segment(req, config, client).await
            } else {
                Ok(HttpResponse::NotFound().finish())
            }
        }
    }
}

async fn handle_master_playlist(
    req: HttpRequest,
    config: web::Data<ServerConfig>,
    client: web::Data<Client>,
    user_defined_query_params: web::Data<UserDefinedQueryParams>,
) -> Result<HttpResponse, Error> {
    let new_url = build_forward_url(&req, &config.forward_url);

    let mut res = client
        .get(new_url.as_str())
        .send()
        .await
        .inspect_err(|err| {
            log::error!("Error fetching master playlist: {:?}", err);
        })
        .map_err(error::ErrorNotFound)?;

    // Save the user-defined query parameters for later use
    if let Some(query_params) = req.uri().query() {
        if let Some(playback_session_id) = get_header_value(&req, "x-playback-session-id") {
            log::info!("Saved user-defined query parameters: {query_params} for session {playback_session_id}");
            user_defined_query_params.0.insert(
                Uuid::parse_str(&playback_session_id).unwrap_or_default(),
                query_params.to_string(),
            );
        }
    }

    let payload = res.body().await.map_err(error::ErrorBadRequest)?;
    let m3u8 = std::str::from_utf8(&payload).map_err(error::ErrorBadRequest)?;
    let playlist = MasterPlaylist::try_from(m3u8).inspect_err(|err| {
        log::error!(
            "Error {:?} when parsing master playlist. Returning the original playlist.",
            err.to_string()
        );
    });

    if playlist.is_err() {
        // Just pass the original payload in case of parsing error
        return Ok(HttpResponse::Ok()
            .content_type(HLS_PLAYLIST_CONTENT_TYPE)
            .body(payload));
    }

    let mut playlist = playlist.unwrap();
    replace_absolute_url_with_relative_url(&mut playlist);
    let playlist_str = playlist.to_string();

    // Prepend the request's directory path to any relative variant URIs.
    // Needed when the origin returns relative URIs (e.g. "v0/media.m3u8") and the
    // master playlist is served under a sub-path (e.g. /loop/master.m3u8).
    let req_path = req.uri().path();
    let base_dir = req_path.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
    let output = if !base_dir.is_empty() {
        let mut result = String::with_capacity(playlist_str.len() + 64);
        let mut prev_was_stream_inf = false;
        for line in playlist_str.lines() {
            if prev_was_stream_inf && !line.starts_with('#') && !line.starts_with("http") && !line.starts_with('/') {
                result.push_str(base_dir);
                result.push('/');
            }
            result.push_str(line);
            result.push('\n');
            prev_was_stream_inf = line.starts_with("#EXT-X-STREAM-INF");
        }
        result
    } else {
        playlist_str
    };

    log::debug!("master playlist \n{output}");

    Ok(HttpResponse::Ok()
        .content_type(HLS_PLAYLIST_CONTENT_TYPE)
        .body(output))
}

async fn handle_media_playlist(
    req: HttpRequest,
    available_slots: web::Data<AvailableAdSlots>,
    config: web::Data<ServerConfig>,
    client: web::Data<Client>,
    last_seen_pdt: web::Data<AtomicI64>,
) -> Result<HttpResponse, Error> {
    let new_url = build_forward_url(&req, &config.forward_url);

    let mut res = client
        .get(new_url.as_str())
        .send()
        .await
        .map_err(error::ErrorInternalServerError)?;

    let payload = res.body().await.map_err(error::ErrorInternalServerError)?;
    let m3u8 = std::str::from_utf8(&payload).map_err(error::ErrorInternalServerError)?;
    let playlist = MediaPlaylist::try_from(m3u8).inspect_err(|err| {
        log::error!(
            "Error {:?} when parsing media playlist. Returning the original playlist.",
            err.to_string()
        );
    });

    if playlist.is_err() {
        // Just pass the original payload in case of parsing error
        return Ok(HttpResponse::Ok()
            .content_type(HLS_PLAYLIST_CONTENT_TYPE)
            .body(payload.clone()));
    }

    let playlist = playlist.unwrap();
    handle_media_playlist_content(playlist, available_slots, config, last_seen_pdt).await
}

async fn handle_master_playlist_content(
    req: HttpRequest,
    mut playlist: MasterPlaylist<'_>,
    user_defined_query_params: web::Data<UserDefinedQueryParams>,
) -> Result<HttpResponse, Error> {
    // Save the user-defined query parameters for later use
    if let Some(query_params) = req.uri().query() {
        if let Some(playback_session_id) = get_header_value(&req, "x-playback-session-id") {
            log::info!("Saved user-defined query parameters: {query_params} for session {playback_session_id}");
            user_defined_query_params.0.insert(
                Uuid::parse_str(&playback_session_id).unwrap_or_default(),
                query_params.to_string(),
            );
        }
    }

    replace_absolute_url_with_relative_url(&mut playlist);
    let playlist_str = playlist.to_string();

    // Prepend the request's directory path to any still-relative variant URIs.
    // Needed when the origin returns relative URIs (e.g. "v0/media.m3u8") and the
    // master playlist is served under a sub-path (e.g. /loop/master.m3u8).
    let req_path = req.uri().path();
    let base_dir = req_path.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
    let output = if !base_dir.is_empty() {
        let mut result = String::with_capacity(playlist_str.len() + 64);
        let mut prev_was_stream_inf = false;
        for line in playlist_str.lines() {
            if prev_was_stream_inf && !line.starts_with('#') && !line.starts_with("http") && !line.starts_with('/') {
                result.push_str(base_dir);
                result.push('/');
            }
            result.push_str(line);
            result.push('\n');
            prev_was_stream_inf = line.starts_with("#EXT-X-STREAM-INF");
        }
        result
    } else {
        playlist_str
    };

    log::debug!("master playlist \n{output}");

    Ok(HttpResponse::Ok()
        .content_type(HLS_PLAYLIST_CONTENT_TYPE)
        .body(output))
}

async fn handle_media_playlist_content(
    mut playlist: MediaPlaylist<'_>,
    available_slots: web::Data<AvailableAdSlots>,
    config: web::Data<ServerConfig>,
    last_seen_pdt: web::Data<AtomicI64>,
) -> Result<HttpResponse, Error> {
    insert_interstitials(&mut playlist, &config, available_slots);
    // Update after insertion so synthetic PDT (injected when stream has none) is visible
    update_last_seen_pdt(&playlist, &last_seen_pdt);
    // Emit a com.apple.hls.preload companion DATERANGE ahead of each injected
    // interstitial so players can preload the interstitial resource early.
    let output = inject_preload_date_ranges(&playlist.to_string());
    log::debug!("media playlist \n{output}");

    Ok(HttpResponse::Ok()
        .content_type(HLS_PLAYLIST_CONTENT_TYPE)
        .body(output))
}

async fn handle_playlist(
    req: HttpRequest,
    available_slots: web::Data<AvailableAdSlots>,
    config: web::Data<ServerConfig>,
    client: web::Data<Client>,
    user_defined_query_params: web::Data<UserDefinedQueryParams>,
    last_seen_pdt: web::Data<AtomicI64>,
) -> Result<HttpResponse, Error> {
    let new_url = build_forward_url(&req, &config.forward_url);

    let mut res = client
        .get(new_url.as_str())
        .send()
        .await
        .map_err(error::ErrorBadGateway)?;

    let payload = res.body().await.map_err(error::ErrorBadGateway)?;
    let m3u8 = std::str::from_utf8(&payload).map_err(error::ErrorBadRequest)?;

    // Try parsing as master playlist first
    if let Ok(master) = MasterPlaylist::try_from(m3u8) {
        return handle_master_playlist_content(req, master, user_defined_query_params).await;
    }

    // Otherwise handle as media playlist
    if let Ok(media) = MediaPlaylist::try_from(m3u8) {
        return handle_media_playlist_content(media, available_slots, config, last_seen_pdt).await;
    }

    // If neither parsing works, return the original content
    log::warn!("Could not parse playlist as master or media playlist, returning original");
    Ok(HttpResponse::Ok()
        .content_type(HLS_PLAYLIST_CONTENT_TYPE)
        .body(payload))
}

async fn handle_segment(
    req: HttpRequest,
    config: web::Data<ServerConfig>,
    client: web::Data<Client>,
) -> Result<HttpResponse, Error> {
    let new_url = build_forward_url(&req, &config.forward_url);
    let res = client
        .get(new_url.as_str())
        .send()
        .await
        .map_err(error::ErrorInternalServerError)?;

    let mut client_resp = HttpResponse::build(res.status());
    copy_headers(&res, &mut client_resp);

    Ok(client_resp.streaming(res))
}

async fn handle_status(
    config: web::Data<ServerConfig>,
    ad_server_url: web::Data<Url>,
    available_ads: web::Data<AvailableAds>,
    available_slots: web::Data<AvailableAdSlots>,
    user_defined_query_params: web::Data<UserDefinedQueryParams>,
) -> Result<HttpResponse, Error> {
    // Return the status of the server
    let response = object! {
        "config": config.to_json(),
        "ad_server_url": ad_server_url.as_str(),
        "user_defined_query_params": user_defined_query_params.to_json(),
        "available_ads": available_ads.to_json(),
        "available_slots": available_slots.to_json(),
    }
    .pretty(2);

    Ok(HttpResponse::Ok()
        .content_type(mime::APPLICATION_JSON)
        .body(response))
}

fn parse_into_u64(value: &str, default: u64) -> u64 {
    value.parse().unwrap_or(default)
}

fn parse_default_values(args: &CliArguments) -> (u64, u64, u64) {
    (
        parse_into_u64(&args.default_ad_duration, 10),     // Default ad duration is 10 seconds
        parse_into_u64(&args.default_repeating_cycle, 30), // Default repeating cycle is 30 seconds
        parse_into_u64(&args.default_ad_number, 1000),     // Default ad number is 1000
    )
}

async fn inspect_master_playlist(
    config: Arc<ClientConfig>,
    master_playlist_url: &Url,
) -> Result<(), Error> {
    if !is_hls_playlist(master_playlist_url.as_str()) {
        return Err(error::ErrorBadRequest("Illegal master playlist URL".to_string()));
    }

    log::info!("Inspecting source stream at: {}", master_playlist_url);
    let client = make_https_client(config);
    let payload = client
        .get(master_playlist_url.as_str())
        .send()
        .await
        .map_err(error::ErrorBadRequest)?
        .body()
        .await
        .map_err(error::ErrorBadRequest)?;
 
    let m3u8 = std::str::from_utf8(&payload).map_err(error::ErrorBadRequest)?;
    
    // Try to parse the master playlist
    MasterPlaylist::try_from(m3u8).map_err(|err| {
        error::ErrorBadRequest(format!("Invalid master playlist: {}", err))
    })?;

    Ok(())
}

async fn parse_test_asset_url(config: Arc<ClientConfig>, path: &str) -> Option<TestAsset> {
    if path.is_empty() || !is_hls_playlist(path) {
        log::error!("Test asset URL is not a valid HLS playlist: {path}");
        return None;
    }

    log::info!("Parsing test asset URL: {path}");
    let url = Url::parse(path).ok()?;
    let client = make_https_client(config);
    let payload = client.get(url.as_str()).send().await.ok()?.body().await.ok()?;
    let text = std::str::from_utf8(&payload).ok()?;

    // If the URL points to a master playlist, resolve the first variant to a media playlist.
    let (media_url, media_text) = if let Ok(master) = MasterPlaylist::try_from(text) {
        let variant_uri = master.variant_streams.iter().find_map(|v| {
            if let VariantStream::ExtXStreamInf { uri, .. } = v { Some(uri.as_ref()) } else { None }
        })?;
        let media_url = url.join(variant_uri).ok()?;
        log::info!("Test asset is a master playlist; using first variant: {media_url}");
        let payload2 = client.get(media_url.as_str()).send().await.ok()?.body().await.ok()?;
        let text2 = String::from_utf8(payload2.to_vec()).ok()?;
        (media_url, text2)
    } else {
        (url.clone(), text.to_string())
    };

    let m3u8 = MediaPlaylist::try_from(media_text.as_str()).ok()?;
    if !is_fragmented_mp4_vod_media_playlist(&m3u8) {
        log::error!("Test asset at {media_url} is not a valid fragmented MP4 VoD media playlist.");
        return None;
    }

    let duration = m3u8.segments.iter().map(|(_, s)| s.duration.duration().as_secs()).sum();
    // Use the master URL so the player can do ABR selection for the ad asset.
    Some(TestAsset::new(url, duration))
}

fn make_https_client(config: Arc<rustls::ClientConfig>) -> Client {
    Client::builder()
        // Add User-Agent header to make requests
        .add_default_header((header::USER_AGENT, "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0.1 Safari/605.1.15"))
        // a "connector" wraps the stream into an encrypted connection
        .connector(Connector::new().rustls_0_23(config.clone()))
        .finish()
}

#[actix_web::main]
async fn main() -> io::Result<()> {
    env_logger::init_from_env(env_logger::Env::new().default_filter_or("info"));

    let args = CliArguments::parse();
    let (default_ad_duration, default_repeating_cycle, default_ad_number) =
        parse_default_values(&args);

    let client_tls_config = Arc::new(rustls_config());

    // Determine mode and set forward_url and master_playlist_path
    let (forward_url, master_playlist_path) = if let Some(ref origin) = args.origin_host {
        // Origin host mode
        let url = Url::parse(origin).expect("Invalid origin host URL");
        (url, None)
    } else {
        // Specific playlist mode (existing behavior)
        let master_url = Url::parse(args.master_playlist_url.as_ref().unwrap())
            .expect("Invalid master playlist URL");
        inspect_master_playlist(client_tls_config.clone(), &master_url)
            .await
            .expect("Failed to inspect master playlist");
        let forward_url = base_url(&master_url).expect("Invalid forward URL");
        let playlist_path = master_url.path().to_string();
        (forward_url, Some(playlist_path))
    };

    let test_asset = parse_test_asset_url(client_tls_config.clone(), &args.test_asset_url).await;

    let listen_url = format!("http://{}:{}", &args.listen_addr, &args.listen_port);
    let listen_url = Url::parse(&listen_url).expect("Invalid listen address");

    let interstitials_address = if args.interstitials_address.is_empty() {
        format!("http://localhost:{}", &args.listen_port)
    } else {
        args.interstitials_address
    };
    let interstitials_address =
        Url::parse(&interstitials_address).expect("Invalid interstitials address");

    let ad_server_url = args.ad_server_endpoint
        .as_deref()
        .map(|s| Url::parse(s).expect("Invalid ad server URL"))
        .unwrap_or_else(|| Url::parse("http://localhost/no-vast").unwrap());

    log::info!("Program started at: {:?}", *START_TIME);
    log::info!("Starting HTTP server at {listen_url}, forwarding to {forward_url}, interstitials' base URL: {interstitials_address}");
    log::info!(
        "Ad server endpoint: {}, {:?} insertion",
        args.ad_server_endpoint.as_deref().unwrap_or("none (test asset mode)"),
        args.ad_insertion_mode.to_str()
    );
    log::info!("Default ad duration: {}s, repeating cycle: {}s, ad number: {}",
        default_ad_duration,
        default_repeating_cycle,
        default_ad_number
    );

    if let Some(ref playlist_path) = master_playlist_path {
        let proxied_playlist_path = listen_url.join(playlist_path)
            .expect("Failed to join listen URL with playlist path");
        log::info!("Proxied stream will be available at: {proxied_playlist_path}");
    } else {
        log::info!("Origin host mode enabled - any stream path will be proxied");
    }

    if let Some(ref asset) = test_asset {
        log::info!("Test asset URL: {}, duration: {}s", asset.url, asset.duration);
    }

    let target_ad_duration = test_asset.as_ref()
        .map_or(default_ad_duration, |asset| asset.duration as u64);
    if args.ad_insertion_mode==InsertionMode::Static && default_repeating_cycle < target_ad_duration {
        log::warn!("Ad duration is greater than the repeating cycle. This may cause issues for live streams.");
    }

    // Parse skip-control args; empty string means "not configured".
    let skip_control_offset: Option<u64> = if args.skip_control_offset.is_empty() {
        None
    } else {
        args.skip_control_offset.parse().ok()
    };
    let skip_control_duration: Option<u64> = if args.skip_control_duration.is_empty() {
        None
    } else {
        match args.skip_control_duration.parse::<u64>() {
            Ok(v) if v >= 1 => Some(v),
            Ok(_) => {
                log::warn!("skip-control-duration must be >= 1; ignoring configured value");
                None
            }
            Err(_) => None,
        }
    };
    let skip_control_label_id: Option<String> = if args.skip_control_label_id.is_empty() {
        None
    } else if is_valid_label_id(&args.skip_control_label_id) {
        Some(args.skip_control_label_id.clone())
    } else {
        log::warn!(
            "skip-control-label-id {:?} contains invalid characters (only ASCII letters, \
             hyphens, underscores allowed); ignoring",
            args.skip_control_label_id
        );
        None
    };

    let available_slots = AvailableAdSlots::default();
    let available_ads = AvailableAds::default();
    let last_seen_pdt = web::Data::new(AtomicI64::new(0));
    let slot_counter = web::Data::new(AtomicU64::new(0));
    let server_config = ServerConfig::new(
        forward_url,
        interstitials_address,
        master_playlist_path,
        args.ad_insertion_mode,
        target_ad_duration,
        default_repeating_cycle,
        default_ad_number,
        test_asset,
        skip_control_offset,
        skip_control_duration,
        skip_control_label_id,
    );
    let user_defined_query_params = UserDefinedQueryParams::default();

    HttpServer::new(move || {
        let cors = actix_cors::Cors::permissive();

        // create https client inside `HttpServer::new` closure to have one per worker thread
        let client = make_https_client(client_tls_config.clone());

        App::new()
            .app_data(web::Data::new(client))
            .app_data(web::Data::new(available_slots.clone()))
            .app_data(web::Data::new(available_ads.clone()))
            .app_data(web::Data::new(server_config.clone()))
            .app_data(web::Data::new(ad_server_url.clone()))
            .app_data(web::Data::new(user_defined_query_params.clone()))
            .app_data(last_seen_pdt.clone())
            .app_data(slot_counter.clone())
            .wrap(middleware::Logger::default())
            .wrap(cors)
            .route(COMMAND_PREFIX, web::get().to(handle_commands))
            .route(STATUS_PREFIX, web::get().to(handle_status))
            .route(INTERSTITIAL_PLAYLIST, web::get().to(handle_interstitials))
            .default_service(web::to(handle_media_stream))
    })
    .bind((args.listen_addr, args.listen_port))?
    .workers(2)
    .run()
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- helpers ----

    fn dummy_config(
        skip_control_offset: Option<u64>,
        skip_control_duration: Option<u64>,
        skip_control_label_id: Option<&str>,
    ) -> ServerConfig {
        ServerConfig {
            forward_url: Url::parse("http://localhost/").unwrap(),
            interstitials_address: Url::parse("http://localhost:8080/").unwrap(),
            master_playlist_path: None,
            insertion_mode: InsertionMode::Static,
            target_ad_duration: 30,
            target_repeating_cycle: 60,
            target_ad_number: 10,
            test_asset: None,
            skip_control_offset,
            skip_control_duration,
            skip_control_label_id: skip_control_label_id.map(str::to_owned),
        }
    }

    /// Build a minimal ExtXDateRange with the given skip-control config and return its
    /// serialised string so tests can check exact attribute presence/format.
    fn build_daterange(cfg: &ServerConfig, slot_duration: f32) -> String {
        let sc = resolve_skip_control(cfg, slot_duration);

        let mut b = ExtXDateRange::builder();
        b.id("test-slot")
            .start_date("2026-01-01T00:00:00.000Z");

        if sc.is_some() {
            b.insert_client_attribute("X-RESTRICT", Value::String("JUMP".into()));
        } else {
            b.insert_client_attribute("X-RESTRICT", Value::String("SKIP,JUMP".into()));
        }

        if let Some(ref sc) = sc {
            b.insert_client_attribute(
                "X-SKIP-CONTROL-OFFSET",
                Value::Float(hls_m3u8::types::Float::new(sc.offset as f32)),
            );
            if let Some(dur) = sc.duration {
                b.insert_client_attribute(
                    "X-SKIP-CONTROL-DURATION",
                    Value::Float(hls_m3u8::types::Float::new(dur as f32)),
                );
            }
            if let Some(ref label) = sc.label_id {
                b.insert_client_attribute(
                    "X-SKIP-CONTROL-LABEL-ID",
                    Value::String(label.clone().into()),
                );
            }
        }

        b.build().unwrap().to_string()
    }

    fn make_slot(start: chrono::DateTime<chrono::Local>, duration: u64) -> AdSlot {
        AdSlot {
            id: Uuid::new_v4(),
            index: 0,
            start_time: start,
            duration,
            pod_num: 2,
        }
    }

    // ---- is_valid_label_id ----

    #[test]
    fn label_id_accepts_letters_hyphen_underscore() {
        assert!(is_valid_label_id("skip_ad"));
        assert!(is_valid_label_id("Skip-Ad"));
        assert!(is_valid_label_id("a"));
        assert!(is_valid_label_id("ABC_def-GHI"));
    }

    #[test]
    fn label_id_rejects_empty_and_non_ascii() {
        assert!(!is_valid_label_id(""));
        assert!(!is_valid_label_id("skip ad"));   // space
        assert!(!is_valid_label_id("skip.ad"));   // dot
        assert!(!is_valid_label_id("skip123"));   // digits
        assert!(!is_valid_label_id("skip\u{e9}")); // non-ASCII
    }

    // ---- resolve_skip_control ----

    #[test]
    fn no_skip_control_when_offset_absent() {
        let cfg = dummy_config(None, None, None);
        assert!(resolve_skip_control(&cfg, 30.0).is_none());
    }

    #[test]
    fn no_skip_control_when_offset_equals_duration() {
        let cfg = dummy_config(Some(30), None, None);
        assert!(resolve_skip_control(&cfg, 30.0).is_none());
    }

    #[test]
    fn no_skip_control_when_offset_exceeds_duration() {
        let cfg = dummy_config(Some(40), None, None);
        assert!(resolve_skip_control(&cfg, 30.0).is_none());
    }

    #[test]
    fn skip_control_applies_when_offset_lt_duration() {
        let cfg = dummy_config(Some(5), None, None);
        let sc = resolve_skip_control(&cfg, 30.0).expect("should have attrs");
        assert_eq!(sc.offset, 5);
        assert!(sc.duration.is_none());
        assert!(sc.label_id.is_none());
    }

    #[test]
    fn skip_control_includes_duration_when_set() {
        let cfg = dummy_config(Some(5), Some(10), None);
        let sc = resolve_skip_control(&cfg, 30.0).unwrap();
        assert_eq!(sc.duration, Some(10));
    }

    #[test]
    fn skip_control_includes_label_when_valid() {
        let cfg = dummy_config(Some(5), None, Some("skip_ad"));
        let sc = resolve_skip_control(&cfg, 30.0).unwrap();
        assert_eq!(sc.label_id.as_deref(), Some("skip_ad"));
    }

    #[test]
    fn skip_control_omits_label_when_invalid() {
        // digits in label-id → suppressed, but other attrs still emitted
        let cfg = dummy_config(Some(5), None, Some("skip123"));
        let sc = resolve_skip_control(&cfg, 30.0).unwrap();
        assert!(sc.label_id.is_none());
        assert_eq!(sc.offset, 5);
    }

    // ---- DATERANGE serialisation ----

    #[test]
    fn daterange_no_skip_control_has_skip_jump_restrict() {
        let cfg = dummy_config(None, None, None);
        let s = build_daterange(&cfg, 30.0);
        assert!(s.contains("X-RESTRICT=\"SKIP,JUMP\""), "got: {s}");
        assert!(!s.contains("X-SKIP-CONTROL"), "got: {s}");
    }

    #[test]
    fn daterange_skip_control_offset_is_unquoted_integer() {
        let cfg = dummy_config(Some(5), None, None);
        let s = build_daterange(&cfg, 30.0);
        // OFFSET must be unquoted (no surrounding quotes around the value)
        assert!(s.contains("X-SKIP-CONTROL-OFFSET=5"), "got: {s}");
        // Must NOT be quoted
        assert!(!s.contains("X-SKIP-CONTROL-OFFSET=\"5\""), "got: {s}");
        // X-RESTRICT must be JUMP only
        assert!(s.contains("X-RESTRICT=\"JUMP\""), "got: {s}");
        assert!(!s.contains("SKIP,JUMP"), "got: {s}");
    }

    #[test]
    fn daterange_skip_control_duration_is_unquoted_integer() {
        let cfg = dummy_config(Some(5), Some(10), None);
        let s = build_daterange(&cfg, 30.0);
        assert!(s.contains("X-SKIP-CONTROL-DURATION=10"), "got: {s}");
        assert!(!s.contains("X-SKIP-CONTROL-DURATION=\"10\""), "got: {s}");
    }

    #[test]
    fn daterange_skip_control_label_id_is_quoted() {
        let cfg = dummy_config(Some(5), None, Some("skip_ad"));
        let s = build_daterange(&cfg, 30.0);
        assert!(s.contains("X-SKIP-CONTROL-LABEL-ID=\"skip_ad\""), "got: {s}");
    }

    #[test]
    fn daterange_full_skip_control_all_attrs_present() {
        let cfg = dummy_config(Some(5), Some(10), Some("skip_ad"));
        let s = build_daterange(&cfg, 30.0);
        assert!(s.contains("X-RESTRICT=\"JUMP\""), "got: {s}");
        assert!(s.contains("X-SKIP-CONTROL-OFFSET=5"), "got: {s}");
        assert!(s.contains("X-SKIP-CONTROL-DURATION=10"), "got: {s}");
        assert!(s.contains("X-SKIP-CONTROL-LABEL-ID=\"skip_ad\""), "got: {s}");
        assert!(!s.contains("SKIP,JUMP"), "got: {s}");
    }

    #[test]
    fn daterange_offset_at_boundary_suppresses_skip_control() {
        // offset == duration → suppressed; SKIP,JUMP unchanged
        let cfg = dummy_config(Some(30), Some(5), Some("skip_ad"));
        let s = build_daterange(&cfg, 30.0);
        assert!(s.contains("X-RESTRICT=\"SKIP,JUMP\""), "got: {s}");
        assert!(!s.contains("X-SKIP-CONTROL"), "got: {s}");
    }

    #[test]
    fn daterange_invalid_label_suppressed_but_other_attrs_emitted() {
        // invalid label-id is suppressed but offset is still emitted
        let cfg = dummy_config(Some(5), Some(8), Some("bad label!"));
        let s = build_daterange(&cfg, 30.0);
        assert!(s.contains("X-SKIP-CONTROL-OFFSET=5"), "got: {s}");
        assert!(s.contains("X-SKIP-CONTROL-DURATION=8"), "got: {s}");
        assert!(!s.contains("X-SKIP-CONTROL-LABEL-ID"), "got: {s}");
    }

    #[test]
    fn daterange_zero_offset_is_valid_immediate_skip() {
        // offset=0 means immediate; 0 < 30 so it applies
        let cfg = dummy_config(Some(0), None, None);
        let s = build_daterange(&cfg, 30.0);
        assert!(s.contains("X-SKIP-CONTROL-OFFSET=0"), "got: {s}");
        assert!(s.contains("X-RESTRICT=\"JUMP\""), "got: {s}");
    }

    // ---- AdSlot::expires_at and break-window eviction (issue #37) ----

    // Issue #37: a command-triggered break must stay available for the whole
    // `dur` window so concurrent viewers all get the ad, not just the first.
    #[test]
    fn slot_survives_eviction_while_break_is_active() {
        let now = chrono::Local::now();
        // A 15s break that started 5s ago is still active.
        let slot = make_slot(now - chrono::Duration::seconds(5), 15);

        let slots = AvailableAdSlots::default();
        slots.0.insert(slot.clone());

        // Simulate the eviction that runs on every media-playlist request:
        // retain while the break window has not fully scrolled past `window_start`.
        let window_start = now;
        slots.0.retain(|s| s.expires_at() >= window_start);

        // The slot representing the in-flight break must still be present so a
        // second (concurrent) viewer polling now still receives the DATERANGE.
        assert_eq!(slots.0.len(), 1, "active break was evicted too early");
        assert!(slots.0.contains(&slot));
    }

    // Once the break has fully ended, the slot is evicted as before.
    #[test]
    fn slot_is_evicted_after_break_ends() {
        let now = chrono::Local::now();
        // A 15s break that started 30s ago has fully ended.
        let slot = make_slot(now - chrono::Duration::seconds(30), 15);

        let slots = AvailableAdSlots::default();
        slots.0.insert(slot);

        let window_start = now;
        slots.0.retain(|s| s.expires_at() >= window_start);

        assert_eq!(slots.0.len(), 0, "ended break should be evicted");
    }

    #[test]
    fn expires_at_is_start_plus_duration() {
        let start = chrono::Local::now();
        let slot = make_slot(start, 15);
        assert_eq!(slot.expires_at(), start + chrono::Duration::seconds(15));
    }

    // ---- preload DATERANGE (issue #11) ----

    // Issue #11: an injected interstitial DATERANGE must be accompanied by a
    // companion `com.apple.hls.preload` DATERANGE that carries its own unique
    // ID, a START-DATE, and the three X- preload attributes.
    #[test]
    fn preload_date_range_emitted_for_interstitial() {
        let interstitial = ExtXDateRange::builder()
            .id("ad-slot-1")
            .class("com.apple.hls.interstitial")
            .start_date("2026-09-17T10:00:00.000Z")
            .duration(Duration::from_secs(30))
            .insert_client_attribute(
                "X-ASSET-LIST",
                Value::String("https://proxy.example/interstitials?_HLS_interstitial_id=ad-slot-1".into()),
            )
            .build()
            .unwrap();

        let preload = build_preload_date_range(&interstitial).expect("preload should be built");
        let line = preload.to_string();

        // Own unique ID, distinct from the target's ID.
        assert!(line.contains("ID=\"ad-slot-1-preload\""), "line: {line}");
        // Preload CLASS.
        assert!(line.contains("CLASS=\"com.apple.hls.preload\""), "line: {line}");
        // START-DATE consistent with the target.
        assert!(line.contains("START-DATE=\"2026-09-17T10:00:00.000Z\""), "line: {line}");
        // A preload DATERANGE MUST carry DURATION (or END-DATE); it mirrors the target's.
        assert!(line.contains("DURATION=30"), "line: {line}");
        // The three preload X- attributes.
        assert!(
            line.contains("X-URI=\"https://proxy.example/interstitials?_HLS_interstitial_id=ad-slot-1\""),
            "line: {line}"
        );
        assert!(line.contains("X-TARGET-ID=\"ad-slot-1\""), "line: {line}");
        assert!(line.contains("X-TARGET-CLASS=\"com.apple.hls.interstitial\""), "line: {line}");
    }

    // The manifest post-processor emits exactly one preload line ahead of each
    // interstitial DATERANGE line, and leaves other lines untouched.
    #[test]
    fn inject_preload_prepends_line_before_interstitial() {
        let manifest = concat!(
            "#EXTM3U\n",
            "#EXT-X-VERSION:6\n",
            "#EXT-X-PROGRAM-DATE-TIME:2026-09-17T10:00:00.000Z\n",
            "#EXT-X-DATERANGE:ID=\"ad-slot-1\",CLASS=\"com.apple.hls.interstitial\",",
            "START-DATE=\"2026-09-17T10:00:00.000Z\",DURATION=30,",
            "X-ASSET-LIST=\"https://proxy.example/interstitials?id=1\"\n",
            "#EXTINF:6.0,\n",
            "seg0.ts\n",
        );

        let out = inject_preload_date_ranges(manifest);

        // Exactly one preload line was added.
        assert_eq!(out.matches("CLASS=\"com.apple.hls.preload\"").count(), 1, "out: {out}");
        // It sits before the interstitial line.
        let preload_pos = out.find("com.apple.hls.preload").unwrap();
        let interstitial_pos = out.find("com.apple.hls.interstitial").unwrap();
        assert!(preload_pos < interstitial_pos, "out: {out}");
        // The preload line carries the target's DURATION, so it is spec-legal.
        let preload_line = out.lines().find(|l| l.contains("com.apple.hls.preload")).unwrap();
        assert!(preload_line.contains("DURATION=30"), "preload_line: {preload_line}");
        // Non-DATERANGE content is preserved.
        assert!(out.contains("#EXTINF:6.0,"), "out: {out}");
        assert!(out.contains("seg0.ts"), "out: {out}");
    }
}
