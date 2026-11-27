//! "What is becoming popular": Google Trends' daily trending searches (the same public feed trendspyg's fast
//! mode reads, no API key) and YouTube's own trending chart.

use crate::llm::http_err;
use anyhow::{bail, Result};
use serde_json::{json, Value};

fn tag<'a>(block: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = block.find(&open)? + open.len();
    let end = block[start..].find(&format!("</{name}>"))? + start;
    Some(block[start..end].trim())
}

fn unescape(s: &str) -> String {
    let s = s.trim_start_matches("<![CDATA[").trim_end_matches("]]>");
    s.replace("&apos;", "'").replace("&#39;", "'").replace("&quot;", "\"").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

/// "5000+" -> 5000, "2M+" -> 2_000_000
fn traffic_num(t: &str) -> u64 {
    let t = t.trim().trim_end_matches('+').replace(',', "");
    let (n, mult) = match t.chars().last() {
        Some('K') | Some('k') => (&t[..t.len() - 1], 1_000.0),
        Some('M') | Some('m') => (&t[..t.len() - 1], 1_000_000.0),
        _ => (t.as_str(), 1.0),
    };
    (n.parse::<f64>().unwrap_or(0.0) * mult) as u64
}

pub fn parse_google_rss(xml: &str) -> Vec<Value> {
    let mut out: Vec<Value> = xml
        .split("<item>")
        .skip(1)
        .filter_map(|item| {
            let title = unescape(tag(item, "title")?);
            let traffic = tag(item, "ht:approx_traffic").map(unescape).unwrap_or_default();
            let news: Vec<Value> = item
                .split("<ht:news_item>")
                .skip(1)
                .filter_map(|n| {
                    Some(json!({"title": unescape(tag(n, "ht:news_item_title")?), "url": unescape(tag(n, "ht:news_item_url").unwrap_or("")),
                                "source": unescape(tag(n, "ht:news_item_source").unwrap_or(""))}))
                })
                .take(3)
                .collect();
            Some(json!({"title": title, "traffic": traffic, "traffic_num": traffic_num(&traffic),
                        "published": tag(item, "pubDate").map(unescape).unwrap_or_default(), "news": news}))
        })
        .collect();
    out.sort_by_key(|v| std::cmp::Reverse(v["traffic_num"].as_u64().unwrap_or(0)));
    out
}

/// Today's rising Google searches in a country (e.g. "US", "GB", "ZA").
pub fn google_trends(geo: &str) -> Result<Vec<Value>> {
    let geo = geo.trim().to_uppercase();
    if geo.len() != 2 || !geo.chars().all(|c| c.is_ascii_alphabetic()) {
        bail!("country must be a 2-letter code like US");
    }
    let xml = ureq::get("https://trends.google.com/trending/rss")
        .query("geo", &geo)
        .set("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64)")
        .call()
        .map_err(http_err)?
        .into_string()?;
    Ok(parse_google_rss(&xml))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_trends_feed() {
        let xml = r#"<rss><channel><title>Daily Search Trends</title>
            <item><title>small topic</title><ht:approx_traffic>500+</ht:approx_traffic></item>
            <item><title>Bears &amp; Swift</title><ht:approx_traffic>2M+</ht:approx_traffic><pubDate>Fri, 27 Nov 2026</pubDate>
              <ht:news_item><ht:news_item_title>Johnson&apos;s plan</ht:news_item_title><ht:news_item_url>https://espn.com/x</ht:news_item_url><ht:news_item_source>ESPN</ht:news_item_source></ht:news_item>
            </item></channel></rss>"#;
        let t = parse_google_rss(xml);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0]["title"], "Bears & Swift"); // sorted by traffic
        assert_eq!(t[0]["traffic_num"], 2_000_000);
        assert_eq!(t[0]["news"][0]["title"], "Johnson's plan");
        assert_eq!(traffic_num("5,000+"), 5000);
    }
}

/// Which of today's trends this channel should jump on, and how.
pub fn opportunities(s: &crate::settings::Settings, google: &[Value], youtube: &[Value], niche: &[Value]) -> Result<Value> {
    let n: Vec<String> = niche.iter().take(20).map(|v| format!("- {} ({} views)", v["title"].as_str().unwrap_or(""), v["views"])).collect();
    let g: Vec<String> = google.iter().take(25).map(|t| format!("- {} ({} searches)", t["title"].as_str().unwrap_or(""), t["traffic"].as_str().unwrap_or(""))).collect();
    let y: Vec<String> = youtube.iter().take(25).map(|v| format!("- {} ({} views)", v["title"].as_str().unwrap_or(""), v["views"])).collect();
    let channel = if s.channel_about.is_empty() { "(unknown: suggest broadly appealing angles)".to_string() } else { s.channel_about.clone() };
    crate::llm::generate_json(s, "research", &format!(
        "You are a YouTube Shorts strategist. Think carefully. Pick the trends this channel can ride RIGHT NOW without going off-brand.\n\
         Channel: {channel}\nRising Google searches today:\n{}\nTrending on YouTube today:\n{}\n\
         Top Shorts this month in the channel's own niche (the strongest signal; model these):\n{}\n\
         EVERY pick must be squarely about the channel's niche: a viewer who subscribed for that niche must instantly see it is for them. \
         Use a general trend only when it connects naturally and obviously to the niche (e.g. a heatwave → cooling costs for a home-energy channel); \
         otherwise ignore it. Never make shorts about games, movies, superheroes, sports or celebrities unless that IS the niche. \
         The niche's own top Shorts are the main source; it is fine if no general trend is used at all. \
         Skip anything tragic, political or that would need copyrighted footage. \
         Return ONLY JSON: {{\"picks\": [{{\"trend\": the trend, \"topic\": a specific short topic with the hook built in, \"why\": one sentence, \
         \"urgency\": \"today\" | \"this week\"}}] (6 picks, best first)}}",
        g.join("\n"), y.join("\n"), n.join("\n")
    ))
}

/// The search query that describes this channel's niche, worked out once by the AI and saved.
pub fn niche_query(s: &mut crate::settings::Settings) -> Result<String> {
    if !s.niche_query.trim().is_empty() {
        return Ok(s.niche_query.trim().to_string());
    }
    let about = if s.channel_about.is_empty() { crate::youtube::my_titles(s).unwrap_or_default().join("\n") } else { s.channel_about.clone() };
    if about.trim().is_empty() {
        bail!("make or upload a few shorts first (or use Suggest for my channel) so the AI knows your niche");
    }
    let v = crate::llm::generate_json(s, "research", &format!(
        "This describes a YouTube channel:\n{about}\nReturn ONLY JSON {{\"query\": a 2-4 word YouTube search query for the channel's niche, e.g. \"space facts\"}}"
    ))?;
    let q = v["query"].as_str().unwrap_or("").trim().to_string();
    if q.is_empty() {
        bail!("could not work out your niche");
    }
    s.niche_query = q.clone();
    s.save()?;
    Ok(q)
}
