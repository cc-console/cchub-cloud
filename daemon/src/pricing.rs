//! Claude model pricing (USD per 1M tokens). Hardcoded for M1.
//! Numbers mirror docs from CC Switch's preset (4.x family).

use std::collections::HashMap;
use std::sync::OnceLock;

use cc_console_proto::Usage;

#[derive(Debug, Clone, Copy)]
pub struct Pricing {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_creation: f64,
}

static TABLE: OnceLock<HashMap<&'static str, Pricing>> = OnceLock::new();

fn table() -> &'static HashMap<&'static str, Pricing> {
    TABLE.get_or_init(|| {
        let mut m = HashMap::new();
        // 4.8 / 4.7 / 4.6 (same per-tier rates as the 4.5 family)
        m.insert("claude-opus-4-8",   Pricing { input: 5.0,  output: 25.0, cache_read: 0.50, cache_creation: 6.25 });
        m.insert("claude-opus-4-7",   Pricing { input: 5.0,  output: 25.0, cache_read: 0.50, cache_creation: 6.25 });
        m.insert("claude-opus-4-6",   Pricing { input: 5.0,  output: 25.0, cache_read: 0.50, cache_creation: 6.25 });
        m.insert("claude-sonnet-4-7", Pricing { input: 3.0,  output: 15.0, cache_read: 0.30, cache_creation: 3.75 });
        m.insert("claude-sonnet-4-6", Pricing { input: 3.0,  output: 15.0, cache_read: 0.30, cache_creation: 3.75 });
        m.insert("claude-haiku-4-6",  Pricing { input: 1.0,  output:  5.0, cache_read: 0.10, cache_creation: 1.25 });
        // 4.5
        m.insert("claude-opus-4-5",   Pricing { input: 5.0,  output: 25.0, cache_read: 0.50, cache_creation: 6.25 });
        m.insert("claude-sonnet-4-5", Pricing { input: 3.0,  output: 15.0, cache_read: 0.30, cache_creation: 3.75 });
        m.insert("claude-haiku-4-5",  Pricing { input: 1.0,  output:  5.0, cache_read: 0.10, cache_creation: 1.25 });
        // 4 / 4.1
        m.insert("claude-opus-4",     Pricing { input: 15.0, output: 75.0, cache_read: 1.50, cache_creation: 18.75 });
        m.insert("claude-opus-4-1",   Pricing { input: 15.0, output: 75.0, cache_read: 1.50, cache_creation: 18.75 });
        m.insert("claude-sonnet-4",   Pricing { input: 3.0,  output: 15.0, cache_read: 0.30, cache_creation: 3.75 });
        // 3.5
        m.insert("claude-3-5-sonnet", Pricing { input: 3.0,  output: 15.0, cache_read: 0.30, cache_creation: 3.75 });
        m.insert("claude-3-5-haiku",  Pricing { input: 0.80, output:  4.0, cache_read: 0.08, cache_creation: 1.00 });
        m
    })
}

/// Normalize a raw model id per CC Switch's rules:
/// 1. drop everything before the last `/`
/// 2. drop everything after the first `:`
/// 3. replace `@` with `-`
pub fn normalize(raw: &str) -> String {
    let s = match raw.rsplit_once('/') {
        Some((_, tail)) => tail,
        None => raw,
    };
    let s = match s.split_once(':') {
        Some((head, _)) => head,
        None => s,
    };
    s.replace('@', "-")
}

/// OpenAI GPT-5 family pricing (USD per 1M tokens), used by the codex CLI. cache_creation is
/// n/a for OpenAI (cached prompt tokens are billed at the cheaper cache_read rate, no separate
/// write cost). Applied as a fallback to any `gpt-5*` model so variants like
/// `gpt-5.2-codex-high` are covered without an exhaustive table.
const GPT5: Pricing = Pricing { input: 1.25, output: 10.0, cache_read: 0.125, cache_creation: 0.0 };

// Google Gemini pricing (USD per 1M tokens), used by the gemini CLI. No separate cache-write
// cost; cached prompt tokens bill at the cheaper cache_read rate. Applied as a fallback to any
// `gemini-*` model, split pro vs flash by name (numbers are approximate and corrected at query
// time, so a price fix needs no re-scan).
const GEMINI_PRO: Pricing = Pricing { input: 1.25, output: 10.0, cache_read: 0.125, cache_creation: 0.0 };
const GEMINI_FLASH: Pricing = Pricing { input: 0.30, output: 2.50, cache_read: 0.075, cache_creation: 0.0 };

pub fn cost(model: &str, usage: &Usage) -> f64 {
    let norm = normalize(model);
    let p = table().get(norm.as_str()).copied().or_else(|| {
        if norm.starts_with("gpt-5") {
            Some(GPT5)
        } else if norm.starts_with("gemini") {
            // "pro" → pro rates; everything else gemini (flash/flash-lite) → flash rates.
            Some(if norm.contains("pro") { GEMINI_PRO } else { GEMINI_FLASH })
        } else {
            None
        }
    });
    let Some(p) = p else {
        return 0.0;
    };
    let m = 1_000_000.0_f64;
    (usage.input as f64) * p.input / m
        + (usage.output as f64) * p.output / m
        + (usage.cache_read as f64) * p.cache_read / m
        + (usage.cache_creation as f64) * p.cache_creation / m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization() {
        assert_eq!(normalize("stepfun-ai/step-3.5-flash"), "step-3.5-flash");
        assert_eq!(normalize("moonshotai/kimi-k2-0905:exa"), "kimi-k2-0905");
        assert_eq!(normalize("gpt-5.2-codex@low"), "gpt-5.2-codex-low");
        assert_eq!(normalize("claude-opus-4-8"), "claude-opus-4-8");
    }

    #[test]
    fn opus_4_8_cost() {
        let u = Usage { input: 1_000_000, output: 0, cache_read: 0, cache_creation: 0 };
        assert!((cost("claude-opus-4-8", &u) - 5.0).abs() < 1e-9);
    }
}
