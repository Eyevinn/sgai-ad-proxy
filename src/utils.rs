use actix_web::{HttpRequest, HttpResponseBuilder};
use rustls::{ClientConfig, RootCertStore};
use url::{ParseError, Url};

pub fn get_all_creatives_from_vast<'a>(
    vast: &'a vast4_rs::Vast<'a>,
) -> Vec<&vast4_rs::Creative<'a>> {
    let ads = &vast.ads;
    ads.iter()
        .flat_map(|ad| {
            ad.in_line
                .iter()
                .flat_map(|in_line| in_line.creatives.creatives.iter().collect::<Vec<_>>())
        })
        .collect::<Vec<_>>()
}

pub fn get_valid_creatives<'a>(
    creatives: Vec<&'a vast4_rs::Creative<'a>>,
) -> Vec<&'a vast4_rs::Creative<'a>> {
    creatives
        .into_iter()
        // Only return creatives with adId and linear.
        .filter(|creative| creative.ad_id.is_some() && creative.linear.is_some())
        .filter(|creative| {
            let media_urls = get_media_urls_from_linear(creative.linear.as_ref().unwrap());
            // Only return linears with mp4 media files.
            // This is a simple way to filter out bumpers (which end with '*_2023_P8_mp4').
            !media_urls.is_empty() && media_urls.first().unwrap().ends_with(".mp4")
        })
        .collect::<Vec<_>>()
}

pub fn get_all_valid_creatives_from_vast<'a>(
    vast: &'a vast4_rs::Vast<'a>,
) -> Vec<&'a vast4_rs::Creative<'a>> {
    get_valid_creatives(get_all_creatives_from_vast(vast))
}

pub fn get_duration_from_linear(linear: &vast4_rs::Linear) -> u64 {
    linear
        .duration
        .as_ref()
        .map(|duration| std::time::Duration::from(duration.clone()).as_secs())
        .unwrap_or_default()
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

pub fn get_duration_and_media_urls_from_linear(linear: &vast4_rs::Linear) -> (u64, Vec<String>) {
    (
        get_duration_from_linear(linear),
        get_media_urls_from_linear(linear),
    )
}

pub fn is_media_segment(path: &str) -> bool {
    path.contains(".ts") || path.contains(".cmf") || path.contains(".mp") || path.contains(".m4s")
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
