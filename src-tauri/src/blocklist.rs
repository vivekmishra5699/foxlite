//! Built-in ad & tracker blocking, compiled by WebKit into content rule lists
//! (`native::blocker_init`). Blocking happens inside the network layer before
//! a request is made — ads/trackers are never fetched, parsed, or executed —
//! which is by far the biggest single win for page RAM, CPU, and load time.
//!
//! The rules are the standard filter lists (EasyList, EasyPrivacy, Peter
//! Lowe's — the same lists uBlock Origin ships), converted at build time by
//! `tools/blocklist/build.mjs` into WebKit's declarative rule JSON and
//! embedded brotli-compressed (~1 MB for ~120k rules). WebKit compiles them
//! once (a few seconds) and caches the compiled lists on disk, so later
//! launches just look them up. Element hiding (`##selector`) is included so
//! blocked ads don't leave empty boxes; scriptlets and other JavaScript-based
//! uBO features have no WebKit equivalent and are not part of this.
//!
//! `DOMAINS` below is a small hand-picked list kept only as a **fallback** for
//! the (unlikely) case that WebKit rejects the generated rules.

use std::io::{BufRead, BufReader, Read};
use std::sync::OnceLock;

/// The generated rule set: line 1 = meta JSON, then one JSON array per chunk.
static RULES_BR: &[u8] = include_bytes!("../blocklist/rules.jsonl.br");

/// Where the rules came from (shown in Settings).
/// Rule categories the user can toggle separately (see `Settings`).
pub const CATEGORY_ADS: &str = "ads";
pub const CATEGORY_PRIVACY: &str = "privacy";
pub const CATEGORY_SECURITY: &str = "security";
pub const CATEGORY_ANNOYANCES: &str = "annoyances";

#[derive(Clone, serde::Serialize, serde::Deserialize, Default)]
#[serde(default)]
pub struct Source {
    pub category: String,
    pub name: String,
    pub url: String,
    pub license: String,
}

#[derive(Clone, serde::Serialize, serde::Deserialize, Default)]
#[serde(default)]
pub struct ChunkMeta {
    pub category: String,
    pub rules: usize,
}

#[derive(Clone, serde::Serialize, serde::Deserialize, Default)]
#[serde(default)]
pub struct CategoryCount {
    pub block: usize,
    pub hide: usize,
    pub rules: usize,
}

#[derive(Clone, serde::Serialize, serde::Deserialize, Default)]
#[serde(default)]
pub struct Meta {
    pub generated: String,
    pub rules: usize,
    pub network: usize,
    pub hide: usize,
    pub exceptions: usize,
    pub categories: std::collections::HashMap<String, CategoryCount>,
    pub chunks: Vec<ChunkMeta>,
    pub sources: Vec<Source>,
}

/// Meta line only (streamed; does not decompress the rules themselves).
pub fn meta() -> &'static Meta {
    static META: OnceLock<Meta> = OnceLock::new();
    META.get_or_init(|| {
        let mut reader = BufReader::new(brotli::Decompressor::new(RULES_BR, 64 * 1024));
        let mut line = String::new();
        let _ = reader.read_line(&mut line);
        serde_json::from_str(&line).unwrap_or_default()
    })
}

/// The rule chunks as JSON strings (full decompression; only needed when
/// WebKit has no cached compile for this rule-set version).
pub fn chunks() -> Vec<String> {
    let mut all = String::with_capacity(16 * 1024 * 1024);
    let _ = brotli::Decompressor::new(RULES_BR, 64 * 1024).read_to_string(&mut all);
    all.lines()
        .skip(1)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Everything `native::blocker_init` needs.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub struct RuleSet {
    /// Identifier prefix for the compiled lists (`<prefix>-<n>`); embeds a hash
    /// of the compressed rules so a rule update compiles fresh while an
    /// unchanged set is loaded from WebKit's cache.
    pub identifier: String,
    /// Category of each chunk, in order (also the chunk count).
    pub categories: Vec<String>,
    pub chunks: fn() -> Vec<String>,
    pub fallback_identifier: String,
    pub fallback_json: fn() -> String,
}

pub fn rule_set() -> RuleSet {
    RuleSet {
        identifier: format!("foxlite-rules-{:016x}", fnv(RULES_BR)),
        categories: meta().chunks.iter().map(|c| c.category.clone()).collect(),
        chunks,
        fallback_identifier: format!("foxlite-fallback-{:016x}", fnv(fallback_json().as_bytes())),
        fallback_json,
    }
}

/// FNV-1a: tiny, stable, no extra dependency.
fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Third-party ad / tracking hosts (registrable domains; subdomains match).
const DOMAINS: &[&str] = &[
    // -- ad exchanges / SSPs / DSPs --
    "doubleclick.net",
    "googlesyndication.com",
    "googleadservices.com",
    "adservice.google.com",
    "adnxs.com",
    "adsrvr.org",
    "rubiconproject.com",
    "pubmatic.com",
    "openx.net",
    "criteo.com",
    "criteo.net",
    "casalemedia.com",
    "indexww.com",
    "33across.com",
    "sharethrough.com",
    "smartadserver.com",
    "teads.tv",
    "yieldmo.com",
    "triplelift.com",
    "bidswitch.net",
    "adform.net",
    "amazon-adsystem.com",
    "media.net",
    "medianet.com",
    "gumgum.com",
    "sovrn.com",
    "lijit.com",
    "spotxchange.com",
    "spotx.tv",
    "undertone.com",
    "conversantmedia.com",
    "contextweb.com",
    "improvedigital.com",
    "adcolony.com",
    "inmobi.com",
    "unityads.unity3d.com",
    "applovin.com",
    "ironsrc.com",
    "vungle.com",
    "chartboost.com",
    "mopub.com",
    "adtelligent.com",
    "onetag-sys.com",
    "rhythmone.com",
    "emxdgt.com",
    "sonobi.com",
    "yieldlab.net",
    "adition.com",
    "advertising.com",
    "adtech.de",
    "zedo.com",
    "adroll.com",
    "adsymptotic.com",
    "serving-sys.com",
    "sizmek.com",
    "flashtalking.com",
    "innovid.com",
    "tremorhub.com",
    "springserve.com",
    "stickyadstv.com",
    "freewheel.tv",
    "fwmrm.net",
    "ads.yahoo.com",
    "analytics.yahoo.com",
    "adtechus.com",
    "bidr.io",
    "eyeota.net",
    "semasio.net",
    "adscale.de",
    "ligadx.com",
    "richaudience.com",
    "seedtag.com",
    "smilewanted.com",
    "adyoulike.com",
    "vidoomy.com",
    "adkernel.com",
    "loopme.me",
    "pubnative.net",
    "smaato.net",
    "bidtellect.com",
    "kargo.com",
    "nativo.com",
    "trafficjunky.net",
    "exoclick.com",
    "exosrv.com",
    "juicyads.com",
    "adcash.com",
    "propellerads.com",
    "propellerclick.com",
    "popads.net",
    "popcash.net",
    "hilltopads.net",
    "onclickads.net",
    "adsterra.com",
    "clickadu.com",
    "adshares.net",
    "revcontent.com",
    "mgid.com",
    "taboola.com",
    "outbrain.com",
    "zemanta.com",
    "content.ad",
    "plista.com",
    "ligatus.com",
    "adblade.com",
    // -- ad verification / viewability --
    "moatads.com",
    "doubleverify.com",
    "adsafeprotected.com",
    "iasds01.com",
    "adlightning.com",
    "confiant-integrations.net",
    // -- analytics / trackers / data brokers --
    "google-analytics.com",
    "analytics.google.com",
    "scorecardresearch.com",
    "quantserve.com",
    "quantcount.com",
    "demdex.net",
    "everesttech.net",
    "omtrdc.net",
    "krxd.net",
    "bluekai.com",
    "exelator.com",
    "agkn.com",
    "mathtag.com",
    "tapad.com",
    "rlcdn.com",
    "liadm.com",
    "id5-sync.com",
    "crwdcntrl.net",
    "lotame.com",
    "dotomi.com",
    "turn.com",
    "amplitude.com",
    "mixpanel.com",
    "segment.io",
    "segment.com",
    "chartbeat.com",
    "chartbeat.net",
    "parsely.com",
    "parse.ly",
    "nr-data.net",
    "bat.bing.com",
    "clarity.ms",
    "hotjar.com",
    "hotjar.io",
    "fullstory.com",
    "mouseflow.com",
    "crazyegg.com",
    "luckyorange.com",
    "inspectlet.com",
    "smartlook.com",
    "logrocket.com",
    "heapanalytics.com",
    "kissmetrics.com",
    "kissmetrics.io",
    "optimizely.com",
    "branch.io",
    "adjust.com",
    "appsflyer.com",
    "kochava.com",
    "singular.net",
    "tynt.com",
    "addthis.com",
    "sharethis.com",
    "po.st",
    "pixel.wp.com",
    "stats.wp.com",
    "mc.yandex.ru",
    "cxense.com",
    "permutive.com",
    "permutive.app",
    "adobedtm.com",
    "tealiumiq.com",
    "ensighten.com",
    "bounceexchange.com",
    "bouncex.net",
    "wunderkind.co",
    "sail-horizon.com",
    "sailthru.com",
    "braze.com",
    "iterable.com",
    "onesignal.com",
    "pushwoosh.com",
    "pushcrew.com",
    "sumo.com",
    "sumome.com",
    "getsitecontrol.com",
    "trustarc.com",
    "yieldify.com",
    "clicktale.net",
    "sessioncam.com",
    "quantummetric.com",
    "glassboxdigital.io",
    "contentsquare.net",
    "trackjs.com",
    "bugsnag.com",
    "connect.facebook.net",
    "pixel.facebook.com",
    "an.facebook.com",
    "ads.linkedin.com",
    "px.ads.linkedin.com",
    "snap.licdn.com",
    "analytics.twitter.com",
    "static.ads-twitter.com",
    "ads.pinterest.com",
    "ct.pinterest.com",
    "analytics.tiktok.com",
    "ads.tiktok.com",
    "business-api.tiktok.com",
    "ads.reddit.com",
    "alb.reddit.com",
    "pixel.quora.com",
    "sp.analytics.yahoo.com",
    "adx.mail.ru",
    "top-fwz1.mail.ru",
    "counter.yadro.ru",
    "hit.gemius.pl",
    "gemius.pl",
    "ivwbox.de",
    "ioam.de",
    "wcfbc.net",
    "webtrekk.net",
    "etracker.com",
    "matomo.cloud",
    "piwik.pro",
    "sitestat.com",
    "nedstat.com",
    "statcounter.com",
    "histats.com",
    "clicky.com",
    "getclicky.com",
    "gosquared.com",
    "woopra.com",
    "track.hubspot.com",
    "js.hs-analytics.net",
    "js.hs-banner.com",
    "pardot.com",
    "marketo.net",
    "mktoresp.com",
    "eloqua.com",
    "en25.com",
    "bizible.com",
    "6sc.co",
    "6sense.com",
    "demandbase.com",
    "clearbit.com",
    "leadfeeder.com",
    "albacross.com",
    "ws.zoominfo.com",
    "intentiq.com",
    "adnium.com",
    "cpmstar.com",
    "adplxmd.com",
    "ad-delivery.net",
    "adsco.re",
    "acuityplatform.com",
    "adgrx.com",
    "adhigh.net",
    "admedo.com",
    "admixer.net",
    "adotmob.com",
    "adpushup.com",
    "adriver.ru",
    "adfox.ru",
    "betweendigital.com",
    "buzzoola.com",
    "relap.io",
    "yieldbird.com",
    "yieldlove.com",
    "yieldoptimizer.com",
    "yieldone.com",
];

/// Escape a hostname for WebKit's content-blocker `url-filter` grammar, ready
/// to be embedded in a JSON string (so the regex `\.` is written `\\.`).
fn escape(host: &str) -> String {
    host.replace('.', "\\\\.")
}

/// Fallback rule list JSON. Each host becomes one third-party block rule
/// matching `scheme://[sub.]host[/:...]`. (WebKit's filter grammar has no `|`
/// alternation, so the host boundary is the `[/:]` class — URLs always carry a
/// path, so a bare host is still matched.)
pub fn fallback_json() -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(DOMAINS.len() * 160);
    out.push('[');
    for (i, host) in DOMAINS.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        // `^https?://([^/]+\.)?host[/:]` — anchored so `evil-doubleclick.net`
        // and `doubleclick.net.evil.com` do not match; `sub.doubleclick.net` does.
        let _ = write!(
            out,
            "{{\"trigger\":{{\"url-filter\":\"^https?://([^/]+\\\\.)?{}[/:]\",\"load-type\":[\"third-party\"]}},\"action\":{{\"type\":\"block\"}}}}",
            escape(host)
        );
    }
    out.push(']');
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn fallback_json_is_valid_and_escaped() {
        let v: serde_json::Value = serde_json::from_str(&super::fallback_json()).unwrap();
        let rules = v.as_array().unwrap();
        assert_eq!(rules.len(), super::DOMAINS.len());
        let first = rules[0]["trigger"]["url-filter"].as_str().unwrap();
        assert_eq!(first, r"^https?://([^/]+\.)?doubleclick\.net[/:]");
    }

    #[test]
    fn generated_rules_parse() {
        let meta = super::meta();
        assert!(meta.rules > 10_000, "meta: {} rules", meta.rules);
        assert!(!meta.sources.is_empty());
        let chunks = super::chunks();
        assert_eq!(chunks.len(), meta.chunks.len());
        assert!(meta.categories.contains_key("ads") && meta.categories.contains_key("security"));
        let mut total = 0;
        for c in &chunks {
            let v: serde_json::Value = serde_json::from_str(c).unwrap();
            let arr = v.as_array().unwrap();
            assert!(arr.len() <= 150_000);
            total += arr.len();
            // Every rule has a trigger with a url-filter and an action type.
            for r in arr.iter().take(200) {
                assert!(r["trigger"]["url-filter"].is_string());
                assert!(r["action"]["type"].is_string());
            }
        }
        // `meta.rules` counts block+hide rules; every chunk additionally
        // carries the global exception rules.
        assert_eq!(total, meta.rules + chunks.len() * meta.exceptions);
        let set = super::rule_set();
        assert!(set.identifier.starts_with("foxlite-rules-"));
        assert_eq!(set.categories.len(), chunks.len());
    }
}
