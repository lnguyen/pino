//! Parse Anthropic token-usage out of a /v1/messages response and price it.
//! Pure functions, no I/O — safe to unit-test against real captured fixtures.
//! Handles both streaming (SSE) responses and single-JSON bodies. Mirrors
//! src/usage.js, including the regex-on-raw-text approach (handles SSE).

use std::sync::LazyLock;

use regex::Regex;

/// Public list prices, USD per million tokens: (base input, output).
pub fn prices(family: &str) -> (f64, f64) {
    match family {
        "fable" => (10.0, 50.0),
        "mythos" => (10.0, 50.0), // same surface/pricing as Fable 5
        "opus" => (5.0, 25.0),    // Opus 4.5/4.6/4.7/4.8 — NOT deprecated 4.1
        "sonnet" => (3.0, 15.0),
        "haiku" => (1.0, 5.0),
        _ => (5.0, 25.0), // fall back to opus
    }
}

// Cache multipliers applied to the base *input* price.
pub const CACHE_READ: f64 = 0.1; // cache hit
pub const CACHE_WRITE_5M: f64 = 1.25; // ephemeral 5-minute write
pub const CACHE_WRITE_1H: f64 = 2.0; // ephemeral 1-hour write

/// The default ephemeral cache lifetime Claude Code already gets for free. A
/// cache entry survives this long *and* its TTL is refreshed every time it's
/// used — so the relevant question for "did the proxy's 1h bump help?" is
/// whether the gap since the previous request in the session exceeded this.
pub const FIVE_MIN_MS: i64 = 5 * 60 * 1000;

/// Map a model id to a price family. Unknown ids fall back to opus.
pub fn model_family(model: &str) -> &'static str {
    let m = model.to_lowercase();
    if m.contains("fable") {
        "fable"
    } else if m.contains("mythos") {
        "mythos"
    } else if m.contains("opus") {
        "opus"
    } else if m.contains("haiku") {
        "haiku"
    } else if m.contains("sonnet") {
        "sonnet"
    } else {
        "opus"
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    pub input_tokens: i64,
    pub cache_create: i64,
    pub cache_read: i64,
    pub ephem_5m: i64,
    pub ephem_1h: i64,
    pub output_tokens: i64,
}

#[derive(Clone, Debug)]
pub struct Cost {
    pub family: &'static str,
    pub estimate: bool,
    pub cost_actual: f64,
    pub cost_uncached: f64,
    pub saved: f64,
    pub saved_pct: f64,
}

static RE_START: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""usage":\{"input_tokens":(\d+),"cache_creation_input_tokens":(\d+),"cache_read_input_tokens":(\d+)"#).unwrap()
});
static RE_SPLIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""ephemeral_5m_input_tokens":(\d+),"ephemeral_1h_input_tokens":(\d+)"#).unwrap()
});
static RE_OUTPUT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""output_tokens":(\d+)"#).unwrap());
static RE_KNOWN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)opus|haiku|sonnet|fable|mythos").unwrap());

fn cap_i64(c: &regex::Captures, i: usize) -> i64 {
    c.get(i).and_then(|m| m.as_str().parse().ok()).unwrap_or(0)
}

/// Pull a usage object out of a response body. Returns None when no usage is
/// present (e.g. count_tokens, errors). Takes the first input/cache triple and
/// the largest output_tokens seen across the stream.
pub fn parse_usage(text: &str) -> Option<Usage> {
    if text.is_empty() {
        return None;
    }
    let start = RE_START.captures(text)?;
    let input_tokens = cap_i64(&start, 1);
    let cache_create = cap_i64(&start, 2);
    let cache_read = cap_i64(&start, 3);

    let (ephem_5m, ephem_1h) = match RE_SPLIT.captures(text) {
        Some(s) => (cap_i64(&s, 1), cap_i64(&s, 2)),
        None => (0, 0),
    };

    let mut output_tokens = 0i64;
    for c in RE_OUTPUT.captures_iter(text) {
        let v = cap_i64(&c, 1);
        if v > output_tokens {
            output_tokens = v;
        }
    }

    Some(Usage {
        input_tokens,
        cache_create,
        cache_read,
        ephem_5m,
        ephem_1h,
        output_tokens,
    })
}

/// Cost the request two ways: as actually billed (with caching) and as if every
/// cached/created token were fresh input. Returns dollars + savings delta/pct.
pub fn compute_cost(usage: &Usage, model: &str) -> Cost {
    let family = model_family(model);
    // "estimate" means we couldn't recognize the model and fell back.
    let known = RE_KNOWN.is_match(model);
    let (in_price, out_price) = prices(family);
    let m = 1_000_000.0;

    let input_tokens = usage.input_tokens as f64;
    let cache_read = usage.cache_read as f64;
    let cache_create = usage.cache_create as f64;
    let ephem_5m = usage.ephem_5m as f64;
    let ephem_1h = usage.ephem_1h as f64;
    let output_tokens = usage.output_tokens as f64;

    // If the 5m/1h split is missing, treat all creation as 5m.
    let write_5m = if ephem_5m != 0.0 {
        ephem_5m
    } else if ephem_1h != 0.0 {
        0.0
    } else {
        cache_create
    };
    let write_1h = ephem_1h;

    let input_actual = (input_tokens
        + cache_read * CACHE_READ
        + write_5m * CACHE_WRITE_5M
        + write_1h * CACHE_WRITE_1H)
        * (in_price / m);

    let input_uncached = (input_tokens + cache_read + cache_create) * (in_price / m);
    let output_cost = output_tokens * (out_price / m);

    let cost_actual = input_actual + output_cost;
    let cost_uncached = input_uncached + output_cost;
    let saved = cost_uncached - cost_actual;
    let saved_pct = if cost_uncached > 0.0 {
        saved / cost_uncached * 100.0
    } else {
        0.0
    };

    Cost {
        family,
        estimate: !known,
        cost_actual,
        cost_uncached,
        saved,
        saved_pct,
    }
}

/// The proxy's *marginal* contribution: 1h caching vs the 5-minute caching
/// Claude Code already does for free.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Marginal {
    /// Net dollars the 1h bump saved over the 5m default for this request.
    /// Negative when the 1h write premium bought nothing (gap < 5min).
    pub saved: f64,
    /// Extra dollars paid to write at 1h instead of 5m on this request. Always
    /// ≥ 0; only "worth it" if a later read lands in the 5m–1h window.
    pub write_premium: f64,
    /// Whether the gap since the previous request in this session exceeded the
    /// 5m default lifetime — i.e. the default cache would have expired and the
    /// 1h bump is what kept this request's `cache_read` alive.
    pub extended_window: bool,
}

/// Reprice a request against the realistic baseline: Claude Code's default 5m
/// caching, not "no cache at all" (which `compute_cost.saved` assumes).
///
/// `gap_ms` is the time since the previous request in the same session. `None`
/// (first request in a session, or a backfill row with no predecessor) is
/// treated conservatively as *not* extended — we don't claim a save we can't
/// justify.
///
/// Two effects, both priced off the base input rate:
///  * **write premium** — every `ephem_1h` token cost 2.0× instead of the 1.25×
///    a default 5m write would have cost. Paid unconditionally on this request.
///  * **read benefit** — a `cache_read` only beats the 5m baseline when the gap
///    exceeded 5min: the default cache would have expired, so those tokens would
///    have been re-written at 1.25× instead of read at 0.1×. Within 5min the
///    default cache (TTL refreshed on use) would have hit too → zero benefit.
///
/// Per request the sign swings (a turn that only *writes* the prefix looks like
/// a loss; the turn that later *reads* it shows the gain), but the two halves
/// telescope: summed over a session, `saved` is the honest net of the 1h bump.
pub fn compute_marginal(usage: &Usage, model: &str, gap_ms: Option<i64>) -> Marginal {
    let (in_price, _) = prices(model_family(model));
    let per_token = in_price / 1_000_000.0;

    let extended = gap_ms.map(|g| g >= FIVE_MIN_MS).unwrap_or(false);

    let write_premium = (usage.ephem_1h as f64) * (CACHE_WRITE_1H - CACHE_WRITE_5M) * per_token;
    let read_benefit = if extended {
        (usage.cache_read as f64) * (CACHE_WRITE_5M - CACHE_READ) * per_token
    } else {
        0.0
    };

    Marginal {
        saved: read_benefit - write_premium,
        write_premium,
        extended_window: extended,
    }
}
