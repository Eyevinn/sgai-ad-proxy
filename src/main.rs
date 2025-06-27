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

    /// HLS stream address (protocol://ip:port/path)
    /// (e.g., http://localhost/test/master.m3u8)
    #[clap(verbatim_doc_comment)]
    master_playlist_url: String,

    /// Ad server endpoint (protocol://ip:port/path)
    /// It should be a VAST4.0/4.1 XML compatible endpoint
    #[clap(verbatim_doc_comment)]
    ad_server_endpoint: String,

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
    master_playlist_path: String,
    insertion_mode: InsertionMode,
    target_ad_duration: u64,
    target_repeating_cycle: u64,
    target_ad_number: u64,
    test_asset: Option<TestAsset>,
}

impl ServerConfig {
    fn new(
        forward_url: Url,
        interstitials_address: Url,
        master_playlist_path: String,
        insertion_mode: InsertionMode,
        target_ad_duration: u64,
        target_repeating_cycle: u64,
        target_ad_number: u64,
        test_asset: Option<TestAsset>,
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
        }
    }

    fn to_json(&self) -> json::JsonValue {
        object! {
            "forward_url": self.forward_url.as_str(),
            "interstitials_address": self.interstitials_address.as_str(),
            "master_playlist_path": self.master_playlist_path.clone(),
            "insertion_mode": self.insertion_mode.to_str(),
            "target_ad_duration": self.target_ad_duration,
            "target_repeating_cycle": self.target_repeating_cycle,
            "target_ad_number": self.target_ad_number,
            "test_asset": self.test_asset.as_ref().map(|asset| asset.to_json()).unwrap_or_else(|| object! {}),
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
    if path.contains(config.master_playlist_path.as_str()) {
        return RequestType::MasterPlayList;
    } else if is_media_segment(path) {
        return RequestType::Segment;
    } else if path.ends_with(".m3u8") {
        return RequestType::MediaPlayList;
    } else {
        return RequestType::Other;
    }
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
        if !is_vod {
            log::warn!("No program_date_time found in the live stream media playlist. Skipping interstitials.");
            return;
        }
        log::warn!("No program_date_time found in the VOD stream media playlist. Using the server start time.");

        // Use server start time as the program_date_time for the first segment
        segments.find_first_mut().and_then(|first_segment| {
            // Add to the playlist
            first_segment.program_date_time = Some(make_program_date_time_tag(&START_TIME));

            // Update the optional
            first_program_date_time = Some(*START_TIME);

            log::info!(
                "Insert program_date_time: {:?} to first segment",
                first_program_date_time
            );
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

    // Match the ad slots with the segments
    let interstitials: Vec<_> = expected_program_date_time_list
        .iter()
        .enumerate()
        .filter_map(|(index, (program_date_time, duration))| {
            // Match the segment with the first possible ad slot
            ad_slots.iter().find_map(|ad_slot| {
                let expected_date_time = ad_slot.start_time;
                let next_program_date_time = expected_date_time + *duration;
                // The ad slot is between two segments
                if program_date_time >= &expected_date_time
                    && program_date_time < &next_program_date_time
                {
                    log::debug!("Insert interstitial at time: {expected_date_time}");

                    let ad_slot_name = ad_slot.name();
                    let url = format!(
                        "{interstitials_address}{INTERSTITIAL_PLAYLIST}?{HLS_INTERSTITIAL_ID}={ad_slot_name}"
                    );
                    let slot_duration = ad_slot.duration as f32;
                    
                    let mut date_range = ExtXDateRange::builder();
                    date_range
                        .id(ad_slot_name)
                        .class("com.apple.hls.interstitial")
                        .start_date(
                            expected_date_time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                        )
                        .duration(Duration::from_secs_f32(slot_duration))
                        .insert_client_attribute("X-ASSET-LIST", Value::String(url.into()))
                        .insert_client_attribute("X-SNAP", Value::String("IN,OUT".into()))
                        .insert_client_attribute("X-RESTRICT", Value::String("SKIP,JUMP".into()));
                    if is_vod {
                        // Set the resume offset to 0 for VOD streams
                        date_range.insert_client_attribute(
                            "X-RESUME-OFFSET",
                            Value::Float(hls_m3u8::types::Float::new(0.0)),
                        );
                    }
                    let date_range = date_range
                        .build()
                        .unwrap();

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

// Take http get requests and parse the query string into commands
async fn handle_commands(
    req: HttpRequest,
    config: web::Data<ServerConfig>,
    available_slots: web::Data<AvailableAdSlots>,
) -> Result<HttpResponse, Error> {
    if config.insertion_mode == InsertionMode::Static {
        return Ok(HttpResponse::BadRequest().body("Ad insertion is not supported in static mode."));
    }

    let query = req.uri().query().unwrap_or_default();
    match InsertionCommand::from_query(query) {
        Ok(command) => {
            let now = chrono::offset::Local::now();
            let start_time = now + chrono::Duration::seconds(command.in_sec as i64);
            let index = available_slots.0.len() as u64;
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
) -> Result<HttpResponse, Error> {
    log::trace!("Received request \n{:?}", req);
    let request_type = get_request_type(&req, &config);

    match request_type {
        RequestType::MasterPlayList => {
            handle_master_playlist(req, config, client, user_defined_query_params).await
        }
        RequestType::MediaPlayList => {
            handle_media_playlist(req, available_slots, config, client).await
        }
        RequestType::Segment => handle_segment(req, config, client).await,
        RequestType::Other => Ok(HttpResponse::NotFound().finish()),
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
    log::debug!("master playlist \n{playlist}");

    Ok(HttpResponse::Ok()
        .content_type(HLS_PLAYLIST_CONTENT_TYPE)
        .body(playlist.to_string()))
}

async fn handle_media_playlist(
    req: HttpRequest,
    available_slots: web::Data<AvailableAdSlots>,
    config: web::Data<ServerConfig>,
    client: web::Data<Client>,
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

    let mut playlist = playlist.unwrap();
    insert_interstitials(&mut playlist, &config, available_slots);
    log::debug!("media playlist \n{playlist}");

    Ok(HttpResponse::Ok()
        .content_type(HLS_PLAYLIST_CONTENT_TYPE)
        .body(playlist.to_string()))
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
    let m3u8 = MediaPlaylist::try_from(std::str::from_utf8(&payload).ok()?).ok()?;
    if !is_fragmented_mp4_vod_media_playlist(&m3u8) {
        log::error!("Test asset at {path} is not a valid fragmented MP4 VoD media playlist.");
        return None;
    }
    
    let duration = m3u8.segments.iter().map(|(_, s)| s.duration.duration().as_secs()).sum();    
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

    let master_playlist_url =
        Url::parse(&args.master_playlist_url).expect("Invalid master playlist URL");

    let client_tls_config = Arc::new(rustls_config());
    // Inspect the master playlist to ensure it's valid
    inspect_master_playlist(client_tls_config.clone(), &master_playlist_url)
        .await
        .expect("Failed to inspect master playlist");

    let test_asset = parse_test_asset_url(client_tls_config.clone(), &args.test_asset_url).await;

    // Forward URL is the base URL of the master playlist
    let forward_url = base_url(&master_playlist_url).expect("Invalid forward URL");
    let playlist_path = master_playlist_url.path();

    let listen_url = format!("http://{}:{}", &args.listen_addr, &args.listen_port);
    let listen_url = Url::parse(&listen_url).expect("Invalid listen address");
    let proxied_playlist_path = listen_url.join(playlist_path)
        .expect("Failed to join listen URL with playlist path");

    let interstitials_address = if args.interstitials_address.is_empty() {
        format!("http://localhost:{}", &args.listen_port)
    } else {
        args.interstitials_address
    };
    let interstitials_address =
        Url::parse(&interstitials_address).expect("Invalid interstitials address");

    let ad_server_url = Url::parse(&args.ad_server_endpoint).unwrap();

    log::info!("Program started at: {:?}", *START_TIME);
    log::info!("Starting HTTP server at {listen_url}, forwarding to {forward_url}, interstitials' base URL: {interstitials_address}");
    log::info!(
        "Ad server endpoint: {ad_server_url}, {:?} insertion",
        args.ad_insertion_mode.to_str()
    );
    log::info!("Default ad duration: {}s, repeating cycle: {}s, ad number: {}",
        default_ad_duration,
        default_repeating_cycle,
        default_ad_number
    );
    log::info!("Proxied stream will be available at: {proxied_playlist_path}");

    if let Some(ref asset) = test_asset {
        log::info!("Test asset URL: {}, duration: {}s", asset.url, asset.duration);
    }

    let target_ad_duration = test_asset.as_ref()
        .map_or(default_ad_duration, |asset| asset.duration as u64);
    if args.ad_insertion_mode==InsertionMode::Static && default_repeating_cycle < target_ad_duration {
        log::warn!("Ad duration is greater than the repeating cycle. This may cause issues for live streams.");
    }

    let available_slots = AvailableAdSlots::default();
    let available_ads = AvailableAds::default();
    let server_config = ServerConfig::new(
        forward_url,
        interstitials_address,
        playlist_path.to_string(),
        args.ad_insertion_mode,
        target_ad_duration,
        default_repeating_cycle,
        default_ad_number,
        test_asset,
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
