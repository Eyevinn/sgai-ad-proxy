mod utils;
use utils::{
    base_url, build_forward_url, copy_headers, fixed_offset_to_local,
    get_all_valid_creatives_from_vast, get_duration_and_media_urls_from_linear, get_header_value,
    get_query_param, is_media_segment, parse_date_time, rustls_config,
};

use actix_web::{error, middleware, web, App, Error, HttpRequest, HttpResponse, HttpServer};
use awc::{http::header, Client, Connector};
use clap::{Parser, ValueEnum};
use couch_rs::database::Database;
use couch_rs::document::{DocumentCollection, TypedCouchDocument};
use couch_rs::types::document::DocumentId;
use couch_rs::types::find::FindQuery;
use couch_rs::CouchDocument;
use dashmap::{DashMap, DashSet};
use hls_m3u8::tags::ExtXDateRange;
use hls_m3u8::types::Value;
use hls_m3u8::MediaPlaylist;
use hls_m3u8::MediaSegment;
use json::object;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::TryFrom;
use std::io;
use std::sync::Arc;
use std::time::Duration;
use url::Url;
use uuid::Uuid;

const DEFAULT_AD_DURATION: u64 = 13;
const DEFAULT_AD_NUMBER: i64 = 100;

const STATUS_PREFIX: &str = "/status";
const COMMAND_PREFIX: &str = "/command";
const INTERSTITIAL_PLAYLIST: &str = "interstitials.m3u8";

const SESSION_ID_TEMPLATE: &str = "[template.sessionId]";
const DURATION_TEMPLATE: &str = "[template.duration]";
const POD_NUM_TEMPLATE: &str = "[template.pod]";

const HLS_INTERSTITIAL_ID: &str = "_HLS_interstitial_id";
const HLS_PRIMARY_ID: &str = "_HLS_primary_id";
const HLS_START_OFFSET: &str = "_HLS_start_offset";
const HLS_FOLLOW_ID: &str = "_HLS_follow_id";

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

#[derive(Serialize, Deserialize, CouchDocument, Debug)]
struct TranscodedAd {
    // UniversalAdId from the VAST
    #[serde(skip_serializing_if = "String::is_empty")]
    pub _id: DocumentId,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub _rev: String,

    name: Option<String>,
    duration: u64,
    url: String,
}

#[derive(Clone, Default)]
struct Ad {
    ad_id: Uuid,
    duration: u64,
    url: String,
    requested_at: chrono::DateTime<chrono::Local>,
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
    /// e.g., http://localhost/test/master.m3u8)
    #[clap(verbatim_doc_comment)]
    master_playlist_url: String,

    /// Ad server endpoint (protocol://ip:port/path)
    /// It should be a VAST4.0/4.1 XML compatible endpoint
    #[clap(verbatim_doc_comment)]
    ad_server_endpoint: String,

    /// Ad insertion mode to use:
    /// 1) static  - add interstitial every 30 seconds (100 in total).
    /// 2) dynamic - add interstitial when requested (Live Content only).
    #[clap(short, long, value_enum, verbatim_doc_comment, default_value_t = InsertionMode::Static)]
    ad_insertion_mode: InsertionMode,

    /// Base URL for interstitials (protocol://ip:port)
    /// If not provided, the server will use 'localhost' and the 'listen port' as the base URL
    /// e.g., http://localhost:${LISTEN_PORT}
    #[clap(short, long, verbatim_doc_comment, default_value_t = String::from(""))]
    interstitials_address: String,

    /// CouchDB endpoint (protocol://ip:port)
    /// If provided, the server will connect to the CouchDB instance to fetch transcoded ads
    /// 'COUCHDB_USER' and 'COUCHDB_PASSWORD' environment variables should be set for authentication
    #[clap(long, verbatim_doc_comment, default_value_t = String::from(""))]
    couchdb_endpoint: String,

    /// CouchDB table name
    #[clap(long, verbatim_doc_comment, default_value_t = String::from("transcoded_test_ads"))]
    couchdb_table: String,
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
}

impl ServerConfig {
    fn new(
        forward_url: Url,
        interstitials_address: Url,
        master_playlist_path: String,
        insertion_mode: InsertionMode,
    ) -> Self {
        Self {
            forward_url,
            interstitials_address,
            master_playlist_path,
            insertion_mode,
        }
    }

    fn to_json(&self) -> json::JsonValue {
        object! {
            "forward_url": self.forward_url.as_str(),
            "interstitials_address": self.interstitials_address.as_str(),
            "master_playlist_path": self.master_playlist_path.clone(),
            "insertion_mode": self.insertion_mode.to_str(),
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
    } else if path.contains(".m3u8") {
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

fn make_new_ad_from_transcoded_ad(transcoded_ad: &TranscodedAd) -> Ad {
    let ad_id = Uuid::parse_str(&transcoded_ad._id).unwrap_or_default();
    let duration = transcoded_ad.duration;
    let url = transcoded_ad.url.clone();

    Ad {
        ad_id,
        duration,
        url,
        requested_at: chrono::Local::now(),
    }
}

fn make_new_ad_from_linear(linear: &vast4_rs::Linear) -> Ad {
    let (duration, urls) = get_duration_and_media_urls_from_linear(linear);
    let url = urls.first().unwrap().clone();
    let ad_id = Uuid::new_v4();

    Ad {
        ad_id,
        duration,
        url,
        requested_at: chrono::Local::now(),
    }
}

fn build_ad_response(
    vast: vast4_rs::Vast,
    req_url: Url,
    interstitial_id: &str,
    user_id: &str,
    transcoded_ads: &Vec<TranscodedAd>,
    available_ads: web::Data<AvailableAds>,
) -> String {
    // Get all linears (regular MP4s) from the VAST
    let valid_creatives = get_all_valid_creatives_from_vast(&vast);

    let mut accumulated_duration = 0;
    let assets = valid_creatives
        .iter()
        .map(|creative| {
            let ad_id = creative
                // Use the UniversalAdId if available, otherwise use the ad_id
                .universal_ad_id
                .first()
                .map(|id| id.id.clone())
                .unwrap_or(creative.ad_id.as_deref().unwrap_or("unknown").into());

            log::info!("Processing ad {ad_id}");
            // Check if this ad has been transcoded
            let transcoded_ad = transcoded_ads.iter().find(|ad| ad._id == ad_id);

            // If the ad has been transcoded, use the transcoded version
            let ad = transcoded_ad
                .inspect(|ad| {
                    log::info!("Using transcoded ad {:?}", ad);
                })
                .map(|ad| make_new_ad_from_transcoded_ad(ad))
                // Otherwise, use the original MP4
                .unwrap_or_else(|| make_new_ad_from_linear(creative.linear.as_ref().unwrap()));

            let duration = ad.duration;
            let id = ad.ad_id;
            // For transcoded ads, this points to a m3u8 playlist containing the fMP4s
            // Otherwise, this points to a MP4 file
            let ad_url = ad.url.clone();

            // Save the ad for follow-up requests (this applies to not-transcoded ads)
            available_ads.linears.insert(id, ad);

            let start_offset = accumulated_duration;
            accumulated_duration += duration;

            let mut url = if transcoded_ad.is_some() {
                url::Url::parse(&ad_url).expect("Invalid transcoded ad URL")
            } else {
                req_url.clone()
            };
            url.query_pairs_mut()
                .clear()
                .append_pair(HLS_INTERSTITIAL_ID, interstitial_id)
                .append_pair(HLS_PRIMARY_ID, user_id)
                .append_pair(HLS_START_OFFSET, &start_offset.to_string())
                .append_pair(HLS_FOLLOW_ID, &id.to_string());

            object! {
                URI: url.as_str(),
                DURATION: duration,
            }
        })
        .collect::<Vec<_>>();

    object! {
        ASSETS: assets,
    }
    .pretty(2)
}

fn insert_interstitials(
    m3u8: &mut MediaPlaylist,
    config: &web::Data<ServerConfig>,
    available_slots: web::Data<AvailableAdSlots>,
) {
    let interstitials_address = &config.interstitials_address;
    let ad_insert_mode = &config.insertion_mode;
    let segments = &mut m3u8.segments;

    let mut first_program_date_time = segments
        .iter()
        .find_map(|(_, segment)| segment.program_date_time.as_ref())
        .and_then(|program_date_time| {
            let date_str = program_date_time.date_time.as_ref();
            parse_date_time(date_str)
                // Ignore invalid date times
                .map_err(|_| log::error!("Invalid date time: {}", date_str))
                .ok()
        })
        .map(fixed_offset_to_local)
        .inspect(|program_date_time| {
            log::debug!(
                "First available program_date_time in local timezone: {:?}",
                program_date_time
            );
        });

    let is_vod = m3u8
        .playlist_type
        .is_some_and(|t| t == hls_m3u8::types::PlaylistType::Vod);
    let is_dynamic = *ad_insert_mode == InsertionMode::Dynamic;

    if is_vod && is_dynamic {
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
            first_segment.program_date_time = Some(hls_m3u8::tags::ExtXProgramDateTime::new(
                (*START_TIME).to_rfc3339(),
            ));

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
    let ad_slots_start_date_time = if is_vod {
        // Use the first program_date_time for VOD streams
        first_program_date_time.expect("Missing program_date_time for VOD streams")
    } else {
        // Use the server start time for live streams
        *START_TIME
    };

    // Generate ad slot every half a minute for static mode by default
    let fixed_ad_slots: Vec<AdSlot> = (1..DEFAULT_AD_NUMBER)
        .map(|i| {
            let seconds = i * 30;
            let start_time = ad_slots_start_date_time + chrono::Duration::seconds(seconds);

            AdSlot {
                id: Uuid::new_v4(),
                index: i as u64,
                start_time: start_time,
                duration: DEFAULT_AD_DURATION,
                pod_num: 2,
            }
        })
        .collect();

    let ad_slots: Vec<AdSlot> = if is_vod || !is_dynamic {
        // Save fixed ad slots to available slots
        if available_slots.0.is_empty() {
            for slot in &fixed_ad_slots {
                available_slots.0.insert(slot.clone());
            }
            log::debug!("Saved fixed ad slots for VOD or static mode.");
        }

        fixed_ad_slots
    } else {
        // Get all available ad slots
        available_slots.0.iter().map(|slot| slot.clone()).collect()
    };
    log::trace!("Available slots: {:?}", ad_slots);

    // Find the date time tag for each segment
    // Or calculate the expected date time based on the previous segments
    let mut current_program_date_time = first_program_date_time.unwrap();
    let mut accumulated_segment_duration_ms = 0u128;
    let expected_program_date_time_list: Vec<_> = segments
        .iter()
        .map(|(_, segment)| {
            let optional_program_date_time = segment
                .program_date_time
                .as_ref()
                .and_then(|program_date_time| {
                    let date_str = program_date_time.date_time.as_ref();
                    parse_date_time(date_str)
                        // Ignore invalid date times
                        .map_err(|_| log::error!("Invalid date time: {}", date_str))
                        .ok()
                })
                .map(fixed_offset_to_local);
            let segment_duration = segment.duration.duration();

            if let Some(program_date_time) = optional_program_date_time {
                current_program_date_time = program_date_time;
                accumulated_segment_duration_ms = segment_duration.as_millis();

                (program_date_time, segment_duration)
            } else {
                let expected_date_time = current_program_date_time
                    + chrono::Duration::milliseconds(accumulated_segment_duration_ms as i64);
                accumulated_segment_duration_ms += segment_duration.as_millis();

                (expected_date_time, segment_duration)
            }
        })
        .collect();

    for (index, (program_date_time, duration)) in expected_program_date_time_list.iter().enumerate()
    {
        log::debug!(
            "Segment {index} starts at {program_date_time} and lasts for {:?}",
            duration
        );
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
                    let resume_offset_key = if is_vod {
                        "X-RESUME-OFFSET"
                    } else {
                        "CUSTOM-DROP-OFFSET" // This will be ignored by the player
                    };
                    let date_range = ExtXDateRange::builder()
                        .id(ad_slot_name)
                        .class("com.apple.hls.interstitial")
                        .start_date(
                            expected_date_time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                        )
                        .duration(Duration::from_secs_f32(slot_duration))
                        .insert_client_attribute("X-ASSET-LIST", Value::String(url.into()))
                        .insert_client_attribute("X-SNAP", Value::String("IN,OUT".into()))
                        .insert_client_attribute("X-RESTRICT", Value::String("SKIP,JUMP".into()))
                        .insert_client_attribute(
                            // Set the resume offset to 0 for VOD streams
                            // Or drop offset for Live streams (then it resumes from the live edge)
                            resume_offset_key,
                            Value::Float(hls_m3u8::types::Float::new(0.0)),
                        )
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
    client: web::Data<Client>,
    user_defined_query_params: web::Data<UserDefinedQueryParams>,
    couch_db: web::Data<Option<Database>>,
) -> Result<HttpResponse, Error> {
    let ad_server_url = ad_server_url.clone();
    let req_url = req.full_url();

    let interstitial_id =
        get_query_param(&req, HLS_INTERSTITIAL_ID).unwrap_or_else(|| "default_ad".to_string());
    let user_id =
        get_query_param(&req, HLS_PRIMARY_ID).unwrap_or_else(|| "default_user".to_string());

    if let Some(linear_id) = get_query_param(&req, HLS_FOLLOW_ID) {
        return handle_follow_up_request(&interstitial_id, &linear_id, &user_id, available_ads)
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
        .send()
        .await
        .map_err(error::ErrorInternalServerError)?;

    let payload = res.body().await.map_err(error::ErrorInternalServerError)?;
    let xml = std::str::from_utf8(&payload).unwrap();
    log::debug!("xml \n{:?}", xml);
    let vast: vast4_rs::Vast = vast4_rs::from_str(&xml)
        .inspect_err(|err| {
            log::error!("Error parsing VAST: {:?}", err);
        })
        // Return an empty VAST in case of parsing error
        .unwrap_or_default();

    // Fetch transcoded ads from the database if available
    let transcoded_ads = if let Some(db) = couch_db.as_ref() {
        get_ad_from_db(db).await
    } else {
        Vec::new()
    };

    let response = build_ad_response(
        vast,
        req_url,
        &interstitial_id,
        &user_id,
        &transcoded_ads,
        available_ads,
    );
    log::info!("asset json reply \n{response}");

    Ok(HttpResponse::Ok()
        .content_type(mime::APPLICATION_JSON)
        .body(response))
}

async fn handle_follow_up_request(
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
            log::debug!("m3u8 \n{m3u8}");
        })
        .unwrap();

    Ok(HttpResponse::Ok()
        .content_type("application/vnd.apple.mpegurl")
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
        .map_err(error::ErrorInternalServerError)?;

    let payload = res.body().await.map_err(error::ErrorInternalServerError)?;

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

    Ok(HttpResponse::Ok()
        .content_type("application/vnd.apple.mpegurl")
        .body(payload))
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
    let manifest = std::str::from_utf8(&payload).unwrap();
    let mut m3u8 = MediaPlaylist::try_from(manifest).unwrap();

    insert_interstitials(&mut m3u8, &config, available_slots);
    log::debug!("m3u8 \n{m3u8}");

    Ok(HttpResponse::Ok()
        .content_type("application/vnd.apple.mpegurl")
        .body(m3u8.to_string()))
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

async fn try_connect_to_couchdb(database_url: &str, table: &str) -> Option<Database> {
    if database_url.is_empty() || table.is_empty() {
        return None;
    }
    log::info!("Connecting to CouchDB at {database_url} with table {table}");

    let (user, password) = (
        std::env::var("COUCHDB_USER"),
        std::env::var("COUCHDB_PASSWORD"),
    );

    match (user, password) {
        (Ok(user), Ok(password)) => {
            let couch_rs_client = couch_rs::Client::new(database_url, &user, &password)
                .expect("Failed to login to CouchDB. Please check your credentials.");
            let db = couch_rs_client
                .db(table)
                .await
                .ok()
                .expect("Failed to join table");
            Some(db)
        }
        _ => {
            log::warn!("Missing 'COUCHDB_USER' or 'COUCHDB_PASSWORD' environment variables");
            None
        }
    }
}

async fn get_ad_from_db(db: &Database) -> Vec<TranscodedAd> {
    let find_all = FindQuery::find_all();
    let docs: DocumentCollection<TranscodedAd> = db.find(&find_all).await.unwrap_or_default();
    docs.rows
}

#[actix_web::main]
async fn main() -> io::Result<()> {
    env_logger::init_from_env(env_logger::Env::new().default_filter_or("info"));

    let args = CliArguments::parse();

    let master_playlist_url =
        Url::parse(&args.master_playlist_url).expect("Invalid master playlist URL");

    // Forward URL is the base URL of the master playlist
    let forward_url = base_url(&master_playlist_url).expect("Invalid forward URL");
    let playlist_path = master_playlist_url.path();

    let listen_url = format!("http://{}:{}", &args.listen_addr, &args.listen_port);
    let listen_url = Url::parse(&listen_url).expect("Invalid listen address");

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

    let client_tls_config = Arc::new(rustls_config());
    let available_slots = AvailableAdSlots::default();
    let available_ads = AvailableAds::default();
    let server_config = ServerConfig::new(
        forward_url,
        interstitials_address,
        playlist_path.to_string(),
        args.ad_insertion_mode,
    );
    let user_defined_query_params = UserDefinedQueryParams::default();
    let data_base = try_connect_to_couchdb(&args.couchdb_endpoint, &args.couchdb_table).await;

    HttpServer::new(move || {
        let cors = actix_cors::Cors::permissive();

        // create https client inside `HttpServer::new` closure to have one per worker thread
        let client = Client::builder()
            // Freewheel requires a User-Agent header to make requests
            .add_default_header((header::USER_AGENT, "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0.1 Safari/605.1.15"))
            // a "connector" wraps the stream into an encrypted connection
            .connector(Connector::new().rustls_0_23(Arc::clone(&client_tls_config)))
            .finish();

        App::new()
            .app_data(web::Data::new(client))
            .app_data(web::Data::new(available_slots.clone()))
            .app_data(web::Data::new(available_ads.clone()))
            .app_data(web::Data::new(server_config.clone()))
            .app_data(web::Data::new(ad_server_url.clone()))
            .app_data(web::Data::new(user_defined_query_params.clone()))
            .app_data(web::Data::new(data_base.clone()))
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
