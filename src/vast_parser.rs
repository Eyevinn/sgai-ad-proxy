mod utils;
use utils::{
    get_all_raw_creatives_from_vast, get_all_transcoded_creatives_from_vast,
    get_duration_and_media_urls_from_linear,
};

fn main() {
    let vast = vast4_rs::from_str(include_str!("../test_data/vast4.1.xml")).unwrap();
    let raw_creatives = get_all_raw_creatives_from_vast(&vast);
    let vast = vast4_rs::from_str(include_str!("../test_data/vast4.0_transcoded.xml")).unwrap();
    let transcoded_creatives = get_all_transcoded_creatives_from_vast(&vast);
    let linears = raw_creatives
        .into_iter()
        .chain(transcoded_creatives.into_iter())
        .map(|creative| creative.linear.as_ref().unwrap())
        .collect::<Vec<_>>();
    for linear in linears {
        let (duration, media_urls) = get_duration_and_media_urls_from_linear(&linear);
        println!("Duration: {}", duration);
        println!("Media URLs: {:?}", media_urls);
    }
}
