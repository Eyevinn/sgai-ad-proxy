use actix_web::{HttpRequest, HttpResponseBuilder};
use rustls::{ClientConfig, RootCertStore};
use url::{ParseError, Url};

#[derive(Clone, Debug)]
pub struct UniversalAdId {
    pub scheme: String,
    pub value: String,
}

#[derive(Clone, Debug)]
pub struct Tracking {
    pub event: String,
    pub offset: Option<String>,
    pub urls: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct VideoClicks {
    pub click_trackings: Vec<String>,
    pub click_through: Option<String>,
}

pub fn get_all_creatives_from_vast<'a>(
    vast: &'a vast4_rs::Vast<'a>,
) -> Vec<&'a vast4_rs::Creative<'a>> {
    let ads = &vast.ads;
    ads.iter()
        .flat_map(|ad| {
            ad.in_line
                .iter()
                .flat_map(|in_line| in_line.creatives.creatives.iter().collect::<Vec<_>>())
        })
        .collect::<Vec<_>>()
}

pub fn filter_creatives_by<'a>(
    creatives: Vec<&'a vast4_rs::Creative<'a>>,
    filter: impl Fn(&str) -> bool,
) -> Vec<&'a vast4_rs::Creative<'a>> {
    creatives
        .into_iter()
        // Only return creatives with adId and linear.
        .filter(|creative| creative.ad_id.is_some() && creative.linear.is_some())
        .filter(|creative| {
            let media_urls = get_media_urls_from_linear(creative.linear.as_ref().unwrap());
            // Only return linears with valid media files.
            // This is a simple way to filter out bumpers (which end with '*_2023_P8_mp4').
            !media_urls.is_empty() && filter(media_urls.first().unwrap())
        })
        .collect::<Vec<_>>()
}

pub fn get_all_raw_creatives_from_vast<'a>(
    vast: &'a vast4_rs::Vast<'a>,
) -> Vec<&'a vast4_rs::Creative<'a>> {
    filter_creatives_by(get_all_creatives_from_vast(vast), is_media_segment)
}

pub fn get_all_transcoded_creatives_from_vast<'a>(
    vast: &'a vast4_rs::Vast<'a>,
) -> Vec<&'a vast4_rs::Creative<'a>> {
    filter_creatives_by(
        get_all_creatives_from_vast(vast),
        is_transcoded_media_segment,
    )
}

pub fn get_universal_ad_ids_from_creative(creative: &vast4_rs::Creative) -> Vec<UniversalAdId> {
    creative
        .universal_ad_id
        .iter()
        .map(|id| UniversalAdId {
            scheme: id.id_registry.to_string(),
            value: id.id.to_string(),
        })
        .collect()
}

pub fn get_duration_from_linear(linear: &vast4_rs::Linear) -> f64 {
    linear
        .duration
        .as_ref()
        .map(|duration| std::time::Duration::from(duration.clone()).as_secs_f64())
        .unwrap_or_default()
}

pub fn get_skip_offset_from_linear(linear: &vast4_rs::Linear) -> Option<f64> {
    linear
        .skipoffset
        .as_ref()
        .map(|skipoffset| std::time::Duration::from(skipoffset.clone()).as_secs_f64())
}

pub fn get_media_urls_from_linear(linear: &vast4_rs::Linear) -> Vec<String> {
    linear
        .media_files
        .as_ref()
        .map(|media_files| {
            media_files
                .media_files
                .iter()
                .map(|media_file| media_file.uri.clone().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

pub fn get_tracking_events_from_linear<'a>(linear: &vast4_rs::Linear) -> Vec<Tracking> {
    linear
        .tracking_events
        .as_ref()
        .map(|tracking_events| {
            tracking_events
                .trackings
                .iter()
                .map(|tracking| Tracking {
                    event: tracking.event.to_string(),
                    offset: tracking.offset.as_ref().map(|offset| offset.to_string()),
                    urls: vec![tracking.uri.to_string()],
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn get_video_clicks_from_linear<'a>(linear: &'a vast4_rs::Linear) -> Option<VideoClicks> {
    linear
        .video_clicks
        .as_ref()
        .map(|video_clicks| VideoClicks {
            click_trackings: video_clicks
                .click_trackings
                .iter()
                .map(|click_tracking| click_tracking.uri.to_string())
                .collect(),
            click_through: video_clicks
                .click_through
                .as_ref()
                .map(|click_through| click_through.uri.to_string()),
        })
}

pub fn get_duration_and_media_urls_and_tracking_events_from_linear<'a>(
    linear: &'a vast4_rs::Linear,
) -> (f64, Vec<String>, Vec<Tracking>) {
    (
        get_duration_from_linear(linear),
        get_media_urls_from_linear(linear),
        get_tracking_events_from_linear(linear),
    )
}

pub fn find_program_datetime_tag(
    playlist: &hls_m3u8::MediaPlaylist,
) -> Option<chrono::DateTime<chrono::Local>> {
    playlist
        .segments
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
        })
}

pub fn calculate_expected_program_date_time_list(
    segments: &hls_m3u8::stable_vec::StableVec<hls_m3u8::MediaSegment>,
    first_program_date_time: chrono::DateTime<chrono::Local>,
) -> Vec<(chrono::DateTime<chrono::Local>, std::time::Duration)> {
    let mut current_program_date_time = first_program_date_time;
    let mut accumulated_segment_duration_ms = 0u128;

    segments
        .iter()
        .map(|(_, segment)| {
            let optional_program_date_time = segment
                .program_date_time
                .as_ref()
                .and_then(|program_date_time| {
                    let date_str = program_date_time.date_time.as_ref();
                    parse_date_time(date_str)
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
        .collect()
}

pub fn is_media_segment(path: &str) -> bool {
    path.ends_with(".ts")
        || path.ends_with(".cmf")
        || path.ends_with(".mp4")
        || path.ends_with(".m4s")
        || path.ends_with(".fmp4")
}

pub fn is_hls_playlist(path: &str) -> bool {
    path.ends_with(".m3u8")
}

pub fn is_transcoded_media_segment(path: &str) -> bool {
    // Transcoded media segments typically forms a HLS VoD playlist.
    is_hls_playlist(path)
}

pub fn is_fragmented_mp4_vod_media_playlist(playlist: &hls_m3u8::MediaPlaylist) -> bool {
    playlist.has_end_list
        && playlist.segments.find_first().is_some_and(|segment| {
            segment
                .map
                .as_ref()
                .is_some_and(|map| !map.uri().is_empty())
        })
}

pub fn fixed_offset_to_local(
    date: chrono::DateTime<chrono::FixedOffset>,
) -> chrono::DateTime<chrono::Local> {
    date.with_timezone(&chrono::Local)
}

pub fn parse_date_time(
    date_time: &str,
) -> chrono::ParseResult<chrono::DateTime<chrono::FixedOffset>> {
    let default_date_time_format = "%Y-%m-%dT%H:%M:%S%.3f%z";

    let date_time = chrono::DateTime::parse_from_rfc3339(date_time)
        .or_else(|_| chrono::DateTime::parse_from_rfc2822(date_time))
        .or_else(|_| chrono::DateTime::parse_from_str(date_time, default_date_time_format));

    date_time
}

pub fn date_time_to_string(date_time: &chrono::DateTime<chrono::Local>) -> String {
    date_time.to_rfc3339_opts(chrono::SecondsFormat::Millis, false)
}

pub fn make_program_date_time_tag(
    date_time: &chrono::DateTime<chrono::Local>,
) -> hls_m3u8::tags::ExtXProgramDateTime<'static> {
    hls_m3u8::tags::ExtXProgramDateTime::new(date_time_to_string(date_time))
}

/// Create simple rustls client config from root certificates.
pub fn rustls_config() -> ClientConfig {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .unwrap();

    let root_store = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.to_owned());

    rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth()
}

pub fn base_url(url: &Url) -> Result<Url, ParseError> {
    let mut clone = url.clone();
    match clone.path_segments_mut() {
        Ok(mut path) => {
            path.clear();
        }
        Err(_) => {
            return Err(ParseError::RelativeUrlWithoutBase);
        }
    }

    clone.set_query(None);

    Ok(clone)
}

pub fn copy_headers<T>(res: &awc::ClientResponse<T>, client_resp: &mut HttpResponseBuilder) {
    for (header_name, header_value) in res.headers().iter().filter(|(h, _)| *h != "connection") {
        client_resp.insert_header((header_name.clone(), header_value.clone()));
    }
}

pub fn build_forward_url(req: &HttpRequest, forward_url: &Url) -> Url {
    let mut new_url = forward_url.clone();
    new_url.set_path(req.uri().path());
    new_url.set_query(req.uri().query());
    new_url
}

pub fn get_query_param(req: &HttpRequest, key: &str) -> Option<String> {
    req.uri().query().and_then(|query| {
        url::form_urlencoded::parse(query.as_bytes())
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.to_string())
    })
}

pub fn get_header_value(req: &HttpRequest, key: &str) -> Option<String> {
    req.headers()
        .get(key)
        .and_then(|v| v.to_str().ok().map(|s| s.to_string()))
}
