//! The adblock engine and the queries the proxy runs against it.
//!
//! privaxy owns the engine on a dedicated thread and talks to it over a crossbeam channel with a
//! oneshot reply per request. Here the engine sits behind an `RwLock` instead: matching only needs
//! `&self`, so requests match concurrently and cosmetic lookups no longer have to `block_on` a
//! channel round trip from inside the HTML rewriter.

use adblock::engine::Engine;
use adblock::lists::{FilterSet, ParseOptions};
use adblock::request::Request;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// A list's title and its raw text.
pub type FilterList = (String, String);

/// The rule that blocked a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockRule {
    pub text: String,
    /// List title and 1-based line, when the engine kept them.
    pub source: Option<(String, u32)>,
}

impl BlockRule {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            source: None,
        }
    }

    /// "EasyList, line 1234".
    pub fn location(&self) -> Option<String> {
        self.source
            .as_ref()
            .map(|(list, line)| format!("{list}, line {line}"))
    }
}

impl std::fmt::Display for BlockRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.location() {
            Some(location) => write!(f, "{} ({location})", self.text),
            None => f.write_str(&self.text),
        }
    }
}

#[derive(Debug, Default)]
pub struct CosmeticResult {
    pub hidden_selectors: Vec<String>,
    pub injected_script: Option<String>,
}

struct Built {
    engine: Engine,
    /// Indexed by the engine's source index.
    titles: Vec<String>,
}

pub struct FilterEngine {
    built: RwLock<Built>,
    enabled: AtomicBool,
}

impl FilterEngine {
    pub fn empty() -> Self {
        Self {
            built: RwLock::new(Built {
                engine: Engine::new_with_filter_set(FilterSet::new(true)),
                titles: Vec::new(),
            }),
            enabled: AtomicBool::new(true),
        }
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Rebuilds the engine from raw filter list text. Blocking and CPU heavy for large lists —
    /// call it from a blocking task, not the request path.
    pub fn replace(&self, filter_lists: Vec<FilterList>) {
        let mut filter_set = FilterSet::new(true);
        let mut titles = Vec::with_capacity(filter_lists.len());
        for (title, text) in filter_lists {
            let added = filter_set.add_filter_list(text, ParseOptions::default());
            titles.resize(titles.len().max(added.source_index + 1), String::new());
            titles[added.source_index] = title;
        }

        let built = Built {
            engine: Engine::new_with_filter_set(filter_set),
            titles,
        };
        match self.built.write() {
            Ok(mut guard) => *guard = built,
            Err(poisoned) => *poisoned.into_inner() = built,
        }
    }

    /// The matching rule when the request should be blocked.
    pub fn check(
        &self,
        url: &str,
        source_url: &str,
        request_type: &str,
        method: &str,
    ) -> Option<BlockRule> {
        if !self.is_enabled() {
            return None;
        }

        let request = Request::new(url, source_url, request_type, method).ok()?;
        let built = self.built.read().ok()?;
        let result = built.engine.check_network_request(&request);
        if !result.should_block() {
            return None;
        }

        let Some(info) = result.filter else {
            return Some(BlockRule::new("matched"));
        };
        let source = info.source_location.and_then(|at| {
            let title = built.titles.get(at.source_index as usize)?;
            Some((title.clone(), at.line_number + 1))
        });
        Some(BlockRule {
            text: info.raw_line.unwrap_or_else(|| String::from("matched")),
            source,
        })
    }

    /// Hostname-only match, used at CONNECT time where there is no path, referer or content type.
    /// Domain-anchored rules (`||host^`) make up the bulk of every list and match on host alone,
    /// which is what makes blocking work without terminating TLS. The method is unknown here, so
    /// `$method=` rules never match.
    pub fn check_host(&self, host: &str) -> Option<BlockRule> {
        let url = format!("https://{host}/");
        self.check(&url, &url, "other", "")
    }

    pub fn cosmetic(&self, url: &str, ids: &[String], classes: &[String]) -> CosmeticResult {
        if !self.is_enabled() {
            return CosmeticResult::default();
        }

        let Ok(built) = self.built.read() else {
            return CosmeticResult::default();
        };
        let engine = &built.engine;

        let url_resources = engine.url_cosmetic_resources(url);

        let mut hidden_selectors = Vec::new();
        if !url_resources.generichide {
            hidden_selectors.extend(engine.hidden_class_id_selectors(
                classes,
                ids,
                &url_resources.exceptions,
            ));
        }
        hidden_selectors.extend(url_resources.hide_selectors);

        CosmeticResult {
            hidden_selectors,
            injected_script: (!url_resources.injected_script.is_empty())
                .then_some(url_resources.injected_script),
        }
    }
}

impl Default for FilterEngine {
    fn default() -> Self {
        Self::empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(lists: &[(&str, &str)]) -> FilterEngine {
        let engine = FilterEngine::empty();
        engine.replace(
            lists
                .iter()
                .map(|(title, text)| (title.to_string(), text.to_string()))
                .collect(),
        );
        engine
    }

    #[test]
    fn a_block_names_its_list_and_line() {
        let engine = engine(&[
            ("EasyList", "! comment\n||ads.example.com^\n"),
            ("Custom rules", "||tracker.example.net^"),
        ]);
        let rule = engine.check_host("ads.example.com").unwrap();
        assert_eq!(rule.text, "||ads.example.com^");
        assert_eq!(rule.source, Some(("EasyList".into(), 2)));
        let rule = engine.check_host("cdn.tracker.example.net").unwrap();
        assert_eq!(rule.location().as_deref(), Some("Custom rules, line 1"));
        assert_eq!(engine.check_host("example.org"), None);
    }

    #[test]
    fn exceptions_and_observe_only_allow() {
        let engine = engine(&[("Lists", "||ads.example.com^\n@@||ok.ads.example.com^")]);
        assert!(engine.check_host("ads.example.com").is_some());
        assert_eq!(engine.check_host("ok.ads.example.com"), None);
        engine.set_enabled(false);
        assert_eq!(engine.check_host("ads.example.com"), None);
    }

    #[test]
    fn method_rules_need_the_method() {
        let engine = engine(&[("Lists", "||api.example.com^$method=post")]);
        let url = "https://api.example.com/collect";
        assert!(engine.check(url, url, "xmlhttprequest", "POST").is_some());
        assert_eq!(engine.check(url, url, "xmlhttprequest", "GET"), None);
        assert_eq!(engine.check_host("api.example.com"), None);
    }
}
